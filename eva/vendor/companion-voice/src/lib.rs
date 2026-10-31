//! Explicit voice timing and dialogue-reference extensions. No audio or provider work.
use companion_client::Framed;
use companion_protocol::{Envelope, Reply, Request, VERSION};
use serde::{Deserialize, Serialize};
use std::{io, os::unix::net::UnixStream, path::Path};

pub mod dialogue;

pub const CLOCK: &str = "linux_boottime_ns_v1";
pub const MAX_AGE_NS: u64 = 250_000_000;
pub const REFRESH_NS: u64 = 80_000_000;

pub fn boottime_ns() -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
        return None;
    }
    let seconds = u64::try_from(time.tv_sec).ok()?;
    let nanos = u64::try_from(time.tv_nsec)
        .ok()
        .filter(|n| *n < 1_000_000_000)?;
    seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}

pub fn fresh(observed: u64, now: u64) -> bool {
    observed > 0
        && now
            .checked_sub(observed)
            .is_some_and(|age| age <= MAX_AGE_NS)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audio {
    pub turn_id: String,
    pub state: companion_protocol::AudioState,
    pub generated_samples: u64,
    pub played_samples: Option<u64>,
    pub interrupted: bool,
    pub observed_at_ns: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Body {
    #[serde(rename = "eva_hello_clocked")]
    Hello { clock: String },
    #[serde(rename = "eva_hello_dialogue")]
    HelloDialogue {
        clock: String,
        dialogue_version: u16,
    },
    #[serde(rename = "audio_clocked")]
    Audio(Audio),
    #[serde(rename = "voice_dialogue")]
    Dialogue(dialogue::Record),
}

#[derive(Clone, Debug)]
pub enum Extension {
    Hello,
    HelloDialogue,
    Audio(Audio),
    Dialogue(dialogue::Record),
}

/// The canonical decoder still validates the envelope, role identity and all
/// ordinary audio fields. Only explicitly negotiated extension bodies are adapted.
pub fn decode(bytes: &[u8]) -> Result<(Envelope, Option<Extension>), String> {
    if bytes.len() > companion_protocol::MAX_FRAME {
        return Err("frame_limit".into());
    }
    let mut value = companion_protocol::strict_json::parse(bytes).map_err(|_| "invalid_json")?;
    let kind = value["body"]["type"].as_str();
    if !matches!(
        kind,
        Some("eva_hello_clocked" | "eva_hello_dialogue" | "audio_clocked" | "voice_dialogue")
    ) {
        return companion_protocol::decode(bytes).map(|envelope| (envelope, None));
    }
    let body: Body =
        serde_json::from_value(value["body"].clone()).map_err(|_| "invalid_voice_extension")?;
    let (legacy, extension) = match body {
        Body::Hello { clock } if clock == CLOCK => (Request::EvaHello, Extension::Hello),
        Body::Hello { .. } => return Err("unsupported_voice_clock".into()),
        Body::HelloDialogue {
            clock,
            dialogue_version,
        } if clock == CLOCK && dialogue_version == dialogue::VERSION => {
            (Request::EvaHello, Extension::HelloDialogue)
        }
        Body::HelloDialogue { .. } => return Err("unsupported_voice_dialogue".into()),
        Body::Audio(audio) => (
            Request::Audio {
                turn_id: audio.turn_id.clone(),
                state: audio.state,
                generated_samples: audio.generated_samples,
                played_samples: audio.played_samples,
                interrupted: audio.interrupted,
            },
            Extension::Audio(audio),
        ),
        Body::Dialogue(record) if record.valid() => (
            Request::Audio {
                turn_id: record.turn_id.clone(),
                state: companion_protocol::AudioState::Thinking,
                generated_samples: 0,
                played_samples: None,
                interrupted: false,
            },
            Extension::Dialogue(record),
        ),
        Body::Dialogue(_) => return Err("invalid_voice_dialogue".into()),
    };
    value["body"] = serde_json::to_value(legacy).map_err(|_| "voice_encoding")?;
    companion_protocol::decode(&serde_json::to_vec(&value).map_err(|_| "voice_encoding")?)
        .map(|envelope| (envelope, Some(extension)))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accepted {
    #[serde(rename = "type")]
    kind: AcceptedKind,
    pub boot_id: String,
    pub companion_id: String,
    pub session_id: String,
    pub clock: String,
    pub max_age_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialogue_version: Option<u16>,
}
#[derive(Debug, Serialize, Deserialize)]
enum AcceptedKind {
    #[serde(rename = "eva_clocked")]
    Clocked,
}
impl Accepted {
    pub fn new(boot_id: String, companion_id: String, session_id: String) -> Self {
        Self {
            kind: AcceptedKind::Clocked,
            boot_id,
            companion_id,
            session_id,
            clock: CLOCK.into(),
            max_age_ns: MAX_AGE_NS,
            dialogue_version: None,
        }
    }
    pub fn with_dialogue(mut self) -> Self {
        self.dialogue_version = Some(dialogue::VERSION);
        self
    }
}

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn connect_checked(path: &Path) -> io::Result<UnixStream> {
    use std::os::unix::{
        fs::{FileTypeExt, MetadataExt},
        io::AsRawFd,
    };
    let parent = path.parent().ok_or_else(|| invalid("socket_parent"))?;
    let directory = std::fs::symlink_metadata(parent)?;
    if !directory.is_dir()
        || directory.file_type().is_symlink()
        || directory.uid() != unsafe { libc::geteuid() }
        || directory.mode() & 0o077 != 0
    {
        return Err(invalid("unsafe_socket_directory"));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(invalid("unsafe_socket"));
    }
    let stream = UnixStream::connect(path)?;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut peer as *mut _ as *mut _,
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if peer.uid != unsafe { libc::geteuid() } {
        return Err(invalid("wrong_server_uid"));
    }
    Ok(stream)
}

/// One exclusive Eva connection. Legacy peers cannot accidentally negotiate a
/// timestamp guarantee; no fallback to unclocked observations is performed.
pub struct Client {
    io: Framed,
    source: String,
    session: String,
    boot: Option<String>,
    sequence: u64,
    dialogue: bool,
}
impl Client {
    pub fn connect(path: &Path, session: String, source: String) -> io::Result<Self> {
        Self::connect_with_dialogue(path, session, source, false)
    }
    pub fn connect_dialogue(path: &Path, session: String, source: String) -> io::Result<Self> {
        Self::connect_with_dialogue(path, session, source, true)
    }
    fn connect_with_dialogue(
        path: &Path,
        session: String,
        source: String,
        dialogue: bool,
    ) -> io::Result<Self> {
        if !companion_protocol::identifier(&source) || !companion_protocol::identifier(&session) {
            return Err(invalid("invalid_voice_identity"));
        }
        let mut client = Self {
            io: Framed::new(connect_checked(path)?)?,
            source,
            session,
            boot: None,
            sequence: 0,
            dialogue,
        };
        client.send_body(
            serde_json::to_value(if dialogue {
                Body::HelloDialogue {
                    clock: CLOCK.into(),
                    dialogue_version: dialogue::VERSION,
                }
            } else {
                Body::Hello {
                    clock: CLOCK.into(),
                }
            })
            .map_err(|_| invalid("encode"))?,
        )?;
        Ok(client)
    }
    fn send_body(&mut self, body: serde_json::Value) -> io::Result<()> {
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("sequence_exhausted"))?;
        let value = serde_json::json!({"protocol_version":VERSION,
            "event_id":format!("{}-1-{sequence}",self.source), "source_id":self.source,
            "source_seq":sequence, "session_id":self.session,
            "boot_id":self.boot.as_deref().unwrap_or(""), "body":body});
        self.io.queue(&value)?;
        self.sequence = sequence;
        self.io.flush()
    }
    pub fn send(&mut self, request: Request) -> io::Result<()> {
        if self.boot.is_none() || matches!(request, Request::Audio { .. }) {
            return Err(invalid("clocked_voice_handshake_or_audio_required"));
        }
        self.send_body(serde_json::to_value(request).map_err(|_| invalid("encode"))?)
    }
    /// An expired local observation is discarded without disconnecting a
    /// healthy transport. A fresh observation may resume the same turn.
    pub fn send_audio(&mut self, audio: Audio) -> io::Result<bool> {
        if self.boot.is_none() {
            return Err(invalid("clocked_voice_handshake_required"));
        }
        if !boottime_ns().is_some_and(|now| fresh(audio.observed_at_ns, now)) {
            return Ok(false);
        }
        self.send_body(serde_json::to_value(Body::Audio(audio)).map_err(|_| invalid("encode"))?)?;
        Ok(true)
    }
    pub fn send_dialogue(&mut self, record: dialogue::Record) -> io::Result<()> {
        if self.boot.is_none() || !self.dialogue || !record.valid() {
            return Err(invalid("voice_dialogue_not_negotiated_or_invalid"));
        }
        self.send_body(serde_json::to_value(Body::Dialogue(record)).map_err(|_| invalid("encode"))?)
    }
    pub fn poll(&mut self) -> io::Result<Vec<Reply>> {
        self.io.flush()?;
        let mut replies = Vec::new();
        for bytes in self.io.read()? {
            let value = companion_protocol::strict_json::parse(&bytes)
                .map_err(|_| invalid("invalid_reply"))?;
            if value["type"] == "eva_clocked" {
                let accepted: Accepted = serde_json::from_value(value)
                    .map_err(|_| invalid("invalid_voice_acceptance"))?;
                if self.boot.is_some()
                    || accepted.session_id != self.session
                    || accepted.clock != CLOCK
                    || accepted.max_age_ns != MAX_AGE_NS
                    || accepted.dialogue_version != self.dialogue.then_some(dialogue::VERSION)
                    || !companion_protocol::identifier(&accepted.boot_id)
                    || !companion_protocol::identifier(&accepted.companion_id)
                {
                    return Err(invalid("incompatible_voice_clock"));
                }
                self.boot = Some(accepted.boot_id.clone());
                replies.push(Reply::Accepted {
                    boot_id: accepted.boot_id,
                    companion_id: accepted.companion_id,
                    session_id: accepted.session_id,
                });
            } else {
                if matches!(value["type"].as_str(), Some("accepted" | "host_accepted")) {
                    return Err(invalid("clocked_voice_handshake_required"));
                }
                let reply: Reply =
                    serde_json::from_value(value).map_err(|_| invalid("invalid_reply"))?;
                replies.push(reply);
            }
        }
        Ok(replies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_acceptance_cannot_negotiate_clocked_audio() {
        use std::{
            io::{Read, Write},
            os::unix::fs::{DirBuilderExt, PermissionsExt},
            os::unix::net::UnixListener,
            time::{Duration, Instant},
        };
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "voice-wire-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let path = root.join("voice.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut size = [0; 4];
            peer.read_exact(&mut size).unwrap();
            let mut input = vec![0; u32::from_be_bytes(size) as usize];
            peer.read_exact(&mut input).unwrap();
            let input: serde_json::Value = serde_json::from_slice(&input).unwrap();
            assert_eq!(input["body"]["type"], "eva_hello_clocked");
            let reply = serde_json::to_vec(&Reply::Accepted {
                boot_id: "boot".into(),
                companion_id: "eva".into(),
                session_id: "s".into(),
            })
            .unwrap();
            peer.write_all(&(reply.len() as u32).to_be_bytes()).unwrap();
            peer.write_all(&reply).unwrap();
        });
        let mut client = Client::connect(&path, "s".into(), "source".into()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match client.poll() {
                Err(error) => {
                    assert_eq!(error.to_string(), "clocked_voice_handshake_required");
                    break;
                }
                Ok(replies) => {
                    assert!(replies.is_empty());
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        assert!(client
            .send(Request::VoiceAcquire {
                session_id: "v".into()
            })
            .is_err());
        worker.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn clock_only_acceptance_cannot_enable_dialogue_records() {
        use std::{
            io::{Read, Write},
            os::unix::{fs::PermissionsExt, net::UnixListener},
            time::{Duration, Instant},
        };
        let root = std::env::temp_dir().join(format!(
            "voice-dialogue-wire-{}-{}",
            std::process::id(),
            boottime_ns().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("voice.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut size = [0; 4];
            peer.read_exact(&mut size).unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(size) as usize];
            peer.read_exact(&mut bytes).unwrap();
            let hello: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(hello["body"]["type"], "eva_hello_dialogue");
            let reply = serde_json::to_vec(&Accepted::new("boot".into(), "eva".into(), "s".into()))
                .unwrap();
            peer.write_all(&(reply.len() as u32).to_be_bytes()).unwrap();
            peer.write_all(&reply).unwrap();
        });
        let mut client = Client::connect_dialogue(&path, "s".into(), "source".into()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match client.poll() {
                Err(error) => {
                    assert_eq!(error.to_string(), "incompatible_voice_clock");
                    break;
                }
                Ok(replies) => {
                    assert!(replies.is_empty());
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        assert!(client
            .send_dialogue(dialogue::Record {
                voice_session_id: "a".into(),
                turn_id: "t".into(),
                observed_at_ns: 1,
                event: dialogue::Event::InputCommitted {
                    item_id: "i".into()
                }
            })
            .is_err());
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn negotiated_acceptance_round_trips_its_explicit_tag() {
        let value =
            serde_json::to_value(Accepted::new("boot".into(), "eva".into(), "s".into())).unwrap();
        assert_eq!(value["type"], "eva_clocked");
        let _: Accepted = serde_json::from_value(value).unwrap();
    }
    #[test]
    fn age_includes_sleep_and_rejects_missing_future_and_overflow() {
        assert!(fresh(1, MAX_AGE_NS + 1));
        assert!(!fresh(1, MAX_AGE_NS + 2));
        assert!(!fresh(0, 1));
        assert!(!fresh(10, 9));
        assert!(!fresh(1, u64::MAX));
        assert!(boottime_ns().is_some_and(|now| now > 0));
    }
    fn envelope(body: &str) -> Vec<u8> {
        format!(r#"{{"protocol_version":1,"event_id":"e","source_id":"v","source_seq":1,"boot_id":"b","session_id":"s","body":{body}}}"#).into_bytes()
    }
    #[test]
    fn canonical_header_and_audio_validation_still_apply() {
        let valid = r#"{"type":"audio_clocked","turn_id":"t","state":"speaking","generated_samples":10,"played_samples":5,"interrupted":false,"observed_at_ns":1}"#;
        assert!(matches!(
            decode(&envelope(valid)).unwrap().1,
            Some(Extension::Audio(_))
        ));
        assert!(decode(&envelope(
            &valid.replace("\"played_samples\":5", "\"played_samples\":11")
        ))
        .is_err());
        assert!(decode(&envelope(&valid.replace(
            "\"observed_at_ns\":1",
            "\"observed_at_ns\":1,\"observed_at_ns\":2"
        )))
        .is_err());
        assert!(decode(&envelope(&valid.replace(
            "\"observed_at_ns\":1",
            "\"observed_at_ns\":1,\"secret\":true"
        )))
        .is_err());
        assert!(decode(
            &envelope(valid)
                .iter()
                .copied()
                .chain(b" ".repeat(companion_protocol::MAX_FRAME))
                .collect::<Vec<_>>()
        )
        .is_err());
    }
    #[test]
    fn dialogue_requires_explicit_version_and_canonical_envelope() {
        let hello =
            r#"{"type":"eva_hello_dialogue","clock":"linux_boottime_ns_v1","dialogue_version":2}"#;
        assert!(matches!(
            decode(&envelope(hello)).unwrap().1,
            Some(Extension::HelloDialogue)
        ));
        assert!(decode(&envelope(&hello.replace("version\":2", "version\":3"))).is_err());
        let body = r#"{"type":"voice_dialogue","voice_session_id":"activation","turn_id":"turn","observed_at_ns":1,"event":{"kind":"input_committed","item_id":"input"}}"#;
        assert!(matches!(
            decode(&envelope(body)).unwrap().1,
            Some(Extension::Dialogue(_))
        ));
        assert!(decode(&envelope(&body.replace(
            "\"item_id\":\"input\"",
            "\"item_id\":\"input\",\"transcript\":\"private\""
        )))
        .is_err());
        let bad = envelope(body);
        assert!(decode(
            &String::from_utf8(bad)
                .unwrap()
                .replace("\"source_seq\":1", "\"source_seq\":0")
                .into_bytes()
        )
        .is_err());
    }
    #[test]
    fn capability_is_explicit_and_legacy_remains_unclocked() {
        assert!(matches!(
            decode(&envelope(&format!(
                r#"{{"type":"eva_hello_clocked","clock":"{CLOCK}"}}"#
            )))
            .unwrap()
            .1,
            Some(Extension::Hello)
        ));
        assert!(decode(&envelope(
            r#"{"type":"eva_hello_clocked","clock":"wall_time"}"#
        ))
        .is_err());
        assert!(decode(&envelope(r#"{"type":"eva_hello"}"#))
            .unwrap()
            .1
            .is_none());
    }
}
