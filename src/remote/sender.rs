//! The viewer queues records; exactly one worker owns the authenticated writer.
//! Limits apply before cloning payloads. Overload fails the session rather than
//! losing an input edge or silently corrupting a clipboard transfer.

use super::RemoteResult;
use super::clipboard_wire::{ClipboardRestriction, OutboundGate};
use super::messages::{MAX_CLIPBOARD_BYTES, decode_source_input, encode_source_input_into};
use super::protocol::{MessageKind, SessionWriter};
use super::x11_input::InputEvent;
use std::collections::VecDeque;
use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const CONTROL_RECORDS: usize = 128;
const CONTROL_BYTES: usize = 128 * 1024;
const BULK_RECORDS: usize = 32;
const BULK_BYTES: usize = MAX_CLIPBOARD_BYTES + 2 * 1024 * 1024;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const RECORD_TIMEOUT: Duration = Duration::from_secs(2);

/// Common record interface keeps serialization tests independent of threads.
pub(super) trait RecordWriter {
    fn write_message(&mut self, kind: MessageKind, payload: &[u8]) -> RemoteResult<()>;
    fn flush(&mut self) -> RemoteResult<()>;
    fn queued(&self) -> bool {
        false
    }
    fn close(&mut self) -> RemoteResult<()> {
        self.write_message(MessageKind::Close, &[])?;
        self.flush()
    }
}

impl<W: io::Write> RecordWriter for SessionWriter<W> {
    fn write_message(&mut self, kind: MessageKind, payload: &[u8]) -> RemoteResult<()> {
        SessionWriter::write_message(self, kind, payload)?;
        Ok(())
    }
    fn flush(&mut self) -> RemoteResult<()> {
        SessionWriter::flush(self)?;
        Ok(())
    }
}

#[derive(Debug)]
struct Record {
    kind: MessageKind,
    payload: Vec<u8>,
}

/// Test-only provenance of the actual first sender worker socket failure.
#[cfg(test)]
#[derive(Debug)]
pub(super) struct SenderShutdownTransportError {
    pub(super) kind: io::ErrorKind,
    pub(super) message: String,
}
#[cfg(test)]
impl std::fmt::Display for SenderShutdownTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
#[cfg(test)]
impl std::error::Error for SenderShutdownTransportError {}

