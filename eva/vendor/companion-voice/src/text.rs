//! Request-bound transfer of one retained voice exchange (dialogue version 3).
//! Eva keeps reply text only in memory and offers it only when the coordinator
//! relays an explicit host review request. Nothing here persists text, audio
//! or a transcript, and an offer is never memory consent by itself.
use companion_protocol::identifier;
use serde::{Deserialize, Serialize};

/// Dialogue version that adds this exchange to the reference extension.
pub const VERSION: u16 = 3;
pub const MAX_REPLY_CHARS: usize = 4096;
pub const MAX_CAPTION_CHARS: usize = 160;

/// Plain text only: bounded and free of control characters.
pub fn text_valid(text: &str, max_chars: usize) -> bool {
    !text.is_empty() && text.chars().count() <= max_chars && !text.chars().any(char::is_control)
}

/// Coordinator → Eva, only on a connection that negotiated [`VERSION`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Push {
    /// Offer the retained text of exactly this original turn, if still held.
    #[serde(rename = "voice_text_request")]
    Request {
        request_id: u64,
        voice_session_id: String,
        turn_id: String,
    },
    /// Drop every retained exchange now.
    #[serde(rename = "voice_text_forget")]
    Forget { request_id: u64 },
    /// Retention is allowed only while memory is not paused and the desktop is
    /// unlocked and known. Disabled means drop and keep nothing.
    #[serde(rename = "voice_text_retention")]
    Retention { enabled: bool },
}
impl Push {
    pub fn valid(&self) -> bool {
        match self {
            Self::Request {
                request_id,
                voice_session_id,
                turn_id,
            } => *request_id > 0 && identifier(voice_session_id) && identifier(turn_id),
            Self::Forget { request_id } => *request_id > 0,
            Self::Retention { .. } => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    /// Expired, evicted, never generated, or retention was off for that turn.
    NotRetained,
    /// Retention is currently disabled.
    RetentionPaused,
}

/// Eva → coordinator answer to [`Push::Request`]. The caption is the optional
/// provider input transcription, already bounded by Eva.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub request_id: u64,
    pub voice_session_id: String,
    pub turn_id: String,
    pub reply: String,
    pub caption: Option<String>,
}
impl Offer {
    pub fn valid(&self) -> bool {
        self.request_id > 0
            && identifier(&self.voice_session_id)
            && identifier(&self.turn_id)
            && text_valid(&self.reply, MAX_REPLY_CHARS)
            && self
                .caption
                .as_ref()
                .is_none_or(|c| text_valid(c, MAX_CAPTION_CHARS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offers_are_bounded_plain_text_with_identities() {
        let mut offer = Offer {
            request_id: 1,
            voice_session_id: "activation".into(),
            turn_id: "turn".into(),
            reply: "好的".into(),
            caption: Some("打开浏览器".into()),
        };
        assert!(offer.valid());
        offer.reply = "x".repeat(MAX_REPLY_CHARS + 1);
        assert!(!offer.valid());
        offer.reply = "line\nbreak".into();
        assert!(!offer.valid());
        offer.reply = "ok".into();
        offer.caption = Some("c".repeat(MAX_CAPTION_CHARS + 1));
        assert!(!offer.valid());
        offer.caption = Some(String::new());
        assert!(!offer.valid());
        offer.caption = None;
        offer.request_id = 0;
        assert!(!offer.valid());
    }
    #[test]
    fn pushes_need_real_request_ids_and_identities() {
        assert!(Push::Retention { enabled: false }.valid());
        assert!(!Push::Forget { request_id: 0 }.valid());
        assert!(!Push::Request {
            request_id: 1,
            voice_session_id: "a b".into(),
            turn_id: "t".into()
        }
        .valid());
        let wire = serde_json::to_value(Push::Forget { request_id: 7 }).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"type":"voice_text_forget","request_id":7})
        );
    }
}
