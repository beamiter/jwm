//! Absolute deadlines for TCP protocol phases.
//!
//! Socket read/write timeouts apply to individual system calls.  A peer that
//! keeps dribbling bytes can therefore keep a multi-step handshake alive
//! forever.  This guard closes every clone of the socket once the whole phase
//! exceeds its deadline.

use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct TcpStreamDeadline {
    cancelled: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TcpStreamDeadline {
    pub(crate) fn arm(stream: &TcpStream, timeout: Duration) -> io::Result<Self> {
        // Include clone/spawn/worker scheduling time in this phase's budget.
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "TCP phase deadline overflow")
        })?;
        let deadline_stream = stream.try_clone()?;
        let cancelled = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::Builder::new()
            .name("jwm-remote-stream-deadline".into())
            .spawn(move || {
                if wait_until_expired(&worker_cancelled, deadline) {
                    let _ = deadline_stream.shutdown(Shutdown::Both);
                }
            })?;
        Ok(Self {
            cancelled,
            worker: Some(worker),
        })
    }

    pub(crate) fn cancel(&mut self) {
        let (lock, wake) = &*self.cancelled;
        if let Ok(mut cancelled) = lock.lock() {
            *cancelled = true;
            wake.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Use the original absolute deadline after startup delay or a spurious wake.
/// Poisoning remains fail-closed, matching the socket guard's original policy.
fn wait_until_expired(cancelled: &(Mutex<bool>, Condvar), deadline: Instant) -> bool {
    let (lock, wake) = cancelled;
    let Ok(mut guard) = lock.lock() else {
        return true;
    };
    loop {
        if *guard {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        match wake.wait_timeout(guard, remaining) {
            Ok((next, _)) => guard = next,
            Err(_) => return true,
        }
    }
}

impl Drop for TcpStreamDeadline {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let connector = thread::spawn(move || TcpStream::connect(address).unwrap());
        let (accepted, _) = listener.accept().unwrap();
        (connector.join().unwrap(), accepted)
    }

    #[test]
    fn expiry_interrupts_a_blocked_read() {
        let (mut stream, _peer) = loopback_pair();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let _deadline = TcpStreamDeadline::arm(&stream, Duration::from_millis(40)).unwrap();
        let started = Instant::now();
        let mut byte = [0_u8; 1];
        let result = stream.read_exact(&mut byte);
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn cancellation_keeps_the_socket_usable() {
        let (mut stream, mut peer) = loopback_pair();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut deadline = TcpStreamDeadline::arm(&stream, Duration::from_millis(40)).unwrap();
        deadline.cancel();
        thread::sleep(Duration::from_millis(80));
        peer.write_all(b"x").unwrap();
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"x");
    }

    #[test]
    fn delayed_worker_uses_already_expired_absolute_deadline() {
        let state = (Mutex::new(false), Condvar::new());
        // Model a worker that first runs after the phase deadline. No sleep or
        // scheduler timing is needed to establish this ordering.
        let deadline = Instant::now() - Duration::from_secs(1);
        assert!(wait_until_expired(&state, deadline));
    }

    #[test]
    fn cancelled_delayed_worker_does_not_close_a_completed_phase() {
        let state = (Mutex::new(true), Condvar::new());
        assert!(!wait_until_expired(
            &state,
            Instant::now() - Duration::from_secs(1)
        ));
    }

    #[test]
    fn cancellation_wakes_a_long_deadline_without_waiting_for_expiry() {
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_state = Arc::clone(&state);
        let (send, receive) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            send.send(wait_until_expired(
                &worker_state,
                Instant::now() + Duration::from_secs(60),
            ))
            .unwrap();
        });
        let (lock, wake) = &*state;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert!(!receive.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn overflowing_deadline_is_rejected_without_closing_socket() {
        let (mut stream, mut peer) = loopback_pair();
        assert!(matches!(
            TcpStreamDeadline::arm(&stream, Duration::MAX),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput
        ));
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        peer.write_all(b"x").unwrap();
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"x");
    }
}