#[cfg(test)]
fn shutdown_transport_provenance_allowed(shutdown: &io::Result<()>) -> bool {
    matches!(shutdown, Ok(()))
        || matches!(shutdown, Err(error) if error.kind() == io::ErrorKind::NotConnected)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PopupTerminal {
    Queued,
    InFlight,
    Flushed,
}

fn write_and_flush_record(
    writer: &mut impl RecordWriter,
    kind: MessageKind,
    payload: &[u8],
) -> RemoteResult<()> {
    writer.write_message(kind, payload)?;
    writer.flush()
}

#[derive(Default)]
struct Queue {
    control: VecDeque<Record>,
    bulk: VecDeque<Record>,
    control_bytes: usize,
    bulk_bytes: usize,
    closing: bool,
    popup_terminal: Option<PopupTerminal>,
    stopped: bool,
    cancelling: bool,
    error: Option<String>,
    #[cfg(test)]
    error_io_kind: Option<io::ErrorKind>,
    writing_since: Option<Instant>,
    input_scope: Option<(u64, u64)>,
    clipboard_policy: Option<Arc<AtomicU8>>,
    // One reserved FIFO barrier survives ordinary queue saturation.
    clipboard_barrier: Option<(Record, usize)>,
}

impl Queue {
    fn record_worker_error(
        &mut self,
        message: impl FnOnce() -> String,
        _io_kind: Option<io::ErrorKind>,
    ) {
        #[cfg(test)]
        if self.error.is_none() {
            self.error_io_kind = _io_kind;
        }
        self.error.get_or_insert_with(message);
    }
    fn check(&self) -> RemoteResult<()> {
        if let Some(error) = &self.error {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()).into());
        }
        if (self.stopped || self.closing) && self.popup_terminal != Some(PopupTerminal::Flushed) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "remote sender stopped").into());
        }
        Ok(())
    }

    fn push(&mut self, kind: MessageKind, payload: &[u8]) -> RemoteResult<()> {
        self.check()?;
        if self.popup_terminal.is_some() {
            return Err(
                io::Error::new(io::ErrorKind::BrokenPipe, "popup sender is terminal").into(),
            );
        }
        if kind == MessageKind::WaylandPopupRevoke {
            super::wayland_popup_wire::Binding::decode(payload)?;
            // Reserved terminal slot. Discard only unsent work; an already
            // popped record remains ordered before this one on the writer.
            self.control.clear();
            self.bulk.clear();
            self.clipboard_barrier = None;
            self.control_bytes = payload.len();
            self.bulk_bytes = 0;
            self.control.push_back(Record {
                kind,
                payload: payload.to_vec(),
            });
            self.popup_terminal = Some(PopupTerminal::Queued);
            return Ok(());
        }
        if kind == MessageKind::ClipboardRestrict {
            ClipboardRestriction::decode(payload)?;
            if self.clipboard_barrier.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a clipboard restriction is already queued",
                )
                .into());
            }
            self.clipboard_barrier = Some((
                Record {
                    kind,
                    payload: payload.to_vec(),
                },
                self.bulk.len(),
            ));
            return Ok(());
        }
        let expected = match kind {
            MessageKind::Heartbeat => Some(0),
            MessageKind::FrameAck => Some(16),
            MessageKind::Viewport => Some(4),
            MessageKind::WindowResizeRequest => Some(28),
            MessageKind::WaylandKeyboardRequest => Some(56),
            MessageKind::WaylandKeyboardKey => Some(43),
            MessageKind::WaylandKeyboardRevoke => Some(8),
            MessageKind::WaylandPointerRequest => Some(24),
            MessageKind::WaylandPointerAction => Some(45),
            MessageKind::WaylandPointerRevoke => Some(8),
            MessageKind::WaylandControlRequest => Some(56),
            MessageKind::WaylandControlAction => Some(48),
            MessageKind::WaylandControlRevoke => Some(8),
            _ => None,
        };
        if expected.is_some_and(|length| payload.len() != length) {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "invalid control record size").into(),
            );
        }
        if kind == MessageKind::WaylandControlRevoke {
            self.control.retain(|r| {
                !matches!(
                    r.kind,
                    MessageKind::WaylandControlRequest | MessageKind::WaylandControlAction
                )
            });
            self.control_bytes = self.control.iter().map(|r| r.payload.len()).sum();
        }
        if kind == MessageKind::WaylandPointerRevoke {
            self.control.retain(|r| {
                !matches!(
                    r.kind,
                    MessageKind::WaylandPointerRequest | MessageKind::WaylandPointerAction
                )
            });
            self.control_bytes = self.control.iter().map(|r| r.payload.len()).sum();
        }
        if kind == MessageKind::WaylandKeyboardRevoke {
            self.control
                .retain(|r| r.kind != MessageKind::WaylandKeyboardKey);
            self.control_bytes = self.control.iter().map(|r| r.payload.len()).sum();
        }
        // ReleaseAll is a fence: discard unsent input before the last release,
        // while preserving subsequent edges in this batch. A record already
        // being written finishes before this fence on the single writer.
        let mut fenced_payload = Vec::new();
        let payload = if kind == MessageKind::InputBatch {
            let (source, geometry, events) = decode_source_input(payload)?;
            self.input_scope = Some((source, geometry));
            if let Some(index) = events.iter().rposition(|e| *e == InputEvent::ReleaseAll) {
                self.control.retain(|r| r.kind != MessageKind::InputBatch);
                self.control_bytes = self.control.iter().map(|r| r.payload.len()).sum();
                encode_source_input_into(source, geometry, &events[index..], &mut fenced_payload)?;
                &fenced_payload
            } else {
                payload
            }
        } else {
            payload
        };
        // These records are cumulative/latest state, never input edges. Replace
        // in place, so repeated redraws/resizes cannot consume the input budget.
        if matches!(
            kind,
            MessageKind::FrameAck | MessageKind::Viewport | MessageKind::Heartbeat
        ) && let Some(record) = self.control.iter_mut().find(|r| r.kind == kind)
        {
            self.control_bytes = self.control_bytes - record.payload.len() + payload.len();
            record.payload = payload.to_vec();
            return Ok(());
        }
        let bulk = matches!(
            kind,
            MessageKind::Clipboard | MessageKind::ClipboardTransfer
        );
        let (queue, bytes, count_limit, byte_limit) = if bulk {
            (
                &mut self.bulk,
                &mut self.bulk_bytes,
                BULK_RECORDS,
                BULK_BYTES,
            )
        } else {
            (
                &mut self.control,
                &mut self.control_bytes,
                CONTROL_RECORDS,
                CONTROL_BYTES,
            )
        };
        if queue.len() >= count_limit || payload.len() > byte_limit.saturating_sub(*bytes) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "remote send queue overloaded; ending session to preserve input and clipboard ordering").into());
        }
        *bytes += payload.len();
        queue.push_back(Record {
            kind,
            payload: payload.to_vec(),
        });
        Ok(())
    }

    fn close(&mut self, input_enabled: bool) {
        if self.popup_terminal.is_some() {
            // Revoke itself is the terminal record, whether queued, currently
            // being flushed, or already completed. Never append generic Close.
            self.closing = true;
            return;
        }
        // Reserved shutdown path needs no free queue slot. Send a release
        // before Close when input was negotiated.
        self.control.clear();
        self.bulk.clear();
        self.clipboard_barrier = None;
        self.control_bytes = 0;
        self.bulk_bytes = 0;
        if input_enabled {
            let mut release = Vec::new();
            encode_source_input_into(
                self.input_scope.unwrap_or((1, 1)).0,
                self.input_scope.unwrap_or((1, 1)).1,
                &[InputEvent::ReleaseAll],
                &mut release,
            )
            .expect("a single release is a valid input batch");
            self.control_bytes = release.len();
            self.control.push_back(Record {
                kind: MessageKind::InputBatch,
                payload: release,
            });
        }
        self.control.push_back(Record {
            kind: MessageKind::Close,
            payload: Vec::new(),
        });
        self.closing = true;
    }

    fn record_flushed(&mut self, kind: MessageKind) {
        self.writing_since = None;
        if kind == MessageKind::WaylandPopupRevoke {
            self.popup_terminal = Some(PopupTerminal::Flushed);
        }
    }

    fn pop(&mut self) -> Option<Record> {
        if let Some(record) = self.control.pop_front() {
            self.control_bytes -= record.payload.len();
            if record.kind == MessageKind::WaylandPopupRevoke {
                self.popup_terminal = Some(PopupTerminal::InFlight);
            }
            Some(record)
        } else if self
            .clipboard_barrier
            .as_ref()
            .is_some_and(|(_, preceding)| *preceding == 0)
        {
            self.clipboard_barrier.take().map(|(record, _)| record)
        } else if let Some(record) = self.bulk.pop_front() {
            self.bulk_bytes -= record.payload.len();
            if let Some((_, preceding)) = self.clipboard_barrier.as_mut() {
                *preceding -= 1;
            }
            Some(record)
        } else {
            None
        }
    }
}

