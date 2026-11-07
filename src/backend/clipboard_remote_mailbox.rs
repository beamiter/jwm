//! Remote-only bounded transport between the X watcher and session workers.
use super::{CopiedClipboard, RemoteServe};
use crate::backend::clipboard_offer::{MAX_FILE_LIST_ITEMS, MAX_TEXT_BYTES};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

pub(super) const SERVE_BUDGET: usize = 8;
const MAX_COMMANDS: usize = 32;
pub(super) const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_RETAINED_BYTES: usize = MAX_IMAGE_BYTES + 1024 * 1024;
const MAX_FILE_BYTES: usize = MAX_TEXT_BYTES;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Count retained allocation, not merely initialized length. A caller can
/// otherwise hide arbitrarily large allocations in short strings/vectors.
pub(super) fn paths_bytes(paths: &Vec<std::path::PathBuf>) -> Option<usize> {
    if paths.len() > MAX_FILE_LIST_ITEMS || paths.capacity() > MAX_FILE_LIST_ITEMS {
        return None;
    }
    let mut retained = paths
        .capacity()
        .checked_mul(std::mem::size_of::<std::path::PathBuf>())?;
    // Upper bound all three output encodings before file_list_payloads builds
    // any URI/string. Each input byte may percent-encode to three URI bytes
    // and lossy UTF-8 may take three bytes; include URI prefixes/separators.
    let mut encoded = 4usize;
    for path in paths {
        retained = retained.checked_add(path.capacity())?;
        encoded = encoded.checked_add(path.as_os_str().len().checked_mul(9)?.checked_add(18)?)?;
    }
    (retained <= MAX_FILE_BYTES && encoded <= MAX_FILE_BYTES).then_some(retained)
}
pub(super) fn copied_bytes(value: &CopiedClipboard) -> Option<usize> {
    match value {
        CopiedClipboard::Text(v) => (v.capacity() <= MAX_TEXT_BYTES).then_some(v.capacity()),
        CopiedClipboard::Png(v) => (v.capacity() <= MAX_IMAGE_BYTES).then_some(v.capacity()),
        CopiedClipboard::Files(v) => paths_bytes(v),
    }
}
pub(super) fn request_bytes(value: &RemoteServe) -> Option<usize> {
    match value {
        RemoteServe::Text(v) => (v.capacity() <= MAX_TEXT_BYTES).then_some(v.capacity()),
        RemoteServe::FulfilPng { png, .. } => {
            (png.capacity() <= MAX_IMAGE_BYTES).then_some(png.capacity())
        }
        RemoteServe::FulfilFiles { paths, .. } => paths_bytes(paths),
        RemoteServe::Announce { .. } | RemoteServe::Abandon { .. } => Some(0),
    }
}

#[derive(Default)]
struct LatestState {
    value: Option<CopiedClipboard>,
    closed: bool,
}
#[derive(Default)]
struct Latest {
    state: Mutex<LatestState>,
    changed: Condvar,
}
pub(super) struct CaptureSender(Arc<Latest>);
pub(super) struct CaptureReceiver(Arc<Latest>);
pub(super) fn capture_channel() -> (CaptureSender, CaptureReceiver) {
    let state = Arc::new(Latest::default());
    (CaptureSender(state.clone()), CaptureReceiver(state))
}
impl CaptureSender {
    pub(super) fn send(&self, value: CopiedClipboard) -> bool {
        let mut state = lock(&self.0.state);
        if state.closed {
            return false;
        }
        if copied_bytes(&value).is_none() {
            state.value = None;
            state.closed = true;
            self.0.changed.notify_all();
            return false;
        }
        state.value = Some(value);
        self.0.changed.notify_one();
        true
    }
}
impl Drop for CaptureSender {
    fn drop(&mut self) {
        lock(&self.0.state).closed = true;
        self.0.changed.notify_all();
    }
}
impl Drop for CaptureReceiver {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.closed = true;
        state.value = None;
        self.0.changed.notify_all();
    }
}
impl CaptureReceiver {
    pub(super) fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Option<CopiedClipboard>, std::sync::mpsc::RecvError> {
        let state = lock(&self.0.state);
        let (mut state, _) = self
            .0
            .changed
            .wait_timeout_while(state, timeout, |s| s.value.is_none() && !s.closed)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(value) = state.value.take() {
            Ok(Some(value))
        } else if state.closed {
            Err(std::sync::mpsc::RecvError)
        } else {
            Ok(None)
        }
    }
}

