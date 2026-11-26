//! Wake the GTK main context from workers without polling or queuing payloads.
use futures_channel::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct Wake(Arc<Mutex<Sender<()>>>);

impl Wake {
    pub(super) fn channel() -> (Self, Receiver<()>) {
        // futures-channel reserves one slot per underlying Sender. Keep exactly
        // one Sender, shared by all producers, so pending wakeups stay at one.
        let (sender, receiver) = mpsc::channel(0);
        (Self(Arc::new(Mutex::new(sender))), receiver)
    }

    /// Publish the result first; GTK consumes the wake before reading results.
    pub(super) fn signal(&self) {
        // Full means another signal is already pending. Disconnected means the
        // browser closed. Neither condition needs more work.
        let _ = self.0.lock().expect("wake sender poisoned").try_send(());
    }
}

pub(super) struct SignalOnDrop(pub(super) Wake);

impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        self.0.signal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_channel_coalesces_and_can_be_rearmed_from_another_thread() {
        let (wake, mut receive) = Wake::channel();
        assert!(receive.try_recv().is_err());
        let producer = wake.clone();
        std::thread::spawn(move || {
            for _ in 0..4096 {
                producer.signal();
            }
        })
        .join()
        .unwrap();
        assert_eq!(receive.try_recv(), Ok(()));
        assert!(receive.try_recv().is_err());
        wake.signal();
        assert_eq!(receive.try_recv(), Ok(()));
    }
}