pub(super) struct Sender {
    shared: Arc<(Mutex<Queue>, Condvar)>,
    control: TcpStream,
    #[cfg(test)]
    done: mpsc::Receiver<()>,
    worker: Option<thread::JoinHandle<()>>,
    joining: Option<mpsc::Receiver<bool>>,
    join_failure: Option<String>,
    input_enabled: bool,
}

/// Join off the foreground thread: even thread-local destructors may block.
/// The one reaper per worker remains detached on timeout; no retry is implied.
pub(super) fn reap_worker(worker: thread::JoinHandle<()>) -> io::Result<mpsc::Receiver<bool>> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("jwm-remote-reaper".into())
        .spawn(move || {
            let _ = tx.send(worker.join().is_ok());
        })?;
    Ok(rx)
}

impl Sender {
    /// Install before queueing clipboard data. The shared mask only narrows;
    /// final dispatch loads it after dequeue, immediately before serialization.
    pub(super) fn set_clipboard_policy(&mut self, policy: Arc<AtomicU8>) -> RemoteResult<()> {
        let mut queue = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
        queue.check()?;
        if queue.clipboard_policy.is_some() || !queue.bulk.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "clipboard dispatch policy must be installed once before data",
            )
            .into());
        }
        queue.clipboard_policy = Some(policy);
        Ok(())
    }

    pub(super) fn shutdown_timeout(&mut self, timeout: Duration) -> RemoteResult<()> {
        {
            let mut queue = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
            queue.cancelling = true;
            queue.stopped = true;
        }
        self.shared.1.notify_all();
        let shutdown = self.control.shutdown(Shutdown::Both);
        self.join_timeout(timeout)?;
        {
            let queue = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(error) = &queue.error {
                #[cfg(test)]
                if let Some(kind @ (io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset)) =
                    queue
                        .error_io_kind
                        .filter(|_| shutdown_transport_provenance_allowed(&shutdown))
                {
                    return Err(Box::new(SenderShutdownTransportError {
                        kind,
                        message: error.clone(),
                    }));
                }
                return Err(io::Error::other(error.clone()).into());
            }
        }
        match shutdown {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn join_timeout(&mut self, timeout: Duration) -> RemoteResult<()> {
        if let Some(error) = &self.join_failure {
            return Err(io::Error::other(error.clone()).into());
        }
        if let Some(worker) = self.worker.take() {
            match reap_worker(worker) {
                Ok(joining) => self.joining = Some(joining),
                Err(error) => {
                    let message = format!("start sender reaper (worker detached): {error}");
                    self.join_failure = Some(message.clone());
                    return Err(io::Error::other(message).into());
                }
            }
        }
        if let Some(joining) = self.joining.as_ref() {
            match joining.recv_timeout(timeout) {
                Ok(true) => {
                    self.joining = None;
                }
                Ok(false) => {
                    self.joining = None;
                    self.join_failure = Some("remote sender panicked".into());
                    return Err(io::Error::other("remote sender panicked").into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "remote sender join timed out (reaper may remain detached)",
                    )
                    .into());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("remote sender reaper disconnected").into());
                }
            }
        }
        Ok(())
    }

    pub(super) fn spawn(
        mut writer: SessionWriter<TcpStream>,
        input_enabled: bool,
        on_written: impl Fn(MessageKind) + Send + 'static,
    ) -> io::Result<Self> {
        let control = writer.get_ref().try_clone()?;
        let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let worker_shared = Arc::clone(&shared);
        let (done_tx, _done) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("jwm-remote-send".into())
            .spawn(move || {
                let watchdog = writer.get_ref().try_clone().and_then(|socket| {
                    let shared = Arc::clone(&worker_shared);
                    thread::Builder::new()
                        .name("jwm-remote-send-deadline".into())
                        .spawn(move || {
                            let (lock, wake) = &*shared;
                            let mut queue = lock.lock().unwrap_or_else(|p| p.into_inner());
                            loop {
                                if queue.stopped {
                                    return;
                                }
                                if let Some(started) = queue.writing_since {
                                    let remaining =
                                        RECORD_TIMEOUT.saturating_sub(started.elapsed());
                                    if remaining.is_zero() {
                                        queue.error =
                                            Some("remote record write deadline exceeded".into());
                                        #[cfg(test)]
                                        {
                                            queue.error_io_kind = None;
                                        }
                                        queue.stopped = true;
                                        drop(queue);
                                        let _ = socket.shutdown(Shutdown::Both);
                                        wake.notify_all();
                                        return;
                                    }
                                    queue = wake
                                        .wait_timeout(queue, remaining)
                                        .unwrap_or_else(|p| p.into_inner())
                                        .0;
                                } else {
                                    queue = wake.wait(queue).unwrap_or_else(|p| p.into_inner());
                                }
                            }
                        })
                });
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> RemoteResult<()> {
                        if let Err(error) = &watchdog {
                            return Err(io::Error::other(format!(
                                "start sender deadline: {error}"
                            ))
                            .into());
                        }
                        let mut clipboard_gate = OutboundGate::default();
                        loop {
                            let (record, clipboard_policy) = {
                                let (lock, wake) = &*worker_shared;
                                let mut queue = lock.lock().unwrap_or_else(|p| p.into_inner());
                                loop {
                                    if queue.stopped {
                                        return Ok(());
                                    }
                                    if let Some(record) = queue.pop() {
                                        queue.writing_since = Some(Instant::now());
                                        wake.notify_all();
                                        break (record, queue.clipboard_policy.clone());
                                    }
                                    queue = wake.wait(queue).unwrap_or_else(|p| p.into_inner());
                                }
                            };
                            let payload = match clipboard_policy {
                                Some(policy) => clipboard_gate.filter(
                                    record.kind,
                                    &record.payload,
                                    policy.load(Ordering::Acquire),
                                )?,
                                None => Some(record.payload),
                            };
                            if let Some(payload) = &payload {
                                write_and_flush_record(&mut writer, record.kind, payload)?;
                            }
                            {
                                let mut queue =
                                    worker_shared.0.lock().unwrap_or_else(|p| p.into_inner());
                                queue.writing_since = None;
                                if payload.is_some() {
                                    queue.record_flushed(record.kind);
                                }
                            }
                            worker_shared.1.notify_all();
                            if payload.is_some() {
                                on_written(record.kind);
                            }
                            if matches!(
                                record.kind,
                                MessageKind::Close | MessageKind::WaylandPopupRevoke
                            ) {
                                return Ok(());
                            }
                        }
                    },
                ))
                .unwrap_or_else(|_| Err(io::Error::other("remote sender panicked").into()));
                let mut queue = worker_shared.0.lock().unwrap_or_else(|p| p.into_inner());
                queue.stopped = true;
                if let Err(error) = result {
                    // Only an interrupted socket operation after explicit abort
                    // is expected. Panics/deadlines/protocol failures survive.
                    let io_error = error.downcast_ref::<io::Error>().or_else(|| {
                        match error.downcast_ref::<super::protocol::ProtocolError>() {
                            Some(super::protocol::ProtocolError::Io(error)) => Some(error),
                            _ => None,
                        }
                    });
                    let cancelled_io = queue.cancelling
                        && queue.popup_terminal.is_none()
                        && io_error.is_some_and(|error| {
                            matches!(
                                error.kind(),
                                io::ErrorKind::BrokenPipe
                                    | io::ErrorKind::ConnectionReset
                                    | io::ErrorKind::ConnectionAborted
                                    | io::ErrorKind::NotConnected
                            )
                        });
                    if !cancelled_io {
                        queue.record_worker_error(
                            || error.to_string(),
                            io_error.map(io::Error::kind),
                        );
                    }
                }
                let popup_flushed =
                    queue.popup_terminal == Some(PopupTerminal::Flushed) && queue.error.is_none();
                drop(queue);
                worker_shared.1.notify_all();
                if let Ok(watchdog) = watchdog {
                    let _ = watchdog.join();
                }
                let _ = writer.get_ref().shutdown(if popup_flushed {
                    Shutdown::Write
                } else {
                    Shutdown::Both
                });
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            shared,
            control,
            #[cfg(test)]
            done: _done,
            worker: Some(worker),
            joining: None,
            join_failure: None,
            input_enabled,
        })
    }
}

