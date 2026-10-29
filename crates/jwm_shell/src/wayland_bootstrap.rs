//! Bound startup even inside wayland-client's blocking registry roundtrip.
//! A duplicate socket descriptor can shut down the connection at the deadline;
//! it does not need the event queue to make progress in order to cancel it.
use std::{
    io,
    net::Shutdown,
    os::{fd::AsFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use wayland_client::Connection;

pub(crate) fn connect(deadline: Instant, alive: &AtomicBool) -> anyhow::Result<Connection> {
    if std::env::var_os("WAYLAND_SOCKET").is_some() {
        // An inherited, already-connected descriptor has no blocking connect.
        return Ok(Connection::connect_to_env()?);
    }
    let display = std::env::var_os("WAYLAND_DISPLAY")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "WAYLAND_DISPLAY is unset"))?;
    let path = if display.is_absolute() {
        display
    } else {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "XDG_RUNTIME_DIR is unset or relative",
                )
            })?;
        runtime.join(display)
    };
    Ok(Connection::from_socket(connect_socket(
        &path, deadline, alive,
    )?)?)
}

fn connect_socket(path: &Path, deadline: Instant, alive: &AtomicBool) -> io::Result<UnixStream> {
    loop {
        if !alive.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "native shell startup cancelled",
            ));
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "native Wayland connect deadline")
            })?;
        match jwm_ipc_transport::connect(path, remaining.min(Duration::from_millis(100))) {
            Err(error) if error.kind() == io::ErrorKind::TimedOut => continue,
            result => return result,
        }
    }
}

pub(crate) struct Deadline {
    finished: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Deadline {
    pub(crate) fn start(
        conn: &Connection,
        deadline: Instant,
        alive: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let socket = UnixStream::from(conn.as_fd().try_clone_to_owned()?);
        Self::watch(socket, deadline, alive)
    }

    fn watch(socket: UnixStream, deadline: Instant, alive: Arc<AtomicBool>) -> io::Result<Self> {
        let (finished, receiver) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("shell-wayland-startup".into())
            .spawn(move || {
                loop {
                    if !alive.load(Ordering::Acquire) || Instant::now() >= deadline {
                        let _ = socket.shutdown(Shutdown::Both);
                        break;
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    match receiver.recv_timeout(remaining.min(Duration::from_millis(25))) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            })?;
        Ok(Self {
            finished: Some(finished),
            worker: Some(worker),
        })
    }

    pub(crate) fn finish(&mut self) {
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use wayland_client::{
        Dispatch, QueueHandle,
        globals::{GlobalListContents, registry_queue_init},
        protocol::wl_registry,
    };

    struct State;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    #[test]
    fn stalled_registry_initialization_is_actually_interrupted() {
        let (client, _silent_compositor) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let started = Instant::now();
        let mut watchdog = Deadline::start(
            &conn,
            started + Duration::from_millis(80),
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
        assert!(registry_queue_init::<State>(&conn).is_err());
        watchdog.finish();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cancellation_interrupts_an_explicit_blocking_roundtrip() {
        let (client, _silent_compositor) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let alive = Arc::new(AtomicBool::new(true));
        let started = Instant::now();
        let mut watchdog =
            Deadline::start(&conn, started + Duration::from_secs(5), alive.clone()).unwrap();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            alive.store(false, Ordering::Release);
        });
        assert!(conn.roundtrip().is_err());
        cancel.join().unwrap();
        watchdog.finish();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn successful_bootstrap_disarms_the_old_deadline() {
        let (mut client, mut compositor) = UnixStream::pair().unwrap();
        let mut watchdog = Deadline::watch(
            client.try_clone().unwrap(),
            Instant::now() + Duration::from_millis(40),
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
        watchdog.finish();
        std::thread::sleep(Duration::from_millis(70));
        compositor.write_all(b"still connected").unwrap();
        let mut data = [0; 15];
        client.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"still connected");
    }
}
