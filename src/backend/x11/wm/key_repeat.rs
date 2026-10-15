//! Recognize classic X11 release/press autorepeat pairs without delaying input.
//!
//! Neither X11 transport enables detectable autorepeat. The server's repeat
//! release and following press share a key, timestamp, and delivery identity.
//! Keep only the immediately preceding raw release: no timer, down-key latch,
//! or focus-dependent cleanup can turn a held shortcut into a new gesture.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyIdentity {
    pub keycode: u8,
    pub time: u32,
    pub event: u32,
    pub root: u32,
    pub synthetic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RawKeyEvent {
    Press(KeyIdentity),
    Release(KeyIdentity),
    Other,
}

#[derive(Default)]
pub(crate) struct ClassicKeyRepeat {
    last_release: Option<KeyIdentity>,
}

impl ClassicKeyRepeat {
    /// Observe every raw event, including events the backend later ignores.
    /// Empty polls and fairness boundaries are not events and leave this alone.
    pub(crate) fn observe(&mut self, event: RawKeyEvent) -> bool {
        let previous = self.last_release.take();
        match event {
            RawKeyEvent::Press(key) => !key.synthetic && previous == Some(key),
            RawKeyEvent::Release(key) => {
                if !key.synthetic {
                    self.last_release = Some(key);
                }
                false
            }
            RawKeyEvent::Other => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(time: u32) -> KeyIdentity {
        KeyIdentity {
            keycode: 58,
            time,
            event: 10,
            root: 1,
            synthetic: false,
        }
    }

    #[test]
    fn classic_pairs_repeat_but_new_physical_presses_do_not() {
        let mut repeat = ClassicKeyRepeat::default();
        assert!(!repeat.observe(RawKeyEvent::Press(key(10))));
        for time in [400, 450, 500] {
            assert!(!repeat.observe(RawKeyEvent::Release(key(time))));
            assert!(repeat.observe(RawKeyEvent::Press(key(time))));
        }
        assert!(!repeat.observe(RawKeyEvent::Release(key(501))));
        // One millisecond later is a new gesture, not a debounce interval.
        assert!(!repeat.observe(RawKeyEvent::Press(key(502))));
    }

    #[test]
    fn an_empty_poll_or_batch_boundary_does_not_break_a_pair() {
        let mut repeat = ClassicKeyRepeat::default();
        for _ in 0..63 {
            repeat.observe(RawKeyEvent::Other);
        }
        repeat.observe(RawKeyEvent::Release(key(900)));
        // The first batch is exhausted; there is no next event in this poll.
        // The same persistent helper receives the next batch's first event.
        assert!(repeat.observe(RawKeyEvent::Press(key(900))));
        assert!(!repeat.observe(RawKeyEvent::Press(key(900))));
    }

    #[test]
    fn raw_intervening_events_invalidate_pairing_even_when_unmapped() {
        // Focus, unmap, mapping and unknown events all use Other. There is no
        // pressed-key state to leak when their context loses a release.
        let mut repeat = ClassicKeyRepeat::default();
        repeat.observe(RawKeyEvent::Release(key(900)));
        repeat.observe(RawKeyEvent::Other);
        assert!(!repeat.observe(RawKeyEvent::Press(key(900))));
    }

    #[test]
    fn different_key_window_root_and_send_event_are_not_repeats() {
        let original = key(900);
        for changed in [
            KeyIdentity {
                keycode: 59,
                ..original
            },
            KeyIdentity {
                event: 11,
                ..original
            },
            KeyIdentity {
                root: 2,
                ..original
            },
            KeyIdentity {
                synthetic: true,
                ..original
            },
        ] {
            let mut repeat = ClassicKeyRepeat::default();
            repeat.observe(RawKeyEvent::Release(original));
            assert!(!repeat.observe(RawKeyEvent::Press(changed)));
            repeat.observe(RawKeyEvent::Release(changed));
            assert!(!repeat.observe(RawKeyEvent::Press(original)));
        }
        let mut repeat = ClassicKeyRepeat::default();
        let synthetic = KeyIdentity {
            synthetic: true,
            ..original
        };
        repeat.observe(RawKeyEvent::Release(synthetic));
        assert!(!repeat.observe(RawKeyEvent::Press(synthetic)));
    }

    #[test]
    fn timestamp_wrap_does_not_become_a_debounce_window() {
        let mut repeat = ClassicKeyRepeat::default();
        repeat.observe(RawKeyEvent::Release(key(u32::MAX)));
        assert!(!repeat.observe(RawKeyEvent::Press(key(0))));
        repeat.observe(RawKeyEvent::Release(key(0)));
        assert!(repeat.observe(RawKeyEvent::Press(key(0))));
    }
}