impl RecordWriter for Sender {
    fn queued(&self) -> bool {
        true
    }
    fn write_message(&mut self, kind: MessageKind, payload: &[u8]) -> RemoteResult<()> {
        let result = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(kind, payload);
        self.shared.1.notify_all();
        result
    }
    fn flush(&mut self) -> RemoteResult<()> {
        // Enqueue is the UI boundary, not a socket flush or worker rendezvous.
        self.shared
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .check()
    }
    fn close(&mut self) -> RemoteResult<()> {
        {
            let mut queue = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
            queue.check()?;
            queue.close(self.input_enabled);
        }
        self.shared.1.notify_all();
        if let Err(error) = self.join_timeout(CLOSE_TIMEOUT) {
            let _ = self.control.shutdown(Shutdown::Both);
            return Err(error);
        }
        let queue = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
        match &queue.error {
            Some(error) => Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()).into()),
            None => Ok(()),
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped = true;
        self.shared.1.notify_all();
        let _ = self.control.shutdown(Shutdown::Both);
        // Shutdown interrupts an in-flight write. Never make the UI join a
        // stalled worker; its owned state and socket are released on exit.
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scoped_control_revoke_clears_pending_request_and_actions() {
        let mut queue = super::Queue::default();
        queue
            .push(super::MessageKind::WaylandControlRequest, &[1; 56])
            .unwrap();
        queue
            .push(super::MessageKind::WaylandControlAction, &[1; 48])
            .unwrap();
        queue.push(super::MessageKind::Heartbeat, &[]).unwrap();
        queue
            .push(super::MessageKind::WaylandControlRevoke, &[1; 8])
            .unwrap();
        assert!(queue.control.iter().all(|r| !matches!(
            r.kind,
            super::MessageKind::WaylandControlRequest | super::MessageKind::WaylandControlAction
        )));
        assert!(
            queue
                .control
                .iter()
                .any(|r| r.kind == super::MessageKind::Heartbeat)
        );
        assert_eq!(
            queue.control.back().unwrap().kind,
            super::MessageKind::WaylandControlRevoke
        );
        assert_eq!(queue.control_bytes, 8);
        for (kind, size) in [
            (super::MessageKind::WaylandControlRequest, 56),
            (super::MessageKind::WaylandControlAction, 48),
            (super::MessageKind::WaylandControlRevoke, 8),
        ] {
            assert!(
                super::Queue::default()
                    .push(kind, &vec![0; size - 1])
                    .is_err()
            );
            assert!(
                super::Queue::default()
                    .push(kind, &vec![0; size + 1])
                    .is_err()
            );
        }
    }
    #[test]
    fn scoped_pointer_revoke_clears_pending_request_and_actions() {
        let mut queue = super::Queue::default();
        queue
            .push(super::MessageKind::WaylandPointerRequest, &[1; 24])
            .unwrap();
        queue
            .push(super::MessageKind::WaylandPointerAction, &[1; 45])
            .unwrap();
        queue.push(super::MessageKind::Heartbeat, &[]).unwrap();
        queue
            .push(super::MessageKind::WaylandPointerRevoke, &[1; 8])
            .unwrap();
        assert!(queue.control.iter().all(|record| !matches!(
            record.kind,
            super::MessageKind::WaylandPointerRequest | super::MessageKind::WaylandPointerAction
        )));
        assert!(
            queue
                .control
                .iter()
                .any(|record| record.kind == super::MessageKind::Heartbeat)
        );
        assert!(
            queue
                .push(super::MessageKind::WaylandPointerAction, &[0; 44])
                .is_err()
        );
    }
    #[test]
    fn wayland_keyboard_revoke_discards_queued_key_edges_only() {
        let mut queue = super::Queue::default();
        queue
            .push(super::MessageKind::WaylandKeyboardKey, &[1; 43])
            .unwrap();
        queue.push(super::MessageKind::Heartbeat, &[]).unwrap();
        queue
            .push(super::MessageKind::WaylandKeyboardRevoke, &[1; 8])
            .unwrap();
        assert!(
            queue
                .control
                .iter()
                .all(|record| record.kind != super::MessageKind::WaylandKeyboardKey)
        );
        assert!(
            queue
                .control
                .iter()
                .any(|record| record.kind == super::MessageKind::Heartbeat)
        );
        assert!(
            queue
                .push(super::MessageKind::WaylandKeyboardKey, &[0; 42])
                .is_err()
        );
    }
    use super::*;