#[derive(Default)]
struct QueueState {
    queue: VecDeque<(RemoteServe, usize)>,
    bytes: usize,
    aborted: bool,
    closed: bool,
    finished: bool,
}
#[derive(Default)]
pub(super) struct ServeQueue(Mutex<QueueState>);
impl ServeQueue {
    pub(super) fn send(&self, value: RemoteServe) -> bool {
        let bytes = request_bytes(&value);
        let mut state = lock(&self.0);
        if state.closed || state.aborted {
            return false;
        }
        let Some(bytes) = bytes.filter(|bytes| {
            state.queue.len() < MAX_COMMANDS && *bytes <= MAX_RETAINED_BYTES - state.bytes
        }) else {
            Self::abort_locked(&mut state);
            return false;
        };
        state.bytes += bytes;
        state.queue.push_back((value, bytes));
        true
    }
    fn abort_locked(state: &mut QueueState) {
        state.aborted = true;
        state.queue.clear();
        state.bytes = 0;
    }
    pub(super) fn abort(&self) {
        Self::abort_locked(&mut lock(&self.0));
    }
    pub(super) fn aborted(&self) -> bool {
        lock(&self.0).aborted
    }
    pub(super) fn pop(&self) -> Option<RemoteServe> {
        let mut state = lock(&self.0);
        let (value, bytes) = state.queue.pop_front()?;
        state.bytes -= bytes;
        Some(value)
    }
    pub(super) fn has_backlog(&self) -> bool {
        !lock(&self.0).queue.is_empty()
    }
    pub(super) fn close(&self) {
        let mut state = lock(&self.0);
        if !state.finished {
            Self::abort_locked(&mut state);
        }
        state.closed = true;
    }
    pub(super) fn close_sender(&self) {
        lock(&self.0).closed = true;
    }
    pub(super) fn close_normally(&self) -> bool {
        let mut state = lock(&self.0);
        state.queue.clear();
        state.bytes = 0;
        state.closed = true;
        state.finished = !state.aborted;
        state.finished
    }
}

