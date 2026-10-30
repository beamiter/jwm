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
    follow_up_ms: u64 = 30000,
    first_speech_timeout_ms: u64 = 8000,
    max_utterance_ms: u64 = 120000,
    max_session_ms: u64 = 600000,
    half_duplex: bool = false,
    escape_cancel: bool = true,
    feedback: String = "voice".into(),
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
    playback_tail_ms: u64 = 200,
    noise_reduction: String = "near_field".into()
});
section!(Openai {
    model: String = "gpt-realtime".into(), transport: String = "websocket".into(),
    api_key_source: String = "secret_service".into(), api_key_file: Option<PathBuf> = None,
    voice: String = "marin".into(), input_transcription: bool = false, connect_timeout_ms: u64 = 8000,
    plan_deadline_ms: u64 = 30000, plan_max_output_tokens: u32 = 4096, feedback_max_output_tokens: u32 = 1024,
    // Spoken feedback for a verified desktop outcome is one or two sentences;
    // open conversation (noop) keeps the full feedback_max_output_tokens.
    feedback_result_max_output_tokens: u32 = 256
});
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TurnDetection {
    #[serde(rename = "type")]
    pub kind: String,
    pub eagerness: String,
    pub threshold: f64,
    pub prefix_padding_ms: u64,
    pub silence_duration_ms: u64,
    pub create_response: bool,
    pub interrupt_response: bool,
}
impl Default for TurnDetection {
    fn default() -> Self {
        Self {
            kind: "semantic_vad".into(),
            eagerness: "auto".into(),
            threshold: 0.5,
            prefix_padding_ms: 300,
            silence_duration_ms: 600,
            create_response: false,
            interrupt_response: false,
        }
    }
}
section!(Execution {
    max_actions_per_turn: usize = 32,
    max_planning_rounds: u8 = 8,
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
    model: String = "gpt-realtime".into(),
    input_text: f64 = 4.0,
    input_audio: f64 = 32.0,
    cached_text: f64 = 0.40,
    cached_audio: f64 = 0.40,
    output_text: f64 = 16.0,
    output_audio: f64 = 64.0,
    transcription_per_minute: f64 = 0.0045
});
section!(WakeWord {
    enabled: bool = false,
    phrase: String = "hi eva".into(),
    model: String = "gpt-realtime-mini".into(),
    reconnect_ms: u64 = 3000,
    max_session_ms: u64 = 600000,
    // Keep silence and background noise local during cloud standby; only
    // sustained speech-like audio (plus a short pre-roll) reaches the mini model.
    local_speech_gate: bool = true
});
fn mini_pricing() -> Pricing {
    Pricing {
        model: "gpt-realtime-mini".into(),
        input_text: 0.60,
        input_audio: 10.0,
        cached_text: 0.06,
        cached_audio: 0.30,
        output_text: 2.40,
        output_audio: 20.0,
        ..Pricing::default()
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
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
    #[serde(alias = "future_wake_word")]
    pub wake_word: WakeWord,
    #[serde(default = "mini_pricing")]
    pub wake_pricing: Pricing,
    pub applications: std::collections::BTreeMap<String, Vec<String>>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            service: Service::default(),
            interaction: Interaction::default(),
            audio: Audio::default(),
            openai: Openai::default(),
            turn_detection: TurnDetection::default(),
            execution: Execution::default(),
            undo: Undo::default(),
            privacy: Privacy::default(),
            budget: Budget::default(),
            pricing: Pricing::default(),
            wake_word: WakeWord::default(),
            wake_pricing: mini_pricing(),
            applications: Default::default(),
        }
    }
}
impl Config {
    /// Model changes require a new Realtime connection; billing follows each model.
    pub fn wake_config(&self) -> Self {
        let mut config = self.clone();
        config.openai.model = self.wake_word.model.clone();
        config.openai.input_transcription = false;
        config.pricing = self.wake_pricing.clone();
        // Short wake phrases benefit from predictable acoustic boundaries;
        // semantic endpointing is reserved for the full conversation.
        config.turn_detection.kind = "server_vad".into();
        config
    }
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
            && ["voice", "brief", "text", "none", "beep"]
                .contains(&self.interaction.feedback.as_str())
            && self.interaction.trigger == "hotkey"
            && self.interaction.mode == "toggle"
            && !self.privacy.save_raw_audio
            && !self.privacy.persist_transcripts
            && self.execution.require_guarded_execution
            && self.execution.stop_on_error
            && (1..=32).contains(&self.execution.max_actions_per_turn)
            && (1..=8).contains(&self.execution.max_planning_rounds)
            && self.execution.max_schema_repair_attempts <= 1
            && self.audio.realtime_rate_hz == 24000
            && self.audio.channels == 1
            && self.audio.sample_format == "s16le"
            && self.audio.chunk_ms == 20
            && ["near_field", "far_field", "off"].contains(&self.audio.noise_reduction.as_str())
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
            && (16..=1024).contains(&self.openai.feedback_result_max_output_tokens)
            && self.audio.connect_buffer_ms <= 2000
            && self.audio.connect_buffer_ms >= 20
            && self.openai.transport == "websocket"
            && !self.openai.model.is_empty()
            && ["server_vad", "semantic_vad"].contains(&self.turn_detection.kind.as_str())
            && ["auto", "low", "medium", "high"].contains(&self.turn_detection.eagerness.as_str())
            && !self.turn_detection.create_response
            && !self.turn_detection.interrupt_response
            && (0.0..=1.0).contains(&self.turn_detection.threshold)
            && self.wake_pricing.model == self.wake_word.model
            && !self.wake_word.model.is_empty()
            && !self.wake_word.phrase.trim().is_empty()
            && self.wake_word.phrase.len() <= 80
            && !self.wake_word.phrase.chars().any(char::is_control)
            && (1000..=60000).contains(&self.wake_word.reconnect_ms)
            && (1000..=3600000).contains(&self.wake_word.max_session_ms)
            && valid_pricing(&self.wake_pricing)
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
fn valid_pricing(p: &Pricing) -> bool {
    !p.version.is_empty()
        && [
            p.input_text,
            p.input_audio,
            p.cached_text,
            p.cached_audio,
            p.output_text,
            p.output_audio,
            p.transcription_per_minute,
        ]
        .iter()
        .all(|v| v.is_finite() && *v >= 0.0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpointing_and_denoising_support_legacy_settings_without_auto_responses() {
        let mut c = Config::default();
        assert_eq!(c.turn_detection.kind, "semantic_vad");
        assert_eq!(c.turn_detection.eagerness, "auto");
        assert!(c.validate().is_ok());
        c.turn_detection.kind = "server_vad".into();
        c.audio.noise_reduction = "off".into();
        assert!(c.validate().is_ok());
        c.turn_detection.create_response = true;
        assert!(c.validate().is_err());
        c.turn_detection.create_response = false;
        c.turn_detection.eagerness = "extreme".into();
        assert!(c.validate().is_err());
        c.turn_detection.eagerness = "auto".into();
        c.audio.noise_reduction = "unknown".into();
        assert!(c.validate().is_err());
    }
    #[test]
    fn rejects_unknown_and_unsupported() {
        assert!(toml::from_str::<Config>("[openai]\nmodle='oops'").is_err());
        let mut c = Config::default();
        c.wake_word.enabled = true;
        assert!(c.validate().is_ok());
        c.wake_word.phrase.clear();
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
    fn wake_model_uses_separate_rates_and_preserves_credential_source() {
        let mut config = Config::default();
        config.openai.api_key_source = "env".into();
        let wake = config.wake_config();
        assert_eq!(config.openai.model, "gpt-realtime");
        assert_eq!(wake.openai.model, "gpt-realtime-mini");
        assert_eq!(wake.pricing.model, wake.openai.model);
        assert_eq!(wake.pricing.input_audio, 10.0);
        assert_eq!(config.pricing.input_audio, 32.0);
        assert_eq!(wake.openai.api_key_source, "env");
        assert!(!wake.openai.input_transcription);
        assert!(!config.interaction.half_duplex);
        assert!(config.validate().is_ok());
        config.wake_pricing.model = config.openai.model.clone();
        assert!(config.validate().is_err());
    }
    #[test]
    fn legacy_wake_section_migrates_and_duplicate_sections_fail() {
        let config: Config = toml::from_str("[future_wake_word]\nenabled=true").unwrap();
        assert!(config.wake_word.enabled);
        assert!(config.validate().is_ok());
        assert!(
            toml::from_str::<Config>(
                "[future_wake_word]\nenabled=true\n[wake_word]\nenabled=false"
            )
            .is_err()
        );
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
