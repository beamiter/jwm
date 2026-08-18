//! Remote desktop viewer/client orchestration.

use super::RemoteResult;
use super::frame::{DecodedFrame, decode_frame};
use super::key::load_key_file;
use super::messages::{ClientHello, ServerHello, encode_input};
use super::protocol::{MessageKind, SessionReader, SessionWriter, client_handshake};
use super::x11_input::InputEvent;
use super::x11_keymap::fingerprint_display;
use super::x11_viewer::{Viewer, ViewerEvent};
use std::io;
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_IDLE_TIMEOUT: Duration = Duration::from_secs(8);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub address: String,
    pub key_file: PathBuf,
    pub display: Option<String>,
    pub view_only: bool,
    pub grab_input: bool,
}

pub fn run_client(options: ClientOptions) -> RemoteResult<()> {
    let key = load_key_file(&options.key_file)?;
    let request_input = !options.view_only;
    let keymap_fingerprint = if request_input {
        fingerprint_display(options.display.as_deref())?
    } else {
        [0; 32]
    };
    let mut stream = TcpStream::connect(&options.address)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let session_keys = client_handshake(&mut stream, &key, rand::random())?;

    stream.set_read_timeout(Some(FIRST_FRAME_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let reader_stream = stream.try_clone()?;
    let (receive_key, send_key) = session_keys.into_client();
    let mut reader = SessionReader::new(reader_stream, receive_key);
    let mut writer = SessionWriter::new(stream, send_key);

    writer.write_message(
        MessageKind::Hello,
        &ClientHello {
            request_input,
            keymap_fingerprint,
        }
        .encode(),
    )?;
    writer.flush()?;
    let (kind, payload) = reader.read_message()?;
    if kind != MessageKind::HelloAck {
        return Err(invalid_data("host did not acknowledge remote session negotiation").into());
    }
    let hello = ServerHello::decode(&payload)?;
    if request_input && !hello.pointer_enabled {
        eprintln!("jwm-remote: host accepted a view-only session; input will not be forwarded");
    } else if request_input && !hello.keyboard_enabled {
        eprintln!("jwm-remote: X11 keymaps differ; pointer works but keyboard is disabled");
    }

    let (kind, payload) = reader.read_message()?;
    if kind != MessageKind::Frame {
        return Err(invalid_data("host did not send an initial video frame").into());
    }
    let first_frame = decode_frame(&payload)?;
    reader
        .get_ref()
        .set_read_timeout(Some(FRAME_IDLE_TIMEOUT))?;
    let initial_width = u16::try_from(first_frame.image.width())
        .map_err(|_| invalid_data("initial frame is wider than an X11 window"))?;
    let initial_height = u16::try_from(first_frame.image.height())
        .map_err(|_| invalid_data("initial frame is taller than an X11 window"))?;
    let last_sequence = first_frame.sequence;
    let mut viewer = Viewer::connect(
        options.display.as_deref(),
        initial_width,
        initial_height,
        hello.pointer_enabled || hello.keyboard_enabled,
        hello.keyboard_enabled,
        hello.keyboard_enabled.then_some(keymap_fingerprint),
        options.grab_input && (hello.pointer_enabled || hello.keyboard_enabled),
    )?;
    viewer.draw(first_frame)?;

    let state = Arc::new(ReceiveState::new());
    let receive_state = Arc::clone(&state);
    let control = writer.get_ref().try_clone()?;
    let receiver = thread::Builder::new()
        .name("jwm-remote-video".into())
        .spawn(move || receive_frames(reader, receive_state, last_sequence))?;

    let pointer_enabled = hello.pointer_enabled && !options.view_only;
    let keyboard_enabled = hello.keyboard_enabled && !options.view_only;
    let loop_result = viewer_loop(
        &mut viewer,
        &mut writer,
        &state,
        pointer_enabled,
        keyboard_enabled,
    );
    state.stopping.store(true, Ordering::Release);
    if pointer_enabled || keyboard_enabled {
        let (kind, payload) = encode_input(InputEvent::ReleaseAll);
        let _ = writer.write_message(kind, &payload);
    }
    let _ = writer.write_message(MessageKind::Close, &[]);
    let _ = writer.flush();
    let _ = control.shutdown(Shutdown::Both);

    match receiver.join() {
        Ok(()) => {}
        Err(_) if loop_result.is_ok() => {
            return Err(io::Error::other("remote video thread panicked").into());
        }
        Err(_) => {}
    }
    loop_result
}

fn viewer_loop(
    viewer: &mut Viewer,
    writer: &mut SessionWriter<TcpStream>,
    state: &ReceiveState,
    pointer_enabled: bool,
    keyboard_enabled: bool,
) -> RemoteResult<()> {
    let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
    loop {
        let mut close = false;
        let events = viewer.poll_events()?;
        let mut wrote_input = false;
        for event in events {
            if event == ViewerEvent::Close {
                close = true;
                continue;
            }
            let input = match event {
                ViewerEvent::Pointer { x, y } if pointer_enabled => InputEvent::Pointer { x, y },
                ViewerEvent::Key { keycode, pressed } if keyboard_enabled => {
                    InputEvent::Key { keycode, pressed }
                }
                ViewerEvent::Button { button, pressed } if pointer_enabled => {
                    InputEvent::Button { button, pressed }
                }
                ViewerEvent::ReleaseAll if pointer_enabled || keyboard_enabled => {
                    InputEvent::ReleaseAll
                }
                ViewerEvent::Close => unreachable!(),
                _ => continue,
            };
            let (kind, payload) = encode_input(input);
            writer.write_message(kind, &payload)?;
            wrote_input = true;
        }
        if Instant::now() >= next_heartbeat {
            writer.write_message(MessageKind::Heartbeat, &[])?;
            wrote_input = true;
            next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
        }
        if wrote_input {
            writer.flush()?;
        }
        if close {
            return Ok(());
        }

        if let Some(frame) = take_latest(state) {
            viewer.draw(frame)?;
        }

        if !state.alive.load(Ordering::Acquire) && state.latest.lock().unwrap().is_none() {
            if state.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            let message = state
                .error
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| "host closed the remote session".to_string());
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, message).into());
        }
        thread::sleep(Duration::from_millis(4));
    }
}

