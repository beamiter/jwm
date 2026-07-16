//! Framework-neutral wake threads for native event loops.
//!
//! These adapters deliberately know nothing about winit, tao, or any other
//! event-loop type. A frontend moves its proxy into a closure and reports
//! whether sending the wake event succeeded.

use std::io;
use std::os::fd::AsRawFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::SharedEventNotifier;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const NOTIFIER_POLL_SLICE_MS: libc::c_int = 50;

/// Conversion used by wake closures to report whether their event loop is
/// still accepting events.
///
/// `true` and `Ok(())` continue the worker. `false` and `Err(_)` stop it as an
/// orderly [`WakeWorkerStatus::EventLoopClosed`] transition.
pub trait IntoWakeResult {
    fn event_loop_is_open(self) -> bool;
}

impl IntoWakeResult for bool {
    fn event_loop_is_open(self) -> bool {
        self
    }
}

impl<E> IntoWakeResult for Result<(), E> {
    fn event_loop_is_open(self) -> bool {
        self.is_ok()
    }
}

/// Last observable state of a wake worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeWorkerStatus {
    Running,
    Shutdown,
    EventLoopClosed,
    Failed {
        operation: &'static str,
        kind: io::ErrorKind,
        message: String,
    },
    Panicked,
}

impl WakeWorkerStatus {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug)]
struct WorkerState {
    stop: bool,
    pending: bool,
    status: WakeWorkerStatus,
}

#[derive(Debug)]
struct WorkerControl {
    state: Mutex<WorkerState>,
    changed: Condvar,
}

