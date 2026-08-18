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
use std::time::Duration;

pub(crate) struct TcpStreamDeadline {
    cancelled: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TcpStreamDeadline {
    pub(crate) fn arm(stream: &TcpStream, timeout: Duration) -> io::Result<Self> {
        let deadline_stream = stream.try_clone()?;
        let cancelled = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::Builder::new()
            .name("jwm-remote-stream-deadline".into())
            .spawn(move || {
                let (lock, wake) = &*worker_cancelled;
                let Ok(guard) = lock.lock() else {
                    let _ = deadline_stream.shutdown(Shutdown::Both);
                    return;
                };
                let Ok((cancelled, wait)) =
                    wake.wait_timeout_while(guard, timeout, |cancelled| !*cancelled)
                else {
                    let _ = deadline_stream.shutdown(Shutdown::Both);
                    return;
                };
                if !*cancelled && wait.timed_out() {
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
}
