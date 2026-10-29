use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

macro_rules! section {
    ($name:ident { $($field:ident : $ty:ty = $default:expr),* $(,)? }) => {
        #[derive(Clone, Debug, Deserialize, Serialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),* }
        impl Default for $name { fn default() -> Self { Self { $($field: $default),* } } }
    };
}
section!(Service {
    enabled: bool = true,
    instance_scope: String = "graphical_session".into(),
    watchdog_seconds: u64 = 5
});
section!(Interaction {
    trigger: String = "hotkey".into(),
    mode: String = "toggle".into(),
    follow_up_ms: u64 = 5000,
    first_speech_timeout_ms: u64 = 8000,
    max_utterance_ms: u64 = 20000,
    max_session_ms: u64 = 300000,
    half_duplex: bool = true,
    escape_cancel: bool = true,
    feedback: String = "brief".into(),
    local_start_detection: bool = true,
    start_candidate_grace_ms: u64 = 1500
});
section!(Audio {
    input_device: String = "default".into(),
    realtime_rate_hz: u32 = 24000,
    channels: u16 = 1,
    sample_format: String = "s16le".into(),
    chunk_ms: u64 = 20,
    connect_buffer_ms: u64 = 2000,
    playback_tail_ms: u64 = 200
});
section!(Openai {
    model: String = "gpt-realtime-2.1-mini".into(), transport: String = "websocket".into(),
    api_key_source: String = "secret_service".into(), api_key_file: Option<PathBuf> = None,
    voice: String = "marin".into(), input_transcription: bool = false, connect_timeout_ms: u64 = 8000,
    plan_deadline_ms: u64 = 12000, plan_max_output_tokens: u32 = 2048, feedback_max_output_tokens: u32 = 256
});
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TurnDetection {
    #[serde(rename = "type")]
    pub kind: String,
    pub threshold: f64,
    pub prefix_padding_ms: u64,
    pub silence_duration_ms: u64,
    pub create_response: bool,
    pub interrupt_response: bool,
}
impl Default for TurnDetection {
    fn default() -> Self {
        Self {
            kind: "server_vad".into(),
            threshold: 0.5,
            prefix_padding_ms: 300,
            silence_duration_ms: 600,
            create_response: false,
            interrupt_response: false,
        }
    }
}
section!(Execution {
    max_actions_per_turn: usize = 8,
    max_planning_rounds: u8 = 3,
    max_schema_repair_attempts: u8 = 1,
    require_guarded_execution: bool = true,
    query_retries: u8 = 1,
    stop_on_error: bool = true,
    confirmation_timeout_ms: u64 = 15000
});
section!(Undo {
    enabled: bool = true,
    max_plans: usize = 20,
    max_age_seconds: u64 = 600
});
section!(Privacy {
    save_raw_audio: bool = false,
    persist_transcripts: bool = false,
    include_clipboard_in_context: bool = false
});
section!(Budget {
    enabled: bool = true,
    daily_limit_usd: f64 = 1.0,
    timezone: String = "local".into()
});
section!(Pricing {
    version: String = "openai-2026-10-29".into(),
    model: String = "gpt-realtime-2.1-mini".into(),
    input_text: f64 = 0.60,
    input_audio: f64 = 10.0,
    cached_text: f64 = 0.06,
    cached_audio: f64 = 0.30,
    output_text: f64 = 2.40,
    output_audio: f64 = 20.0,
    transcription_per_minute: f64 = 0.0045
});
section!(FutureWakeWord {
    enabled: bool = false,
    phrase: String = "hi eva".into()
});
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub service: Service,
    pub interaction: Interaction,
    pub audio: Audio,
    pub openai: Openai,
    pub turn_detection: TurnDetection,
    pub execution: Execution,
    pub undo: Undo,
    pub privacy: Privacy,
    pub budget: Budget,
    pub pricing: Pricing,
    pub future_wake_word: FutureWakeWord,
    pub applications: std::collections::BTreeMap<String, Vec<String>>,
}
impl Config {
    pub fn path() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            })
            .join("jwm/eva.toml")
    }
    pub fn load(path: Option<&Path>) -> io::Result<Self> {
        let explicit = path.is_some();
        let path = path.map(Path::to_path_buf).unwrap_or_else(Self::path);
        let value: Self = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|_|io::Error::other("invalid Eva configuration: unknown field or invalid value; compare eva.toml with the bundled sample"))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound && !explicit => Self::default(),
            Err(e) => return Err(e),
        };
        value.validate()?;
        Ok(value)
    }
    /// Configuration contains credential source identifiers, never key values.
    /// Only this digest is shared with CLI clients of the running daemon.
    pub fn fingerprint(&self) -> String {
        jwm_eva_core::protocol::digest(
            &serde_json::to_value(self).expect("configuration is JSON serializable"),
        )
    }
    pub fn validate(&self) -> io::Result<()> {
        let valid = self.service.instance_scope == "graphical_session"
            && (3..=60).contains(&self.service.watchdog_seconds)
            && self.interaction.escape_cancel
            && self.interaction.local_start_detection
            && ["brief", "text", "none", "beep"].contains(&self.interaction.feedback.as_str())
            && self.interaction.trigger == "hotkey"
            && self.interaction.mode == "toggle"
            && self.interaction.half_duplex
            && !self.future_wake_word.enabled
            && !self.privacy.save_raw_audio
            && !self.privacy.persist_transcripts
            && self.execution.require_guarded_execution
            && self.execution.stop_on_error
            && (1..=8).contains(&self.execution.max_actions_per_turn)
            && (1..=3).contains(&self.execution.max_planning_rounds)
            && self.execution.max_schema_repair_attempts <= 1
            && self.audio.realtime_rate_hz == 24000
            && self.audio.channels == 1
            && self.audio.sample_format == "s16le"
            && self.audio.chunk_ms == 20
            && self.audio.playback_tail_ms <= 2000
            && self.undo.max_plans <= 20
            && self.undo.max_age_seconds <= 600
            && self.execution.query_retries == 1
            && (1000..=600000).contains(&self.execution.confirmation_timeout_ms)
            && self.turn_detection.prefix_padding_ms <= 1000
            && (100..=2000).contains(&self.turn_detection.silence_duration_ms)
            && self.openai.plan_max_output_tokens >= 64
            && self.openai.plan_max_output_tokens <= 4096
            && self.openai.feedback_max_output_tokens >= 16
            && self.openai.feedback_max_output_tokens <= 1024
            && self.audio.connect_buffer_ms <= 2000
            && self.audio.connect_buffer_ms >= 20
            && self.openai.transport == "websocket"
            && !self.openai.model.is_empty()
            && self.turn_detection.kind == "server_vad"
            && !self.turn_detection.create_response
            && !self.turn_detection.interrupt_response
            && (0.0..=1.0).contains(&self.turn_detection.threshold)
            && self.pricing.model == self.openai.model
            && !self.pricing.version.is_empty()
            && [
                self.pricing.input_text,
                self.pricing.input_audio,
                self.pricing.cached_text,
                self.pricing.cached_audio,
                self.pricing.output_text,
                self.pricing.output_audio,
                self.pricing.transcription_per_minute,
            ]
            .iter()
            .all(|v| v.is_finite() && *v >= 0.0)
            && self.budget.daily_limit_usd.is_finite()
            && self.budget.daily_limit_usd > 0.0
            && ["Asia/Shanghai", "UTC", "local"].contains(&self.budget.timezone.as_str())
            && ["secret_service", "file", "env"].contains(&self.openai.api_key_source.as_str())
            && [
                self.interaction.follow_up_ms,
                self.interaction.first_speech_timeout_ms,
                self.interaction.max_utterance_ms,
                self.interaction.max_session_ms,
                self.openai.connect_timeout_ms,
                self.openai.plan_deadline_ms,
                self.execution.confirmation_timeout_ms,
            ]
            .iter()
            .all(|v| (100..=600000).contains(v));
        if !valid {
            return Err(io::Error::other(
                "unsupported or out-of-range Eva configuration",
            ));
        }
        if self.audio.input_device.contains("monitor") {
            return Err(io::Error::other(
                "select a microphone, not a playback monitor",
            ));
        }
        Ok(())
    }
    pub async fn api_key(&self) -> io::Result<String> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let key = match self.openai.api_key_source.as_str() {
            "env" => std::env::var("OPENAI_API_KEY")
                .map_err(|_| io::Error::other("OPENAI_API_KEY is not configured"))?,
            "file" => {
                let path = self
                    .openai
                    .api_key_file
                    .as_ref()
                    .ok_or_else(|| io::Error::other("api_key_file required"))?;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)?;
                let meta = file.metadata()?;
                if !meta.is_file()
                    || meta.uid() != unsafe { libc::geteuid() }
                    || meta.mode() & 0o777 != 0o600
                {
                    return Err(io::Error::other(
                        "credential file must be a regular file owned by this user with mode 0600",
                    ));
                }
                let mut text = String::new();
                file.take(8193).read_to_string(&mut text)?;
                if text.len() > 8192 {
                    return Err(io::Error::other("credential file exceeded size limit"));
                }
                text
            }
            _ => {
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    tokio::process::Command::new("secret-tool")
                        .args(["lookup", "service", "jwm-eva", "provider", "openai"])
                        .kill_on_drop(true)
                        .output(),
                )
                .await
                .map_err(|_| io::Error::other("Secret Service timeout"))??;
                if !output.status.success() {
                    return Err(io::Error::other(
                        "OpenAI credential unavailable in Secret Service",
                    ));
                }
                String::from_utf8(output.stdout).map_err(io::Error::other)?
            }
        };
        let key = key.trim().to_string();
        if key.is_empty() || key.contains(['\n', '\r']) {
            return Err(io::Error::other("invalid OpenAI credential"));
        }
        Ok(key)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unknown_and_unsupported() {
        assert!(toml::from_str::<Config>("[openai]\nmodle='oops'").is_err());
        let mut c = Config::default();
        c.future_wake_word.enabled = true;
        assert!(c.validate().is_err());
        c = Config::default();
        c.execution.require_guarded_execution = false;
        assert!(c.validate().is_err());
        c = Config::default();
        c.interaction.local_start_detection = false;
        assert!(c.validate().is_err());
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn explicit_missing_configuration_cannot_fall_back_to_defaults() {
        let path = std::env::temp_dir().join(format!(
            "eva-missing-config-{}-{}.toml",
            std::process::id(),
            crate::store::wall_ms()
        ));
        let error = Config::load(Some(&path)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(Config::default().budget.timezone, "local");
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[tokio::test]
    async fn credential_file_checks_the_opened_inode_and_never_follows_symlinks() {
        let path = std::env::temp_dir().join(format!(
            "eva-credential-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        let file = path.join("key");
        std::fs::write(&file, "test-credential").unwrap();
        let mut c = Config::default();
        c.openai.api_key_source = "file".into();
        c.openai.api_key_file = Some(file.clone());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(c.api_key().await.is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(c.api_key().await.unwrap(), "test-credential");
        let link = path.join("link");
        symlink(&file, &link).unwrap();
        c.openai.api_key_file = Some(link);
        assert!(c.api_key().await.is_err());
        c.openai.api_key_file = Some(file.clone());
        std::fs::write(&file, vec![b'x'; 8193]).unwrap();
        assert!(c.api_key().await.is_err());
        std::fs::remove_dir_all(path).unwrap();
    }
}
