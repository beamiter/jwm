//! Requests that need the pointer, made while somebody else still had it.
//!
//! X11 hands a client an implicit pointer grab for as long as a button pressed
//! inside it stays down. A status bar therefore owns the pointer for the whole
//! duration of a click on one of its pills — and the request that click sends
//! arrives at the window manager in well under the time a human takes to let
//! go. Anything that answers by grabbing the pointer finds it already taken.
//!
//! Refusing is the wrong answer to a condition that clears itself in a few
//! milliseconds, and it is the reason two different pills looked dead: the
//! screenshot pill and the shell hub. The request is parked here instead and
//! retried from the event loop until the pointer comes free, then given up on
//! so a grab nobody intends to release cannot leave a request pending forever.
//!
//! Every feature reached from a bar this way belongs here rather than growing
//! its own retry, which is what keeps the next one from rediscovering the bug.

use super::ShellHubRoute;

/// What to do once the pointer is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredGrabAction {
    /// Enter interactive region capture, saving to the path already chosen.
    /// The path is picked up front so a retry cannot land in a different
    /// second and produce a filename that disagrees with the log line.
    Screenshot { output_path: String },
    /// Open the shell from a status bar. `None` is the hub home page.
    ShellHub { route: Option<ShellHubRoute> },
}

impl DeferredGrabAction {
    /// What to call this request in a log line.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Screenshot { .. } => "interactive capture",
            Self::ShellHub { .. } => "shell hub",
        }
    }
}

/// One parked request and the point at which it stops being worth retrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredGrab {
    pub action: DeferredGrabAction,
    pub deadline: std::time::Instant,
}

impl DeferredGrab {
    #[must_use]
    pub fn new(action: DeferredGrabAction, now: std::time::Instant) -> Self {
        Self {
            action,
            deadline: now + TIMEOUT,
        }
    }

    #[must_use]
    pub fn is_expired(&self, now: std::time::Instant) -> bool {
        now >= self.deadline
    }
}

/// How long to keep retrying before concluding the pointer is held by
/// something that is not going to let go. Comfortably longer than a slow
/// click, short enough that a genuinely stuck grab does not linger.
pub const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// How often to retry while waiting. One frame: the wait ends on a button
/// release, which is not an event this side is told about, so it has to poll.
pub const RETRY: std::time::Duration = std::time::Duration::from_millis(16);

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression that made two different pills look dead: a request
    /// arriving while the bar still held its implicit pointer grab used to be
    /// refused outright. It has to survive a slow click…
    #[test]
    fn a_parked_request_outlives_a_slow_click() {
        let now = std::time::Instant::now();
        let parked = DeferredGrab::new(
            DeferredGrabAction::Screenshot {
                output_path: "/tmp/shot.png".to_owned(),
            },
            now,
        );
        assert!(!parked.is_expired(now));
        assert!(!parked.is_expired(now + RETRY));
        assert!(!parked.is_expired(now + std::time::Duration::from_millis(500)));
    }

    /// …and still give up, rather than wait on a grab nobody will release.
    #[test]
    fn a_parked_request_expires() {
        let now = std::time::Instant::now();
        let parked = DeferredGrab::new(DeferredGrabAction::ShellHub { route: None }, now);
        assert!(parked.is_expired(now + std::time::Duration::from_secs(5)));
        assert!(RETRY < TIMEOUT, "a request must get more than one attempt");
    }

    #[test]
    fn every_action_has_something_to_call_itself_in_a_log() {
        for action in [
            DeferredGrabAction::Screenshot {
                output_path: String::new(),
            },
            DeferredGrabAction::ShellHub { route: None },
            DeferredGrabAction::ShellHub {
                route: Some(ShellHubRoute::Clipboard),
            },
        ] {
            assert!(!action.label().is_empty());
        }
    }
}