    fn test_encode_input(events: &[InputEvent], bytes: &mut Vec<u8>) -> RemoteResult<()> {
        encode_source_input_into(1, 1, events, bytes)
    }
    fn test_decode_input(bytes: &[u8]) -> RemoteResult<Vec<InputEvent>> {
        Ok(decode_source_input(bytes)?.2)
    }
    fn input(queue: &mut Queue, events: &[InputEvent]) {
        let mut bytes = Vec::new();
        test_encode_input(events, &mut bytes).unwrap();
        queue.push(MessageKind::InputBatch, &bytes).unwrap();
    }

    fn popup_revoke_payload() -> Vec<u8> {
        super::super::wayland_popup_wire::Binding {
            source_instance: 1,
            source_epoch: 2,
            lease_epoch: 3,
        }
        .encode()
        .unwrap()
        .to_vec()
    }
    #[test]
    fn popup_terminal_survives_close_queued_inflight_and_flushed() {
        let payload = popup_revoke_payload();
        for phase in 0..3 {
            let mut q = Queue::default();
            for _ in 0..CONTROL_RECORDS {
                q.push(MessageKind::WaylandPopupClick, &[1; 80]).unwrap();
            }
            q.push(MessageKind::Clipboard, b"unsent").unwrap();
            q.push(MessageKind::WaylandPopupRevoke, &payload).unwrap();
            assert_eq!(
                (q.control.len(), q.control_bytes, q.bulk_bytes),
                (1, payload.len(), 0)
            );
            assert!(q.push(MessageKind::FrameAck, &[0; 16]).is_err());
            assert!(q.push(MessageKind::WaylandPopupRevoke, &payload).is_err());
            let popped = if phase > 0 { q.pop() } else { None };
            if phase == 2 {
                q.record_flushed(MessageKind::WaylandPopupRevoke);
                q.stopped = true;
                assert!(q.check().is_ok());
            }
            q.close(false);
            let record = popped.or_else(|| q.pop()).unwrap();
            assert_eq!(record.kind, MessageKind::WaylandPopupRevoke);
            assert_eq!(record.payload, payload);
            assert!(q.pop().is_none());
            assert_eq!((q.control_bytes, q.bulk_bytes), (0, 0));
        }
    }
    #[test]
    fn popup_terminal_memory_writer_emits_only_exact_revoke() {
        use super::super::protocol::SessionReader;
        let payload = popup_revoke_payload();
        let mut q = Queue::default();
        q.push(MessageKind::FrameAck, &[0; 16]).unwrap();
        q.push(MessageKind::WaylandPopupRevoke, &payload).unwrap();
        q.close(false);
        let record = q.pop().unwrap();
        let key = [0x42; 32];
        let mut writer = SessionWriter::new(Vec::new(), key);
        write_and_flush_record(&mut writer, record.kind, &record.payload).unwrap();
        q.record_flushed(record.kind);
        let mut reader = SessionReader::new(std::io::Cursor::new(writer.get_ref().clone()), key);
        assert_eq!(
            reader.read_message().unwrap(),
            (MessageKind::WaylandPopupRevoke, payload)
        );
        assert!(reader.read_message().is_err());
        assert_eq!(q.popup_terminal, Some(PopupTerminal::Flushed));
        assert!(q.pop().is_none());
    }
    #[test]
    fn popup_terminal_preserves_invalid_binding_and_flush_failure() {
        struct FailsFlush;
        impl RecordWriter for FailsFlush {
            fn write_message(&mut self, _: MessageKind, _: &[u8]) -> RemoteResult<()> {
                Ok(())
            }
            fn flush(&mut self) -> RemoteResult<()> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "flush failed").into())
            }
        }
        let mut q = Queue::default();
        assert!(q.push(MessageKind::WaylandPopupRevoke, &[0; 20]).is_err());
        assert_eq!(q.popup_terminal, None);
        q.push(MessageKind::WaylandPopupRevoke, &popup_revoke_payload())
            .unwrap();
        let record = q.pop().unwrap();
        assert!(write_and_flush_record(&mut FailsFlush, record.kind, &record.payload).is_err());
        assert_eq!(q.popup_terminal, Some(PopupTerminal::InFlight));
        q.record_worker_error(|| "flush failed".into(), Some(io::ErrorKind::BrokenPipe));
        assert!(q.check().is_err());
    }

    #[test]
    fn restriction_barrier_has_reserved_capacity_and_preserves_bulk_order() {
        let mut q = Queue::default();
        for _ in 0..CONTROL_RECORDS {
            input(
                &mut q,
                &[InputEvent::Key {
                    keycode: 42,
                    pressed: true,
                }],
            );
        }
        for _ in 0..BULK_RECORDS {
            q.push(MessageKind::Clipboard, b"before").unwrap();
        }
        let request = ClipboardRestriction {
            revision: 1,
            content: 3,
        }
        .encode();
        q.push(MessageKind::ClipboardRestrict, &request).unwrap();
        assert!(q.push(MessageKind::ClipboardRestrict, &request).is_err());
        for _ in 0..CONTROL_RECORDS {
            assert_eq!(q.pop().unwrap().kind, MessageKind::InputBatch);
        }
        assert_eq!(q.pop().unwrap().payload, b"before");
        q.push(MessageKind::Clipboard, b"after").unwrap();
        for _ in 1..BULK_RECORDS {
            assert_eq!(q.pop().unwrap().payload, b"before");
        }
        assert_eq!(q.pop().unwrap().kind, MessageKind::ClipboardRestrict);
        assert_eq!(q.pop().unwrap().payload, b"after");
        assert!(q.pop().is_none());
        assert_eq!(q.bulk_bytes, 0);
    }

    #[test]
    fn release_and_shutdown_fences_preserve_source_and_geometry() {
        let mut q = Queue::default();
        q.push(
            MessageKind::InputBatch,
            &super::super::messages::encode_source_input(
                2,
                5,
                &[InputEvent::Key {
                    keycode: 42,
                    pressed: true,
                }],
            )
            .unwrap(),
        )
        .unwrap();
        q.push(
            MessageKind::InputBatch,
            &super::super::messages::encode_source_input(2, 5, &[InputEvent::ReleaseAll]).unwrap(),
        )
        .unwrap();
        assert_eq!(q.control.len(), 1);
        assert_eq!(
            decode_source_input(&q.pop().unwrap().payload).unwrap(),
            (2, 5, vec![InputEvent::ReleaseAll])
        );
        q.close(true);
        assert_eq!(
            decode_source_input(&q.pop().unwrap().payload).unwrap(),
            (2, 5, vec![InputEvent::ReleaseAll])
        );
    }

    #[test]
    fn unscoped_ack_and_input_are_rejected() {
        let mut q = Queue::default();
        assert!(q.push(MessageKind::FrameAck, &1_u64.to_be_bytes()).is_err());
        assert!(q.push(MessageKind::InputBatch, &[1, 4]).is_err());
    }

    #[test]
    fn control_overtakes_bulk_but_input_edges_stay_fifo() {
        let mut q = Queue::default();
        q.push(MessageKind::Clipboard, b"bulk").unwrap();
        input(
            &mut q,
            &[InputEvent::Key {
                keycode: 42,
                pressed: true,
            }],
        );
        input(
            &mut q,
            &[InputEvent::Key {
                keycode: 42,
                pressed: false,
            }],
        );
        assert_eq!(
            test_decode_input(&q.pop().unwrap().payload).unwrap(),
            [InputEvent::Key {
                keycode: 42,
                pressed: true
            }]
        );
        assert_eq!(
            test_decode_input(&q.pop().unwrap().payload).unwrap(),
            [InputEvent::Key {
                keycode: 42,
                pressed: false
            }]
        );
        assert_eq!(q.pop().unwrap().kind, MessageKind::Clipboard);
        assert_eq!((q.control_bytes, q.bulk_bytes), (0, 0));
    }

    #[test]
    fn release_fence_drops_old_edges_and_preserves_new_edges() {
        let mut q = Queue::default();
        input(
            &mut q,
            &[InputEvent::Key {
                keycode: 42,
                pressed: true,
            }],
        );
        let tail = [
            InputEvent::ReleaseAll,
            InputEvent::Key {
                keycode: 43,
                pressed: true,
            },
        ];
        input(
            &mut q,
            &[InputEvent::Pointer { x: 1, y: 2 }, tail[0], tail[1]],
        );
        assert_eq!(test_decode_input(&q.pop().unwrap().payload).unwrap(), tail);
        assert!(q.pop().is_none());
    }

    #[test]
    fn queues_are_bounded_and_close_has_reserved_capacity() {
        let mut q = Queue::default();
        for _ in 0..CONTROL_RECORDS {
            input(
                &mut q,
                &[InputEvent::Key {
                    keycode: 42,
                    pressed: true,
                }],
            );
        }
        assert!(q.push(MessageKind::Heartbeat, &[]).is_err());
        for _ in 0..BULK_RECORDS {
            q.push(MessageKind::Clipboard, b"a").unwrap();
        }
        assert!(q.push(MessageKind::Clipboard, b"a").is_err());
        q.close(true);
        let release = q.pop().unwrap();
        assert_eq!(release.kind, MessageKind::InputBatch);
        assert_eq!(
            test_decode_input(&release.payload).unwrap(),
            [InputEvent::ReleaseAll]
        );
        assert_eq!(q.pop().unwrap().kind, MessageKind::Close);
        assert!(q.pop().is_none());
        assert!(q.push(MessageKind::Heartbeat, &[]).is_err());
    }

    #[test]
    fn byte_budget_and_latest_state_are_bounded() {
        let mut q = Queue::default();
        assert!(
            q.push(MessageKind::Clipboard, &vec![0; BULK_BYTES + 1])
                .is_err()
        );
        for n in 0_u64..1000 {
            q.push(
                MessageKind::FrameAck,
                &super::super::messages::encode_source_frame_ack(1, n).unwrap(),
            )
            .unwrap();
        }
        assert_eq!(q.control.len(), 1);
        assert_eq!(q.control_bytes, 16);
        assert_eq!(
            q.pop().unwrap().payload,
            super::super::messages::encode_source_frame_ack(1, 999).unwrap()
        );
    }
    #[test]
    fn view_only_close_never_injects_input() {
        let mut q = Queue::default();
        q.close(false);
        assert_eq!(q.pop().unwrap().kind, MessageKind::Close);
        assert!(q.pop().is_none());
    }

    #[test]
    fn queued_revoked_payloads_are_removed_at_actual_socket_dispatch() {
        use super::super::clipboard_sync::{TransferKind, TransferOp};
        use super::super::protocol::SessionReader;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let key = [71; 32];
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (barrier_tx, barrier_rx) = mpsc::sync_channel(1);
        let mut sender = Sender::spawn(SessionWriter::new(socket, key), false, move |kind| {
            if kind == MessageKind::Heartbeat {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            } else if kind == MessageKind::ClipboardRestrict {
                barrier_tx.send(()).unwrap();
            }
        })
        .unwrap();
        let policy = Arc::new(AtomicU8::new(7));
        sender.set_clipboard_policy(Arc::clone(&policy)).unwrap();
        sender.write_message(MessageKind::Heartbeat, &[]).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let begin = TransferOp::Begin {
            id: 9,
            kind: TransferKind::Files,
            length: 3,
            entries: 1,
            items: 1,
        };
        let data = TransferOp::Data {
            id: 9,
            bytes: vec![11, 22, 33],
        };
        for op in [begin, data, TransferOp::End { id: 9 }] {
            sender
                .write_message(MessageKind::ClipboardTransfer, &op.encode())
                .unwrap();
        }
        sender
            .write_message(MessageKind::Clipboard, b"private text")
            .unwrap();
        policy.store(0, Ordering::Release);
        let request = ClipboardRestriction {
            revision: 1,
            content: 0,
        };
        sender
            .write_message(MessageKind::ClipboardRestrict, &request.encode())
            .unwrap();
        release_tx.send(()).unwrap();
        barrier_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        sender.close().unwrap();
        let mut reader = SessionReader::new(peer, key);
        assert_eq!(reader.read_message().unwrap().0, MessageKind::Heartbeat);
        for expected in [
            TransferOp::Cancel { id: 9 },
            TransferOp::Skip { id: 9, bytes: 3 },
            TransferOp::Cancel { id: 9 },
        ] {
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::ClipboardTransfer);
            assert_eq!(TransferOp::decode(&payload).unwrap(), expected);
        }
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::ClipboardRestrict);
        assert_eq!(ClipboardRestriction::decode(&payload).unwrap(), request);
        assert_eq!(reader.read_message().unwrap().0, MessageKind::Close);
        assert!(reader.read_message().is_err());
    }

    #[test]
    fn blocked_writer_does_not_block_ingress_and_close_fences_wire_input() {
        use super::super::protocol::SessionReader;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let key = [42; 32];
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (gate_tx, gate_rx) = mpsc::sync_channel(1);
        let mut sender = Sender::spawn(SessionWriter::new(socket, key), true, move |kind| {
            if kind == MessageKind::Heartbeat {
                entered_tx.send(()).unwrap();
                gate_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        })
        .unwrap();
        sender.write_message(MessageKind::Heartbeat, &[]).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // The worker cannot progress until explicitly released. All foreground
        // operations below must still return and preserve bounded queue state.
        let started = Instant::now();
        sender
            .write_message(MessageKind::Clipboard, b"bulk")
            .unwrap();
        let mut payload = Vec::new();
        test_encode_input(
            &[InputEvent::Key {
                keycode: 42,
                pressed: true,
            }],
            &mut payload,
        )
        .unwrap();
        sender
            .write_message(MessageKind::InputBatch, &payload)
            .unwrap();
        sender.flush().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        // Install the same reserved fence used by close while the worker is
        // paused, then release it. No pre-fence queued data may reach the wire.
        sender.shared.0.lock().unwrap().close(true);
        gate_tx.send(()).unwrap();
        sender.done.recv_timeout(Duration::from_secs(2)).unwrap();
        sender.worker.take().unwrap().join().unwrap();
        let mut reader = SessionReader::new(peer, key);
        assert_eq!(reader.read_message().unwrap().0, MessageKind::Heartbeat);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::InputBatch);
        assert_eq!(
            test_decode_input(&payload).unwrap(),
            [InputEvent::ReleaseAll]
        );
        assert_eq!(reader.read_message().unwrap().0, MessageKind::Close);
        assert!(reader.read_message().is_err());
    }

    #[test]
    fn shutdown_deadline_is_explicit_and_later_join_is_possible() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut sender = Sender::spawn(SessionWriter::new(socket, [42; 32]), false, move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
        .unwrap();
        sender.write_message(MessageKind::Heartbeat, &[]).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let error = sender.shutdown_timeout(Duration::ZERO).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(sender.joining.is_some());
        release_tx.send(()).unwrap();
        sender.shutdown_timeout(Duration::from_secs(2)).unwrap();
        assert!(sender.worker.is_none());
        sender.shutdown_timeout(Duration::ZERO).unwrap();
    }

    #[test]
    fn completion_notification_does_not_make_a_hung_worker_joinable() {
        let (done_tx, done_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let joined = reap_worker(worker).unwrap();
        assert_eq!(
            joined.recv_timeout(Duration::ZERO),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        release_tx.send(()).unwrap();
        assert!(joined.recv_timeout(Duration::from_secs(2)).unwrap());
    }

    #[test]
    fn blocking_thread_local_destructor_cannot_block_foreground_join() {
        struct Gate(mpsc::Sender<()>, mpsc::Receiver<()>);
        impl Drop for Gate {
            fn drop(&mut self) {
                let _ = self.0.send(());
                let _ = self.1.recv();
            }
        }
        thread_local! {
            static GATE: std::cell::RefCell<Option<Gate>> = const { std::cell::RefCell::new(None) };
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            GATE.with(|gate| *gate.borrow_mut() = Some(Gate(entered_tx, release_rx)));
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let joined = reap_worker(worker).unwrap();
        assert_eq!(
            joined.recv_timeout(Duration::ZERO),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        release_tx.send(()).unwrap();
        assert!(joined.recv_timeout(Duration::from_secs(2)).unwrap());
    }

    #[test]
    fn worker_panic_is_reported_and_shutdown_is_observable() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let mut sender = Sender::spawn(SessionWriter::new(socket, [42; 32]), false, |_| {
            panic!("fixture")
        })
        .unwrap();
        sender.write_message(MessageKind::Heartbeat, &[]).unwrap();
        sender.done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(sender.flush().unwrap_err().to_string().contains("panicked"));
        assert!(
            sender
                .shutdown_timeout(Duration::from_secs(2))
                .unwrap_err()
                .to_string()
                .contains("panicked")
        );
        assert!(sender.worker.is_none());
    }
}

#[cfg(test)]
mod explicit_resize_tests {
    use super::*;
    #[test]
    fn explicit_resize_is_bounded_and_never_coalesced_as_viewport() {
        let mut queue = Queue::default();
        assert!(
            queue
                .push(MessageKind::WindowResizeRequest, &[0; 27])
                .is_err()
        );
        queue
            .push(MessageKind::WindowResizeRequest, &[1; 28])
            .unwrap();
        queue
            .push(MessageKind::WindowResizeRequest, &[2; 28])
            .unwrap();
        assert_eq!(queue.control.len(), 2);
    }
}

#[cfg(test)]
mod shutdown_provenance_tests {
    use super::*;
    #[test]
    fn independent_shutdown_failure_cannot_be_sole_worker_provenance() {
        assert!(shutdown_transport_provenance_allowed(&Ok(())));
        assert!(shutdown_transport_provenance_allowed(&Err(
            io::ErrorKind::NotConnected.into()
        )));
        for kind in [
            io::ErrorKind::Other,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::TimedOut,
        ] {
            assert!(!shutdown_transport_provenance_allowed(&Err(kind.into())));
        }
    }
    #[test]
    fn first_worker_error_and_kind_are_indivisible() {
        for first in [
            None,
            Some(io::ErrorKind::BrokenPipe),
            Some(io::ErrorKind::TimedOut),
        ] {
            let mut queue = Queue::default();
            queue.record_worker_error(|| "first".into(), first);
            queue.record_worker_error(
                || panic!("first error must not be replaced"),
                Some(io::ErrorKind::ConnectionReset),
            );
            assert_eq!(queue.error.as_deref(), Some("first"));
            assert_eq!(queue.error_io_kind, first);
        }
        let mut deadline = Queue {
            error: Some("deadline".into()),
            ..Queue::default()
        };
        deadline.record_worker_error(
            || panic!("deadline must not be replaced"),
            Some(io::ErrorKind::BrokenPipe),
        );
        assert_eq!(deadline.error_io_kind, None);
    }
}