struct ReceiveState {
    latest: Mutex<Option<DecodedFrame>>,
    error: Mutex<Option<String>>,
    alive: AtomicBool,
    stopping: AtomicBool,
}

impl ReceiveState {
    fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            error: Mutex::new(None),
            alive: AtomicBool::new(true),
            stopping: AtomicBool::new(false),
        }
    }
}

fn take_latest(state: &ReceiveState) -> Option<DecodedFrame> {
    state.latest.lock().unwrap().take()
}

fn receive_frames(
    mut reader: SessionReader<TcpStream>,
    state: Arc<ReceiveState>,
    mut last_sequence: u64,
) {
    let result: RemoteResult<()> = (|| loop {
        let (kind, payload) = reader.read_message()?;
        match kind {
            MessageKind::Frame => {
                let frame = decode_frame(&payload)?;
                let expected = last_sequence.wrapping_add(1);
                if frame.sequence != expected {
                    return Err(invalid_data(format!(
                        "video frame sequence mismatch: expected {expected}, got {}",
                        frame.sequence
                    ))
                    .into());
                }
                last_sequence = frame.sequence;
                *state.latest.lock().unwrap() = Some(frame);
            }
            MessageKind::Close => break Ok(()),
            MessageKind::Hello
            | MessageKind::HelloAck
            | MessageKind::Pointer
            | MessageKind::Key
            | MessageKind::Button
            | MessageKind::ReleaseAll => {
                return Err(invalid_data(format!("unexpected host message: {kind:?}")).into());
            }
            MessageKind::Heartbeat => {
                return Err(invalid_data("host sent an unexpected heartbeat").into());
            }
        }
    })();

    if let Err(error) = result
        && !state.stopping.load(Ordering::Acquire)
    {
        *state.error.lock().unwrap() = Some(error.to_string());
    }
    state.alive.store(false, Ordering::Release);
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
