//! The authenticated reader's bounded FIFO for the sequential decoder.
//! Only the two granted video credits may wait here. An additional record may
//! be in the reader, one in the decoder and one spare allocation may be kept.

use super::protocol::{MAX_PAYLOAD_LEN, MessageKind};
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const RECORDS: usize = 2;
// SessionReader reads the authentication tag into the same allocation.
const RECORD_BYTES: usize = MAX_PAYLOAD_LEN + 16;
const BYTES: usize = RECORDS * RECORD_BYTES;
const MAX_AGE: Duration = Duration::from_secs(2);
const CANCEL_POLL: Duration = Duration::from_millis(25);

struct Record {
    kind: MessageKind,
    payload: Vec<u8>,
    authenticated_at: Instant,
}

#[derive(Default)]
struct State {
    records: VecDeque<Record>,
    bytes: usize,
    spare: Vec<u8>,
    closed: bool,
    video_deadline: Option<Instant>,
    expired: Option<ReceiveDeadline>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReceiveDeadline {
    QueueAge,
    VideoIdle,
}

impl ReceiveDeadline {
    pub(super) fn error(self) -> io::Error {
        let message = match self {
            Self::QueueAge => "authenticated remote decode queue exceeded its age limit",
            Self::VideoIdle => "host stopped sending video frames",
        };
        io::Error::new(io::ErrorKind::TimedOut, message)
    }
}

impl State {
    fn deadline(&self) -> Option<(Instant, ReceiveDeadline)> {
        let queue = self
            .records
            .front()
            .map(|front| front.authenticated_at + MAX_AGE);
        match (queue, self.video_deadline) {
            (Some(queue), Some(video)) if video < queue => {
                Some((video, ReceiveDeadline::VideoIdle))
            }
            (Some(queue), _) => Some((queue, ReceiveDeadline::QueueAge)),
            (None, Some(video)) => Some((video, ReceiveDeadline::VideoIdle)),
            (None, None) => None,
        }
    }

    fn expire(&mut self, now: Instant) -> Option<ReceiveDeadline> {
        if self.expired.is_none()
            && let Some((due, reason)) = self.deadline()
            && due <= now
        {
            self.expired = Some(reason);
        }
        self.expired
    }
}

#[derive(Default)]
pub(super) struct DecodeQueue {
    state: Mutex<State>,
    ready: Condvar,
    cursor_barriers: AtomicUsize,
}

#[derive(Debug)]
pub(super) struct DequeueTiming {
    pub(super) age: Duration,
    pub(super) records: usize,
    pub(super) bytes: usize,
}

impl DecodeQueue {
    pub(super) fn with_video_deadline(deadline: Instant) -> Self {
        Self {
            state: Mutex::new(State {
                video_deadline: Some(deadline),
                ..State::default()
            }),
            ..Self::default()
        }
    }

    pub(super) fn record_authenticated(
        &self,
        kind: MessageKind,
        authenticated_at: Instant,
        video_timeout: Duration,
    ) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "remote decode queue closed",
            ));
        }
        if let Some(reason) = state.expire(authenticated_at) {
            return Err(reason.error());
        }
        if kind == MessageKind::Frame {
            state.video_deadline = Some(authenticated_at + video_timeout);
            self.ready.notify_all();
        }
        Ok(())
    }

    pub(super) fn expired_at(&self, now: Instant) -> Option<ReceiveDeadline> {
        self.state.lock().unwrap().expire(now)
    }

    /// A stalled decoder and a blocked network read must not retain an expired
    /// record or delay closing the viewer's input/publication gate.
    pub(super) fn wait_for_expiry(&self) -> Option<ReceiveDeadline> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return None;
            }
            let now = Instant::now();
            if let Some(reason) = state.expire(now) {
                return Some(reason);
            }
            state = match state.deadline() {
                Some((due, _)) => self.ready.wait_timeout(state, due - now).unwrap().0,
                None => self.ready.wait(state).unwrap(),
            };
        }
    }

    pub(super) fn can_dispatch_cursor(&self) -> bool {
        self.cursor_barriers.load(Ordering::Acquire) == 0
    }

    pub(super) fn handled(&self, kind: MessageKind) {
        if orders_cursor(kind) {
            self.cursor_barriers.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub(super) fn push(
        &self,
        kind: MessageKind,
        payload: &mut Vec<u8>,
        authenticated_at: Instant,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<()> {
        if payload.capacity() > RECORD_BYTES {
            payload.shrink_to_fit();
        }
        let bytes = payload.capacity();
        if bytes > RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "receive record exceeds the buffer limit",
            ));
        }
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed || cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "remote decode queue cancelled",
                ));
            }
            let now = Instant::now();
            if let Some(reason) = state.expire(now) {
                return Err(reason.error());
            }
            let pending_deadline = authenticated_at + MAX_AGE;
            let due = state
                .deadline()
                .map_or(pending_deadline, |(active, _)| active.min(pending_deadline));
            let remaining = due.saturating_duration_since(now);
            if remaining.is_zero() {
                return Err(ReceiveDeadline::QueueAge.error());
            }
            if state.records.len() < RECORDS && bytes <= BYTES - state.bytes {
                if orders_cursor(kind) {
                    self.cursor_barriers.fetch_add(1, Ordering::AcqRel);
                }
                let spare = std::mem::take(&mut state.spare);
                state.records.push_back(Record {
                    kind,
                    payload: std::mem::replace(payload, spare),
                    authenticated_at,
                });
                state.bytes += bytes;
                self.ready.notify_all();
                return Ok(());
            }
            state = self
                .ready
                .wait_timeout(state, remaining.min(CANCEL_POLL))
                .unwrap()
                .0;
        }
    }

    pub(super) fn pop(
        &self,
        payload: &mut Vec<u8>,
        timeout: Duration,
    ) -> io::Result<(MessageKind, DequeueTiming)> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "remote decode queue closed",
                ));
            }
            let now = Instant::now();
            if let Some(reason) = state.expire(now) {
                return Err(reason.error());
            }
            if let Some(record) = state.records.front() {
                let timing = DequeueTiming {
                    age: now.saturating_duration_since(record.authenticated_at),
                    records: state.records.len(),
                    bytes: state.bytes,
                };
                let record = state.records.pop_front().unwrap();
                state.bytes -= record.payload.capacity();
                let last = std::mem::replace(payload, record.payload);
                let retired = if last.capacity() <= RECORD_BYTES
                    && last.capacity() >= state.spare.capacity()
                {
                    std::mem::replace(&mut state.spare, last)
                } else {
                    last
                };
                self.ready.notify_all();
                drop(state);
                drop(retired);
                return Ok((record.kind, timing));
            }
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                return Err(ReceiveDeadline::VideoIdle.error());
            }
            state = self.ready.wait_timeout(state, remaining).unwrap().0;
        }
    }

    pub(super) fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let retired = std::mem::take(&mut *state);
        state.closed = true;
        self.ready.notify_all();
        drop(state);
        drop(retired);
    }
}

