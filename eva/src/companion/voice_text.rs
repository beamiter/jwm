//! In-memory only: the latest few spoken replies, plus the optional provider
//! caption of the request, kept so that an explicit host-side review can ask
//! for one through jcompaniond. Nothing here touches disk. Retention stays off
//! until the coordinator enables it; pause, lock, forget and disconnect drop
//! everything. Offering text is not consent: the coordinator verifies it
//! against the recorded reply digest and the host still reviews and approves.
use companion_voice::text::{self, Offer, Push, Unavailable};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(crate) const MAX_EXCHANGES: usize = 4;
pub(crate) const TTL: Duration = Duration::from_secs(600);

struct Exchange {
    session: String,
    turn: String,
    reply: Option<String>,
    caption: Option<String>,
    at: Instant,
}

#[derive(Default)]
pub(crate) struct Retained {
    enabled: bool,
    exchanges: VecDeque<Exchange>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Offer(Offer),
    Unavailable(u64, Unavailable),
    Forgotten(u64, u32),
}

impl Retained {
    fn entry(&mut self, session: &str, turn: &str, now: Instant) -> &mut Exchange {
        if let Some(index) = self
            .exchanges
            .iter()
            .position(|e| e.session == session && e.turn == turn)
        {
            return &mut self.exchanges[index];
        }
        if self.exchanges.len() >= MAX_EXCHANGES {
            self.exchanges.pop_front();
        }
        self.exchanges.push_back(Exchange {
            session: session.into(),
            turn: turn.into(),
            reply: None,
            caption: None,
            at: now,
        });
        self.exchanges.back_mut().expect("just pushed")
    }
    /// Keeps the spoken reply of a completed turn while retention is on.
    pub(crate) fn reply(&mut self, session: &str, turn: &str, reply: &str, now: Instant) {
        if self.enabled && text::text_valid(reply, text::MAX_REPLY_CHARS) {
            let entry = self.entry(session, turn, now);
            entry.reply = Some(reply.into());
            entry.at = now;
        }
    }
    /// Keeps the provider caption only when input transcription is enabled.
    pub(crate) fn caption(&mut self, session: &str, turn: &str, caption: &str, now: Instant) {
        if self.enabled && text::text_valid(caption, text::MAX_CAPTION_CHARS) {
            self.entry(session, turn, now).caption = Some(caption.into());
        }
    }
    fn clear(&mut self) -> u32 {
        let count = self.exchanges.len() as u32;
        self.exchanges.clear();
        count
    }
    pub(crate) fn disconnected(&mut self) {
        self.enabled = false;
        self.clear();
    }
    pub(crate) fn prune(&mut self, now: Instant) {
        self.exchanges
            .retain(|e| now.saturating_duration_since(e.at) < TTL);
    }
    pub(crate) fn answer(&mut self, push: Push, now: Instant) -> Option<Answer> {
        self.prune(now);
        match push {
            Push::Retention { enabled } => {
                self.enabled = enabled;
                if !enabled {
                    self.clear();
                }
                None
            }
            Push::Forget { request_id } => Some(Answer::Forgotten(request_id, self.clear())),
            Push::Request {
                request_id,
                voice_session_id,
                turn_id,
            } => {
                if !self.enabled {
                    return Some(Answer::Unavailable(
                        request_id,
                        Unavailable::RetentionPaused,
                    ));
                }
                Some(
                    match self.exchanges.iter().find(|e| {
                        e.session == voice_session_id && e.turn == turn_id && e.reply.is_some()
                    }) {
                        Some(exchange) => Answer::Offer(Offer {
                            request_id,
                            voice_session_id,
                            turn_id,
                            reply: exchange.reply.clone().expect("filtered"),
                            caption: exchange.caption.clone(),
                        }),
                        None => Answer::Unavailable(request_id, Unavailable::NotRetained),
                    },
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(id: u64, turn: &str) -> Push {
        Push::Request {
            request_id: id,
            voice_session_id: "activation".into(),
            turn_id: turn.into(),
        }
    }
    #[test]
    fn nothing_is_kept_until_the_coordinator_enables_retention() {
        let now = Instant::now();
        let mut kept = Retained::default();
        kept.reply("activation", "turn", "好的", now);
        assert_eq!(
            kept.answer(request(1, "turn"), now),
            Some(Answer::Unavailable(1, Unavailable::RetentionPaused))
        );
        kept.answer(Push::Retention { enabled: true }, now);
        assert_eq!(
            kept.answer(request(2, "turn"), now),
            Some(Answer::Unavailable(2, Unavailable::NotRetained)),
            "text from before enabling was never kept"
        );
    }
    #[test]
    fn the_exact_turn_is_offered_with_its_caption_until_expiry() {
        let now = Instant::now();
        let mut kept = Retained::default();
        kept.answer(Push::Retention { enabled: true }, now);
        kept.caption("activation", "turn", "把窗口移到右边", now);
        kept.reply("activation", "turn", "好的，已经移过去了。", now);
        kept.reply("activation", "other", "另一个回复", now);
        let Some(Answer::Offer(offer)) = kept.answer(request(3, "turn"), now) else {
            panic!("offer")
        };
        assert_eq!(offer.reply, "好的，已经移过去了。");
        assert_eq!(offer.caption.as_deref(), Some("把窗口移到右边"));
        assert!(offer.valid());
        assert_eq!(
            kept.answer(request(4, "turn"), now + TTL),
            Some(Answer::Unavailable(4, Unavailable::NotRetained))
        );
    }
    #[test]
    fn bounded_and_cleared_by_pause_forget_and_disconnect() {
        let now = Instant::now();
        let mut kept = Retained::default();
        kept.answer(Push::Retention { enabled: true }, now);
        for turn in 0..6 {
            kept.reply("activation", &format!("turn-{turn}"), "reply", now);
        }
        assert_eq!(
            kept.answer(request(1, "turn-0"), now),
            Some(Answer::Unavailable(1, Unavailable::NotRetained)),
            "oldest evicted"
        );
        assert_eq!(
            kept.answer(Push::Forget { request_id: 2 }, now),
            Some(Answer::Forgotten(2, MAX_EXCHANGES as u32))
        );
        kept.reply("activation", "turn", "reply", now);
        kept.answer(Push::Retention { enabled: false }, now);
        kept.answer(Push::Retention { enabled: true }, now);
        assert!(matches!(
            kept.answer(request(3, "turn"), now),
            Some(Answer::Unavailable(3, Unavailable::NotRetained))
        ));
        kept.reply("activation", "turn", "reply", now);
        kept.disconnected();
        assert!(matches!(
            kept.answer(request(4, "turn"), now),
            Some(Answer::Unavailable(4, Unavailable::RetentionPaused))
        ));
        kept.answer(Push::Retention { enabled: true }, now);
        kept.reply("activation", "turn", "two\nlines", now);
        assert!(matches!(
            kept.answer(request(5, "turn"), now),
            Some(Answer::Unavailable(5, Unavailable::NotRetained))
        ));
    }
}