impl WorkerControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(WorkerState {
                stop: false,
                pending: false,
                status: WakeWorkerStatus::Running,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, WorkerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn request_shutdown(&self) {
        let mut state = self.lock();
        state.stop = true;
        state.pending = false;
        self.changed.notify_all();
    }

    fn stop_requested(&self) -> bool {
        self.lock().stop
    }

    fn status(&self) -> WakeWorkerStatus {
        self.lock().status.clone()
    }

    fn finish(&self, status: WakeWorkerStatus) {
        let mut state = self.lock();
        state.stop = true;
        state.pending = false;
        state.status = status;
        self.changed.notify_all();
    }

    /// Wait for `duration`, returning `true` when shutdown interrupted the
    /// wait and `false` when the complete duration elapsed.
    fn wait_for_shutdown(&self, duration: Duration) -> bool {
        let start = Instant::now();
        let deadline = start.checked_add(duration).unwrap_or(start);
        let mut state = self.lock();
        while !state.stop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            if remaining.is_zero() {
                return false;
            }
            let waited = self.changed.wait_timeout(state, remaining);
            let (next_state, timeout) = waited.unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next_state;
            if timeout.timed_out() {
                return state.stop;
            }
        }
        true
    }

    fn begin_pending(&self) -> bool {
        let mut state = self.lock();
        if state.stop || state.pending {
            return false;
        }
        state.pending = true;
        true
    }

    fn clear_pending(&self) {
        let mut state = self.lock();
        state.pending = false;
        self.changed.notify_all();
    }

    fn has_pending(&self) -> bool {
        self.lock().pending
    }

    /// Wait until the outstanding wake is acknowledged. Returns `true` when
    /// shutdown was requested instead.
    fn wait_for_ack_or_shutdown(&self) -> bool {
        let mut state = self.lock();
        while state.pending && !state.stop {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.stop
    }
}

/// RAII thread that wakes an event loop at the next wall-clock second
/// boundary and every wall-clock second thereafter.
///
/// Waiting uses a condition variable, so dropping the handle interrupts the
/// current wait immediately instead of blocking until the next second.
#[derive(Debug)]
pub struct AlignedWakeThread {
    control: Arc<WorkerControl>,
    worker: Option<JoinHandle<()>>,
}

impl AlignedWakeThread {
    pub fn spawn<F, R>(wake: F) -> io::Result<Self>
    where
        F: FnMut() -> R + Send + 'static,
        R: IntoWakeResult,
    {
        let control = Arc::new(WorkerControl::new());
        let worker_control = Arc::clone(&control);
        let worker = thread::Builder::new()
            .name("xbar-aligned-wake".into())
            .spawn(move || {
                let status = catch_unwind(AssertUnwindSafe(|| {
                    aligned_wake_loop(&worker_control, wake)
                }))
                .unwrap_or(WakeWorkerStatus::Panicked);
                worker_control.finish(status);
            })?;
        Ok(Self {
            control,
            worker: Some(worker),
        })
    }

    /// Request prompt shutdown. Repeated calls are harmless.
    pub fn request_shutdown(&self) {
        self.control.request_shutdown();
    }

    #[must_use]
    pub fn status(&self) -> WakeWorkerStatus {
        self.control.status()
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for AlignedWakeThread {
    fn drop(&mut self) {
        self.request_shutdown();
        join_without_unwinding(&self.control, self.worker.take());
    }
}

fn aligned_wake_loop<F, R>(control: &WorkerControl, mut wake: F) -> WakeWorkerStatus
where
    F: FnMut() -> R,
    R: IntoWakeResult,
{
    loop {
        if control.wait_for_shutdown(until_next_wall_second()) {
            return WakeWorkerStatus::Shutdown;
        }
        if !wake().event_loop_is_open() {
            return WakeWorkerStatus::EventLoopClosed;
        }
    }
}

fn until_next_wall_second() -> Duration {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let subsecond = u64::from(elapsed.subsec_nanos());
    Duration::from_nanos(NANOS_PER_SECOND.saturating_sub(subsecond).max(1))
}

/// Acknowledgement carried by one coalesced frontend wake event.
///
/// Dropping or explicitly acknowledging this value releases the forwarder to
/// poll for another notification. It is intentionally not cloneable: one
/// accepted wake has one acknowledgement owner.
#[derive(Debug)]
pub struct WakeAck {
    control: Arc<WorkerControl>,
    released: bool,
}

impl WakeAck {
    fn new(control: Arc<WorkerControl>) -> Self {
        Self {
            control,
            released: false,
        }
    }

    /// Release the pending wake. Dropping the value has the same effect.
    pub fn ack(mut self) {
        self.release();
    }

    #[must_use]
    pub const fn is_acknowledged(&self) -> bool {
        self.released
    }

    fn release(&mut self) {
        if !self.released {
            self.control.clear_pending();
            self.released = true;
        }
    }
}

impl Drop for WakeAck {
    fn drop(&mut self) {
        self.release();
    }
}

/// RAII bridge from a [`SharedEventNotifier`] to a framework event-loop wake
/// closure.
///
/// The worker handles Linux poll interruption and terminal descriptor states,
/// drains eventfd, and allows at most one unacknowledged frontend event. The
/// notifier is shared only to let `Drop` request both worker shutdowns at
/// once; ownership remains entirely inside this handle.
#[derive(Debug)]
pub struct CoalescedNotifierForwarder {
    control: Arc<WorkerControl>,
    notifier: Arc<SharedEventNotifier>,
    worker: Option<JoinHandle<()>>,
}

impl CoalescedNotifierForwarder {
    pub fn spawn<F, R>(notifier: SharedEventNotifier, wake: F) -> io::Result<Self>
    where
        F: FnMut(WakeAck) -> R + Send + 'static,
        R: IntoWakeResult,
    {
        let control = Arc::new(WorkerControl::new());
        let notifier = Arc::new(notifier);
        let worker_control = Arc::clone(&control);
        let worker_notifier = Arc::clone(&notifier);
        let worker = thread::Builder::new()
            .name("xbar-notifier-forwarder".into())
            .spawn(move || {
                let status = catch_unwind(AssertUnwindSafe(|| {
                    notifier_forward_loop(&worker_control, &worker_notifier, wake)
                }))
                .unwrap_or(WakeWorkerStatus::Panicked);
                worker_notifier.request_shutdown();
                worker_control.finish(status);
            })?;
        Ok(Self {
            control,
            notifier,
            worker: Some(worker),
        })
    }

    /// Request prompt shutdown of both the forwarding worker and the notifier
    /// worker. Repeated calls are harmless.
    pub fn request_shutdown(&self) {
        self.control.request_shutdown();
        self.notifier.request_shutdown();
    }

    #[must_use]
    pub fn status(&self) -> WakeWorkerStatus {
        self.control.status()
    }

    #[must_use]
    pub fn has_pending_wake(&self) -> bool {
        self.control.has_pending()
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for CoalescedNotifierForwarder {
    fn drop(&mut self) {
        self.request_shutdown();
        join_without_unwinding(&self.control, self.worker.take());
    }
}

/// Result of synchronizing a [`TransportWakeSlot`] with a runtime transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportWakeChange {
    Unchanged,
    Removed { generation: u64 },
    Replaced { generation: u64 },
}

impl TransportWakeChange {
    /// The newly observed generation when synchronization changed the slot.
    #[must_use]
    pub const fn generation(self) -> Option<u64> {
        match self {
            Self::Unchanged => None,
            Self::Removed { generation } | Self::Replaced { generation } => Some(generation),
        }
    }
}

/// Reconnect-aware owner for a coalesced transport wake forwarder.
///
/// Frontends call [`Self::sync`] after servicing [`crate::BarRuntime`] and no
/// longer need to track transport generations themselves. A replacement is
/// fully constructed before the previous forwarder is dropped. Consequently,
/// eventfd or thread creation failure leaves the old forwarder and generation
/// unchanged, and the next call retries the same runtime generation.
#[derive(Debug)]
pub struct TransportWakeSlot {
    generation: u64,
    forwarder: Option<CoalescedNotifierForwarder>,
    non_blocking: bool,
}

impl TransportWakeSlot {
    #[must_use]
    pub const fn new(non_blocking: bool) -> Self {
        Self {
            generation: 0,
            forwarder: None,
            non_blocking,
        }
    }

    /// Synchronize the forwarding worker with the runtime's current transport.
    ///
    /// `wake` is moved into a new worker only for `Replaced`; it is simply
    /// dropped for `Unchanged` or `Removed`.
    pub fn sync<F, R>(
        &mut self,
        runtime: &crate::BarRuntime,
        wake: F,
    ) -> io::Result<TransportWakeChange>
    where
        F: FnMut(WakeAck) -> R + Send + 'static,
        R: IntoWakeResult,
    {
        self.sync_with(runtime, wake, |transport, non_blocking, wake| {
            let notifier = transport.notifier(non_blocking)?;
            CoalescedNotifierForwarder::spawn(notifier, wake)
        })
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn forwarder(&self) -> Option<&CoalescedNotifierForwarder> {
        self.forwarder.as_ref()
    }

    #[must_use]
    pub const fn non_blocking(&self) -> bool {
        self.non_blocking
    }

    fn sync_with<F, R, C>(
        &mut self,
        runtime: &crate::BarRuntime,
        wake: F,
        create: C,
    ) -> io::Result<TransportWakeChange>
    where
        F: FnMut(WakeAck) -> R + Send + 'static,
        R: IntoWakeResult,
        C: FnOnce(&crate::SharedTransport, bool, F) -> io::Result<CoalescedNotifierForwarder>,
    {
        let generation = runtime.transport_generation();
        let transport = runtime.transport();
        let state_matches =
            generation == self.generation && transport.is_some() == self.forwarder.is_some();
        if state_matches {
            return Ok(TransportWakeChange::Unchanged);
        }

        let Some(transport) = transport else {
            self.forwarder = None;
            self.generation = generation;
            return Ok(TransportWakeChange::Removed { generation });
        };

        let replacement = create(transport, self.non_blocking, wake)?;
        self.forwarder = Some(replacement);
        self.generation = generation;
        Ok(TransportWakeChange::Replaced { generation })
    }
}

fn notifier_forward_loop<F, R>(
    control: &Arc<WorkerControl>,
    notifier: &SharedEventNotifier,
    mut wake: F,
) -> WakeWorkerStatus
where
    F: FnMut(WakeAck) -> R,
    R: IntoWakeResult,
{
    let mut descriptor = libc::pollfd {
        fd: notifier.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    loop {
        if control.stop_requested() {
            return WakeWorkerStatus::Shutdown;
        }
        descriptor.revents = 0;
        let ready = unsafe { libc::poll(&mut descriptor, 1, NOTIFIER_POLL_SLICE_MS) };
        if control.stop_requested() {
            return WakeWorkerStatus::Shutdown;
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return wake_failure("poll", error);
        }
        if ready == 0 {
            continue;
        }

        let terminal = descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL);
        if terminal != 0 {
            return WakeWorkerStatus::Failed {
                operation: "poll",
                kind: io::ErrorKind::BrokenPipe,
                message: format!("notifier descriptor became unusable: revents={terminal}"),
            };
        }
        if descriptor.revents & libc::POLLIN == 0 {
            continue;
        }

        match notifier.drain() {
            Ok(0) => continue,
            Ok(_) => {}
            Err(error) => return wake_failure("drain", error),
        }
        if !control.begin_pending() {
            continue;
        }

        let acknowledged = WakeAck::new(Arc::clone(control));
        if !wake(acknowledged).event_loop_is_open() {
            control.clear_pending();
            return WakeWorkerStatus::EventLoopClosed;
        }
        if control.wait_for_ack_or_shutdown() {
            return WakeWorkerStatus::Shutdown;
        }
    }
}

fn wake_failure(operation: &'static str, error: io::Error) -> WakeWorkerStatus {
    WakeWorkerStatus::Failed {
        operation,
        kind: error.kind(),
        message: error.to_string(),
    }
}

fn join_without_unwinding(control: &WorkerControl, worker: Option<JoinHandle<()>>) {
    if worker.is_some_and(|worker| worker.join().is_err()) {
        control.finish(WakeWorkerStatus::Panicked);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use shared_structures::{SharedMessage, SharedRingBuffer};

    static NEXT_TEST_PATH: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn aligned_worker_stops_promptly_and_observes_a_closed_loop() {
        let started = Instant::now();
        drop(AlignedWakeThread::spawn(|| true).unwrap());
        assert!(started.elapsed() < Duration::from_millis(250));

        let closed = AlignedWakeThread::spawn(|| false).unwrap();
        wait_for_status(&closed.control, WakeWorkerStatus::EventLoopClosed);
        assert_eq!(closed.status(), WakeWorkerStatus::EventLoopClosed);
    }

    #[test]
    fn notifier_forwarder_coalesces_until_ack_drop() {
        let (owner, notifier) = test_notifier("coalesce");
        let (sender, receiver) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let forwarder = CoalescedNotifierForwarder::spawn(notifier, move |ack| {
            worker_calls.fetch_add(1, Ordering::Relaxed);
            sender.send(ack).is_ok()
        })
        .unwrap();

        assert!(owner.try_write_message(&SharedMessage::default()).unwrap());
        let ack = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(forwarder.has_pending_wake());
        for _ in 0..3 {
            assert!(owner.try_write_message(&SharedMessage::default()).unwrap());
        }
        thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(receiver.try_recv().is_err());

        assert!(owner.try_read_latest_message().unwrap().is_some());
        drop(ack);
        let deadline = Instant::now() + Duration::from_secs(1);
        while forwarder.has_pending_wake() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(!forwarder.has_pending_wake());

        drop(forwarder);
        owner.destroy().unwrap();
    }

    #[test]
    fn notifier_forwarder_stops_for_closed_loop_and_drop_is_bounded() {
        let (owner, notifier) = test_notifier("closed");
        let forwarder =
            CoalescedNotifierForwarder::spawn(notifier, |_ack| Err::<(), _>(())).unwrap();
        assert!(owner.try_write_message(&SharedMessage::default()).unwrap());
        wait_for_status(&forwarder.control, WakeWorkerStatus::EventLoopClosed);
        assert_eq!(forwarder.status(), WakeWorkerStatus::EventLoopClosed);

        let started = Instant::now();
        drop(forwarder);
        assert!(started.elapsed() < Duration::from_millis(300));
        owner.destroy().unwrap();
    }

    #[test]
    fn notifier_forwarder_drop_interrupts_an_unacknowledged_wake() {
        let (owner, notifier) = test_notifier("pending-drop");
        let (sender, receiver) = mpsc::channel();
        let forwarder =
            CoalescedNotifierForwarder::spawn(notifier, move |ack| sender.send(ack).is_ok())
                .unwrap();

        assert!(owner.try_write_message(&SharedMessage::default()).unwrap());
        let ack = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(forwarder.has_pending_wake());

        let started = Instant::now();
        drop(forwarder);
        assert!(started.elapsed() < Duration::from_millis(300));
        drop(ack);
        owner.destroy().unwrap();
    }

    #[test]
    fn transport_wake_slot_tracks_disconnect_and_reconnect() {
        let (path, owner) = test_ring("transport-slot");
        let mut runtime = crate::BarRuntime::default();
        let mut slot = TransportWakeSlot::new(true);

        assert!(slot.non_blocking());
        assert_eq!(
            slot.sync(&runtime, |_ack| true).unwrap(),
            TransportWakeChange::Unchanged
        );
        assert!(slot.forwarder().is_none());

        runtime.set_transport(Some(crate::SharedTransport::open(&path).unwrap()));
        let connected_generation = runtime.transport_generation();
        let (sender, receiver) = mpsc::channel();
        assert_eq!(
            slot.sync(&runtime, move |ack| sender.send(ack).is_ok())
                .unwrap(),
            TransportWakeChange::Replaced {
                generation: connected_generation
            }
        );
        assert_eq!(slot.generation(), connected_generation);
        assert!(slot.forwarder().is_some());

        assert!(owner.try_write_message(&SharedMessage::default()).unwrap());
        let ack = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(owner.try_read_latest_message().unwrap().is_some());
        drop(ack);

        runtime.set_transport(None);
        let disconnected_generation = runtime.transport_generation();
        let removed = slot.sync(&runtime, |_ack| true).unwrap();
        assert_eq!(removed.generation(), Some(disconnected_generation));
        assert!(matches!(removed, TransportWakeChange::Removed { .. }));
        assert_eq!(slot.generation(), disconnected_generation);
        assert!(slot.forwarder().is_none());

        runtime.set_transport(Some(crate::SharedTransport::open(&path).unwrap()));
        let reconnected_generation = runtime.transport_generation();
        let (sender, receiver) = mpsc::channel();
        assert_eq!(
            slot.sync(&runtime, move |ack| sender.send(ack).is_ok())
                .unwrap(),
            TransportWakeChange::Replaced {
                generation: reconnected_generation
            }
        );
        assert!(owner.try_write_message(&SharedMessage::default()).unwrap());
        let ack = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(owner.try_read_latest_message().unwrap().is_some());
        drop(ack);

        drop(slot);
        drop(runtime);
        owner.destroy().unwrap();
    }

    #[test]
    fn transport_wake_slot_preserves_state_after_creation_failure() {
        let (path, owner) = test_ring("transport-slot-failure");
        let initial = crate::SharedTransport::open(&path).unwrap();
        let mut runtime =
            crate::BarRuntime::with_transport(crate::ModelConfig::default(), Some(initial))
                .unwrap();
        let mut slot = TransportWakeSlot::new(true);
        slot.sync(&runtime, |_ack| true).unwrap();

        let initial_generation = slot.generation();
        let initial_forwarder = slot.forwarder().unwrap() as *const CoalescedNotifierForwarder;
        runtime.set_transport(Some(crate::SharedTransport::open(&path).unwrap()));
        let replacement_generation = runtime.transport_generation();
        assert_ne!(replacement_generation, initial_generation);

        let error = slot
            .sync_with(
                &runtime,
                |_ack| true,
                |_, _, _| Err(io::Error::other("injected wake creation failure")),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(slot.generation(), initial_generation);
        assert!(std::ptr::eq(slot.forwarder().unwrap(), initial_forwarder));
        assert_eq!(
            slot.forwarder().unwrap().status(),
            WakeWorkerStatus::Running
        );

        assert_eq!(
            slot.sync(&runtime, |_ack| true).unwrap(),
            TransportWakeChange::Replaced {
                generation: replacement_generation
            }
        );
        assert_eq!(slot.generation(), replacement_generation);
        assert!(slot.forwarder().is_some());

        drop(slot);
        drop(runtime);
        owner.destroy().unwrap();
    }

    fn test_notifier(label: &str) -> (SharedRingBuffer, SharedEventNotifier) {
        let (path, owner) = test_ring(label);
        let transport = crate::SharedTransport::open(&path).unwrap();
        let notifier = transport.notifier(true).unwrap();
        (owner, notifier)
    }

    fn test_ring(label: &str) -> (String, SharedRingBuffer) {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "/tmp/xbar-core-wake-{label}-{}-{sequence}",
            std::process::id()
        );
        let owner = SharedRingBuffer::create_aux(&path, Some(8), Some(0)).unwrap();
        (path, owner)
    }

    fn wait_for_status(control: &WorkerControl, expected: WakeWorkerStatus) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.status() != expected && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(control.status(), expected);
    }
}
