//! One explicit text attempt, released only by actual presentation.
use super::*;
use crate::remote::wayland_text_wire::{ExplicitText, TextCommit, TextResult};
#[derive(Default)]
pub(super) struct ClientText {
    pub(super) payload: Option<ExplicitText>,
    requested: Option<(DrawnScope, Instant)>,
    completed: bool,
}
impl ClientText {
    pub(super) fn presented(&mut self, scope: DrawnScope) -> Option<TextCommit> {
        let text = self.payload.take()?;
        self.requested = Some((scope, Instant::now()));
        Some(TextCommit { scope, text })
    }
    pub(super) fn receive(&mut self, result: TextResult) -> RemoteResult<()> {
        self.check()?;
        if self.completed
            || self
                .requested
                .is_none_or(|(scope, _)| scope != result.scope)
        {
            return Err(invalid_data("unsolicited or repeated text result").into());
        }
        self.completed = true;
        if !result.accepted {
            return Err(invalid_data(
                "target text context rejected the one-shot submission; it will not be retried",
            )
            .into());
        }
        eprintln!(
            "jwm-remote: one-shot text sent as commit_string/done to the target resource; application insertion or saving is not confirmed"
        );
        Ok(())
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        if self.completed {
            None
        } else {
            self.requested.map(|(_, at)| at + Duration::from_secs(5))
        }
    }
    pub(super) fn check(&self) -> RemoteResult<()> {
        if self.deadline().is_some_and(|at| Instant::now() >= at) {
            Err(invalid_data("text result timed out; outcome uncertain; no retry").into())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope() -> DrawnScope {
        DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
        }
    }
    #[test]
    fn only_presented_scope_releases_exact_text_once() {
        let mut s = ClientText {
            payload: Some(ExplicitText::new("中文\n".as_bytes().to_vec()).unwrap()),
            ..Default::default()
        };
        assert!(s.requested.is_none());
        assert!(s.deadline().is_none());
        let req = s.presented(scope()).unwrap();
        assert_eq!(req.text.as_bytes(), "中文\n".as_bytes());
        assert!(s.presented(scope()).is_none());
        let mut wrong = scope();
        wrong.frame_sequence = 1;
        assert!(
            s.receive(TextResult {
                scope: wrong,
                accepted: true
            })
            .is_err()
        );
        s.receive(TextResult {
            scope: scope(),
            accepted: true,
        })
        .unwrap();
        assert!(
            s.receive(TextResult {
                scope: scope(),
                accepted: true
            })
            .is_err()
        );
    }
    #[test]
    fn matching_late_results_cannot_erase_timeout() {
        for accepted in [false, true] {
            let mut s = ClientText {
                payload: Some(ExplicitText::new(b"x".to_vec()).unwrap()),
                ..Default::default()
            };
            s.presented(scope()).unwrap();
            s.requested.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(6);
            assert!(
                s.receive(TextResult {
                    scope: scope(),
                    accepted
                })
                .is_err()
            );
            assert!(!s.completed);
            assert!(s.check().is_err());
            assert!(s.presented(scope()).is_none());
        }
    }
    #[test]
    fn rejection_and_timeout_never_requeue_payload() {
        for timeout in [false, true] {
            let mut s = ClientText {
                payload: Some(ExplicitText::new(b"x".to_vec()).unwrap()),
                ..Default::default()
            };
            s.presented(scope()).unwrap();
            if timeout {
                s.requested.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(6);
                assert!(s.check().is_err());
            } else {
                assert!(
                    s.receive(TextResult {
                        scope: scope(),
                        accepted: false
                    })
                    .is_err()
                );
            }
            assert!(s.presented(scope()).is_none());
        }
    }
}
