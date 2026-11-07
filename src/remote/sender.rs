//! The viewer queues records; exactly one worker owns the authenticated writer.
//! Limits apply before cloning payloads. Overload fails the session rather than
//! losing an input edge or silently corrupting a clipboard transfer.

use super::RemoteResult;
use super::messages::{MAX_CLIPBOARD_BYTES, decode_input_batch, encode_input_batch_into};
use super::protocol::{MessageKind, SessionWriter};
use super::x11_input::InputEvent;
use std::collections::VecDeque;
use std::io;
use std::net::{Shutdown, TcpStream};
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

#[derive(Default)]
struct Queue {
    control: VecDeque<Record>,
    bulk: VecDeque<Record>,
    control_bytes: usize,
    bulk_bytes: usize,
    closing: bool,
    stopped: bool,
    cancelling: bool,
    error: Option<String>,
    writing_since: Option<Instant>,
}

impl Queue {
    fn check(&self) -> RemoteResult<()> {
        if let Some(error) = &self.error {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()).into());
        }
        if self.stopped || self.closing {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "remote sender stopped").into());
        }
        Ok(())
    }

    fn push(&mut self, kind: MessageKind, payload: &[u8]) -> RemoteResult<()> {
        self.check()?;
        let expected = match kind {
            MessageKind::Heartbeat => Some(0),
            MessageKind::FrameAck => Some(8),
            MessageKind::Viewport => Some(4),
            _ => None,
        };
        if expected.is_some_and(|length| payload.len() != length) {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "invalid control record size").into(),
            );
        }
        // ReleaseAll is a fence: discard unsent input before the last release,
        // while preserving subsequent edges in this batch. A record already
        // being written finishes before this fence on the single writer.
        let mut fenced_payload = Vec::new();
        let payload = if kind == MessageKind::InputBatch {
            let events = decode_input_batch(payload)?;
            if let Some(index) = events.iter().rposition(|e| *e == InputEvent::ReleaseAll) {
                self.control.retain(|r| r.kind != MessageKind::InputBatch);
                self.control_bytes = self.control.iter().map(|r| r.payload.len()).sum();
                encode_input_batch_into(&events[index..], &mut fenced_payload)?;
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
        // Reserved shutdown path needs no free queue slot. Send a release
        // before Close when input was negotiated.
        self.control.clear();
        self.bulk.clear();
        self.control_bytes = 0;
        self.bulk_bytes = 0;
        if input_enabled {
            let mut release = Vec::new();
            encode_input_batch_into(&[InputEvent::ReleaseAll], &mut release)
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

    fn pop(&mut self) -> Option<Record> {
        if let Some(record) = self.control.pop_front() {
            self.control_bytes -= record.payload.len();
            Some(record)
        } else if let Some(record) = self.bulk.pop_front() {
            self.bulk_bytes -= record.payload.len();
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
    pub(super) fn shutdown_timeout(&mut self, timeout: Duration) -> RemoteResult<()> {
        {
            let mut queue = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
            queue.cancelling = true;
            queue.stopped = true;
        }
        self.shared.1.notify_all();
        let shutdown = self.control.shutdown(Shutdown::Both);
        self.join_timeout(timeout)?;
        if let Some(error) = &self
            .shared
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .error
        {
            return Err(io::Error::other(error.clone()).into());
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
                        loop {
                            let record = {
                                let (lock, wake) = &*worker_shared;
                                let mut queue = lock.lock().unwrap_or_else(|p| p.into_inner());
                                loop {
                                    if queue.stopped {
                                        return Ok(());
                                    }
                                    if let Some(record) = queue.pop() {
                                        queue.writing_since = Some(Instant::now());
                                        wake.notify_all();
                                        break record;
                                    }
                                    queue = wake.wait(queue).unwrap_or_else(|p| p.into_inner());
                                }
                            };
                            writer.write_message(record.kind, &record.payload)?;
                            writer.flush()?;
                            worker_shared
                                .0
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .writing_since = None;
                            worker_shared.1.notify_all();
                            on_written(record.kind);
                            if record.kind == MessageKind::Close {
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
                        queue.error.get_or_insert_with(|| error.to_string());
                    }
                }
                drop(queue);
                worker_shared.1.notify_all();
                if let Ok(watchdog) = watchdog {
                    let _ = watchdog.join();
                }
                let _ = writer.get_ref().shutdown(Shutdown::Both);
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
    use super::*;

    fn input(queue: &mut Queue, events: &[InputEvent]) {
        let mut bytes = Vec::new();
        encode_input_batch_into(events, &mut bytes).unwrap();
        queue.push(MessageKind::InputBatch, &bytes).unwrap();
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
            decode_input_batch(&q.pop().unwrap().payload).unwrap(),
            [InputEvent::Key {
                keycode: 42,
                pressed: true
            }]
        );
        assert_eq!(
            decode_input_batch(&q.pop().unwrap().payload).unwrap(),
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
        assert_eq!(decode_input_batch(&q.pop().unwrap().payload).unwrap(), tail);
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
            decode_input_batch(&release.payload).unwrap(),
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
            q.push(MessageKind::FrameAck, &n.to_be_bytes()).unwrap();
        }
        assert_eq!(q.control.len(), 1);
        assert_eq!(q.control_bytes, 8);
        assert_eq!(q.pop().unwrap().payload, 999_u64.to_be_bytes());
    }
    #[test]
    fn view_only_close_never_injects_input() {
        let mut q = Queue::default();
        q.close(false);
        assert_eq!(q.pop().unwrap().kind, MessageKind::Close);
        assert!(q.pop().is_none());
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
        encode_input_batch_into(
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
            decode_input_batch(&payload).unwrap(),
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
