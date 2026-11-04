//! Select one real local neural backend per input stream. Initialization may
//! use the configured RNNoise fallback; inference failures end the turn rather
//! than admitting unclassified audio or changing models mid-utterance.
use super::speech::SpeechDetector;
use crate::config::Config;
use serde::Serialize;
use std::io;

#[derive(Clone, Debug, Serialize)]
pub struct DetectorStatus {
    pub requested: String,
    pub active: &'static str,
    pub degraded_reason: Option<&'static str>,
}

/// Provenance describes this probe only. It never promotes an initialized model
/// to microphone, acoustic short-command, or end-to-end acceptance.
pub fn diagnostic(status: &DetectorStatus) -> serde_json::Value {
    let (model, hash, runtime) = if status.active == "silero" {
        #[cfg(feature = "silero")]
        {
            (
                Some("silero-v5.1.2"),
                Some(super::silero::MODEL_SHA256),
                super::silero::runtime_version(),
            )
        }
        #[cfg(not(feature = "silero"))]
        {
            (None, None, None)
        }
    } else if status.active == "rnnoise" {
        (Some("nnnoiseless-built-in-rnnoise"), None, None)
    } else {
        (None, None, None)
    };
    serde_json::json!({
        "requested":status.requested,"active":status.active,
        "degraded_reason":status.degraded_reason,"model":model,"model_sha256":hash,
        "runtime_version":runtime,"capture_sample_rate":24000,
        "model_sample_rate":if status.active=="silero"{Some(16000)}else{None},
        "model_window_samples":if status.active=="silero"{Some(512)}else{None},
        "verification":{"kind":"local_initialization","physical_audio":"not_tested",
            "chinese_short_commands":"not_tested","last_physical_validation":null}
    })
}

pub struct Detection {
    pub probability: f32,
    pub ready: bool,
    /// RMS of the same input window as probability. A 32 ms Silero decision
    /// can finish after a native 20 ms frame has already fallen quiet.
    pub evidence_rms: f32,
}

enum Backend {
    Disabled,
    Rnnoise(Box<SpeechDetector>),
    #[cfg(feature = "silero")]
    Silero(Box<super::silero::SileroDetector>),
    Unavailable(&'static str),
}

pub struct LocalVoiceDetector {
    backend: Backend,
    pub status: DetectorStatus,
}

impl LocalVoiceDetector {
    pub(crate) fn disabled(config: &Config) -> Self {
        Self {
            backend: Backend::Disabled,
            status: DetectorStatus {
                requested: config.audio.local_voice_backend.clone(),
                active: "disabled",
                degraded_reason: None,
            },
        }
    }
    pub fn new(config: &Config) -> Self {
        if !config.audio.local_voice_detection {
            return Self::disabled(config);
        }
        let requested = config.audio.local_voice_backend.clone();
        if requested == "rnnoise" {
            return Self {
                backend: Backend::Rnnoise(Box::default()),
                status: DetectorStatus {
                    requested,
                    active: "rnnoise",
                    degraded_reason: None,
                },
            };
        }
        #[cfg(feature = "silero")]
        let reason =
            match super::silero::SileroDetector::new(config.audio.silero_model_path.as_deref()) {
                Ok(detector) => {
                    return Self {
                        backend: Backend::Silero(Box::new(detector)),
                        status: DetectorStatus {
                            requested,
                            active: "silero",
                            degraded_reason: None,
                        },
                    };
                }
                Err(reason) => reason,
            };
        #[cfg(not(feature = "silero"))]
        let reason = "silero_feature_unavailable";
        let fallback = config.audio.local_voice_fallback == "rnnoise";
        Self {
            backend: if fallback {
                Backend::Rnnoise(Box::default())
            } else {
                Backend::Unavailable(reason)
            },
            status: DetectorStatus {
                requested,
                active: if fallback { "rnnoise" } else { "unavailable" },
                degraded_reason: Some(reason),
            },
        }
    }

    pub fn reset(&mut self) {
        match &mut self.backend {
            Backend::Disabled => {}
            Backend::Rnnoise(detector) => detector.reset(),
            #[cfg(feature = "silero")]
            Backend::Silero(detector) => detector.reset(),
            Backend::Unavailable(_) => {}
        }
    }

    pub fn process(&mut self, pcm: &[u8]) -> io::Result<Detection> {
        let result = match &mut self.backend {
            Backend::Disabled => Err(io::Error::other("local_voice_detection_disabled")),
            Backend::Rnnoise(detector) => detector.process(pcm).map(|score| Detection {
                probability: score.probability,
                ready: score.ready,
                evidence_rms: super::rms(pcm),
            }),
            #[cfg(feature = "silero")]
            Backend::Silero(detector) => detector.process(pcm),
            Backend::Unavailable(reason) => Err(io::Error::other(*reason)),
        };
        if result.is_err() {
            self.reset();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_is_reported_and_fallback_is_explicit() {
        let mut c = Config::default();
        c.audio.silero_model_path = Some("/nonexistent/eva-test-silero.onnx".into());
        let mut detector = LocalVoiceDetector::new(&c);
        assert_eq!(detector.status.requested, "silero");
        assert_eq!(detector.status.active, "rnnoise");
        assert!(detector.status.degraded_reason.is_some());
        assert!(detector.process(&[0; 960]).is_ok());
        c.audio.local_voice_fallback = "none".into();
        let mut detector = LocalVoiceDetector::new(&c);
        assert_eq!(detector.status.active, "unavailable");
        assert!(detector.process(&[0; 960]).is_err());
    }

    #[test]
    fn explicit_rnnoise_needs_neither_runtime_nor_model() {
        let mut c = Config::default();
        c.audio.local_voice_backend = "rnnoise".into();
        let detector = LocalVoiceDetector::new(&c);
        assert_eq!(detector.status.active, "rnnoise");
        assert!(detector.status.degraded_reason.is_none());
    }
}