fn orders_cursor(kind: MessageKind) -> bool {
    matches!(
        kind,
        MessageKind::SourceChanged | MessageKind::CursorShape | MessageKind::CursorPosition
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_preserves_frame_and_source_barrier_order() {
        let queue = DecodeQueue::default();
        for (kind, byte) in [(MessageKind::Frame, 1), (MessageKind::SourceChanged, 2)] {
            queue
                .push(kind, &mut vec![byte], Instant::now(), || false)
                .unwrap();
        }
        assert!(!queue.can_dispatch_cursor());
        let mut payload = Vec::new();
        for (kind, byte) in [(MessageKind::Frame, 1), (MessageKind::SourceChanged, 2)] {
            assert_eq!(queue.pop(&mut payload, Duration::ZERO).unwrap().0, kind);
            assert_eq!(payload, [byte]);
            assert!(!queue.can_dispatch_cursor());
            queue.handled(kind);
        }
        assert!(queue.can_dispatch_cursor());
        queue.close();
        assert_eq!(
            queue.pop(&mut payload, Duration::ZERO).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn expired_and_cancelled_frames_cannot_enter_the_queue() {
        let queue = DecodeQueue::default();
        let mut payload = vec![1];
        assert_eq!(
            queue
                .push(
                    MessageKind::Frame,
                    &mut payload,
                    Instant::now() - MAX_AGE,
                    || false
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            queue
                .push(MessageKind::Frame, &mut payload, Instant::now(), || true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(payload, [1]);
    }

    #[test]
    fn queue_age_expires_without_another_push_or_pop() {
        let now = Instant::now();
        let queue = DecodeQueue::with_video_deadline(now + Duration::from_secs(60));
        queue
            .push(
                MessageKind::Frame,
                &mut vec![1],
                now - MAX_AGE + Duration::from_millis(200),
                || false,
            )
            .unwrap();
        assert_eq!(queue.wait_for_expiry(), Some(ReceiveDeadline::QueueAge));
        // A new complete record cannot displace the first timeout.
        let error = queue
            .record_authenticated(MessageKind::Frame, Instant::now(), Duration::from_secs(8))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            ReceiveDeadline::QueueAge.error().to_string()
        );
    }

    #[test]
    fn media_idle_can_precede_queue_age_and_only_video_renews_it() {
        let now = Instant::now();
        let timeout = Duration::from_millis(500);
        let queue = DecodeQueue::with_video_deadline(now + timeout);
        queue
            .push(MessageKind::Frame, &mut vec![1], now, || false)
            .unwrap();
        queue
            .record_authenticated(MessageKind::CursorPosition, now, Duration::from_secs(8))
            .unwrap();
        {
            let mut state = queue.state.lock().unwrap();
            assert_eq!(
                state.deadline(),
                Some((now + timeout, ReceiveDeadline::VideoIdle))
            );
            assert_eq!(
                state.expire(now + timeout),
                Some(ReceiveDeadline::VideoIdle)
            );
        }
        assert_eq!(queue.wait_for_expiry(), Some(ReceiveDeadline::VideoIdle));

        let queue = DecodeQueue::with_video_deadline(now + timeout);
        queue
            .record_authenticated(MessageKind::Frame, now, Duration::from_secs(8))
            .unwrap();
        assert_eq!(
            queue.state.lock().unwrap().deadline(),
            Some((now + Duration::from_secs(8), ReceiveDeadline::VideoIdle))
        );
    }

    #[test]
    fn drained_queue_replaces_age_deadline_and_close_releases_waiter() {
        let now = Instant::now();
        let media_due = now + Duration::from_secs(60);
        let queue = DecodeQueue::with_video_deadline(media_due);
        queue
            .push(MessageKind::Frame, &mut vec![1], now, || false)
            .unwrap();
        assert_eq!(
            queue.state.lock().unwrap().deadline(),
            Some((now + MAX_AGE, ReceiveDeadline::QueueAge))
        );
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| queue.wait_for_expiry());
            let mut payload = Vec::new();
            assert_eq!(
                queue.pop(&mut payload, Duration::ZERO).unwrap().0,
                MessageKind::Frame
            );
            assert_eq!(
                queue.state.lock().unwrap().deadline(),
                Some((media_due, ReceiveDeadline::VideoIdle))
            );
            queue.close();
            assert_eq!(waiter.join().unwrap(), None);
        });
    }
}