pub(super) struct ServeReceiver(pub(super) Arc<ServeQueue>);
impl Drop for ServeReceiver {
    fn drop(&mut self) {
        self.0.close();
    }
}
pub(super) struct ServeProducer {
    pub(super) queue: Arc<ServeQueue>,
    pub(super) wake: crate::backend::update_notifier::AsyncUpdateNotifier,
}
impl Drop for ServeProducer {
    fn drop(&mut self) {
        // Normal sync-worker exit drops the last setter before explicit
        // shutdown. Preserve admitted work for the final bounded drain.
        self.queue.close_sender();
        self.wake.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latest_replaces_and_closes_after_last_value() {
        let (sender, receiver) = capture_channel();
        for i in 0..1000 {
            assert!(sender.send(CopiedClipboard::Text(i.to_string())));
        }
        drop(sender);
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO).unwrap(),
            Some(CopiedClipboard::Text("999".into()))
        );
        assert!(receiver.recv_timeout(Duration::ZERO).is_err());
    }
    #[test]
    fn dropping_capture_consumer_rejects_producer() {
        let (sender, receiver) = capture_channel();
        drop(receiver);
        assert!(!sender.send(CopiedClipboard::Text("gone".into())));
    }
    #[test]
    fn full_queue_reserves_sticky_abort_and_drops_all_payloads() {
        let queue = ServeQueue::default();
        for token in 0..MAX_COMMANDS {
            assert!(queue.send(RemoteServe::Abandon {
                token: token as u64
            }));
        }
        assert!(!queue.send(RemoteServe::Abandon { token: 100 }));
        assert!(queue.aborted());
        assert!(queue.pop().is_none());
        assert_eq!(lock(&queue.0).bytes, 0);
        assert!(!queue.send(RemoteServe::Text("never revive".into())));
    }
    #[test]
    fn fifo_budget_leaves_backlog_for_next_pass() {
        let queue = ServeQueue::default();
        for token in 0..(SERVE_BUDGET + 1) {
            assert!(queue.send(RemoteServe::Abandon {
                token: token as u64
            }));
        }
        for token in 0..SERVE_BUDGET {
            assert!(
                matches!(queue.pop(), Some(RemoteServe::Abandon { token: got }) if got == token as u64)
            );
        }
        assert!(queue.has_backlog());
        assert!(
            matches!(queue.pop(), Some(RemoteServe::Abandon { token }) if token == SERVE_BUDGET as u64)
        );
        assert!(!queue.has_backlog());
    }
    #[test]
    fn allocation_capacity_and_encoded_paths_are_bounded() {
        let queue = ServeQueue::default();
        assert!(!queue.send(RemoteServe::Text(String::with_capacity(MAX_TEXT_BYTES + 1))));
        let paths = vec![std::path::PathBuf::from(format!(
            "/{}",
            " ".repeat(MAX_FILE_BYTES / 8)
        ))];
        assert!(paths_bytes(&paths).is_none());
        let mut paths = Vec::with_capacity(MAX_FILE_LIST_ITEMS + 1);
        paths.push(std::path::PathBuf::from("/a"));
        assert!(paths_bytes(&paths).is_none());
    }
    #[test]
    fn byte_saturation_and_receiver_drop_close_queue() {
        let queue = Arc::new(ServeQueue::default());
        // Exercise the byte boundary without allocating half a gigabyte.
        lock(&queue.0).bytes = MAX_RETAINED_BYTES;
        assert!(!queue.send(RemoteServe::Text("x".into())));
        assert!(queue.aborted());
        let queue = Arc::new(ServeQueue::default());
        drop(ServeReceiver(queue.clone()));
        assert!(!queue.send(RemoteServe::Abandon { token: 1 }));
    }
    #[test]
    fn producer_panic_wakes_capture_waiter() {
        let (sender, receiver) = capture_channel();
        let worker = std::thread::spawn(move || {
            let _sender = sender;
            panic!("simulated watcher panic");
        });
        assert!(receiver.recv_timeout(Duration::from_secs(2)).is_err());
        assert!(worker.join().is_err());
    }
    #[test]
    fn normal_close_is_not_reported_as_abnormal_failure() {
        let queue = Arc::new(ServeQueue::default());
        let receiver = ServeReceiver(queue.clone());
        queue.close_normally();
        drop(receiver);
        assert!(!queue.aborted());
        assert!(!queue.send(RemoteServe::Abandon { token: 1 }));
    }
    #[test]
    fn normal_producer_drop_preserves_admitted_commands_for_shutdown() {
        let queue = Arc::new(ServeQueue::default());
        let producer = ServeProducer {
            queue: queue.clone(),
            wake: crate::backend::update_notifier::AsyncUpdateNotifier::new().unwrap(),
        };
        assert!(queue.send(RemoteServe::Announce {
            token: 1,
            kind: super::super::PendingKind::Png
        }));
        assert!(queue.send(RemoteServe::FulfilPng {
            token: 1,
            png: vec![1, 2, 3]
        }));
        drop(producer);
        assert!(!queue.aborted());
        assert!(!queue.send(RemoteServe::Abandon { token: 1 }));
        assert!(matches!(
            queue.pop(),
            Some(RemoteServe::Announce { token: 1, .. })
        ));
        assert!(matches!(
            queue.pop(),
            Some(RemoteServe::FulfilPng { token: 1, .. })
        ));
        assert!(!queue.has_backlog());
    }
    #[test]
    fn abort_during_handoff_cannot_be_marked_clean() {
        let queue = Arc::new(ServeQueue::default());
        let receiver = ServeReceiver(queue.clone());
        queue.close_sender();
        queue.abort();
        assert!(!queue.close_normally());
        drop(receiver);
        assert!(queue.aborted());
        assert!(lock(&queue.0).closed);
        assert!(!lock(&queue.0).finished);
    }
}
