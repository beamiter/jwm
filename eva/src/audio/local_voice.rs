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

pub struct Detection {
    pub probability: f32,
    pub ready: bool,
}

enum Backend {
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
    pub fn new(config: &Config) -> Self {
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
            Backend::Rnnoise(detector) => detector.reset(),
            #[cfg(feature = "silero")]
            Backend::Silero(detector) => detector.reset(),
            Backend::Unavailable(_) => {}
        }
    }

    pub fn process(&mut self, pcm: &[u8]) -> io::Result<Detection> {
        let result = match &mut self.backend {
            Backend::Rnnoise(detector) => detector.process(pcm).map(|score| Detection {
                probability: score.probability,
                ready: score.ready,
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
