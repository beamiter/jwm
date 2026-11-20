//! Local ownership and shortcuts for an opt-in direct X11 connection.

use super::RemoteResult;
use super::client::WakeSender;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use sha2::{Digest, Sha256};
use std::io;
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

pub(super) struct ConnectToggle {
    state: Arc<ControlState>,
    stop: UnixStream,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct ControlState {
    session: Mutex<SessionControl>,
    changed: Condvar,
}

#[derive(Default)]
struct SessionControl {
    phase: Phase,
    next_source: bool,
}

enum Phase {
    Starting(Option<TcpStream>),
    Viewing(WakeSender),
    Closing,
}

impl Default for Phase {
    fn default() -> Self {
        Self::Starting(None)
    }
}

impl ConnectToggle {
    /// Claim the X screen's slot for this endpoint, or forward a shortcut to its owner.
    pub(super) fn claim(
        display: Option<&str>,
        endpoint: &str,
        next_source: bool,
    ) -> RemoteResult<Option<Self>> {
        let (conn, root) = super::x11_viewer::open_root(display)?;
        let digest: String = Sha256::digest(endpoint)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let name = format!("_JWM_REMOTE_CONNECT_{root}_{digest}");
        let selection = conn.intern_atom(false, name.as_bytes())?;
        let cycle = conn.intern_atom(false, b"_JWM_REMOTE_NEXT_SOURCE")?;
        let selection = selection.reply()?.atom;
        let cycle = cycle.reply()?.atom;
        let window = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )?
        .check()?;

        // Serialize the lookup and claim across processes. The X server drops
        // the selection when the owning connection closes, even after a crash.
        conn.grab_server()?.check()?;
        let action = if next_source { cycle } else { selection };
        let claimed = claim_selection(&conn, selection, window, action);
        conn.ungrab_server()?.check()?;
        if !claimed? {
            return Ok(None);
        }
        let state = Arc::new(ControlState {
            session: Mutex::new(SessionControl {
                next_source,
                ..SessionControl::default()
            }),
            changed: Condvar::new(),
        });
        let worker_state = Arc::clone(&state);
        let (stop, stopped) = UnixStream::pair()?;
        let worker = thread::Builder::new()
            .name("jwm-remote-toggle".into())
            .spawn(move || {
                if let Err(error) =
                    watch_actions(&conn, selection, cycle, window, &stopped, &worker_state)
                {
                    eprintln!("jwm-remote: viewer shortcuts stopped: {error}");
                    worker_state.close();
                }
            })?;
        Ok(Some(Self {
            state,
            stop,
            worker: Some(worker),
        }))
    }

    pub(super) fn begin_attempt(&self) -> bool {
        self.state.replace(Phase::Starting(None))
    }

    pub(super) fn watch_startup(&self, stream: &TcpStream) -> io::Result<bool> {
        Ok(self
            .state
            .replace(Phase::Starting(Some(stream.try_clone()?))))
    }

    pub(super) fn begin_viewing(&self, wake: &WakeSender) -> io::Result<bool> {
        Ok(self.state.replace(Phase::Viewing(wake.try_clone()?)))
    }

    pub(super) fn close_requested(&self) -> bool {
        matches!(self.state.session.lock().unwrap().phase, Phase::Closing)
    }

    pub(super) fn next_source_pending(&self) -> bool {
        self.state.session.lock().unwrap().next_source
    }

    pub(super) fn take_next_source(&self) -> bool {
        std::mem::take(&mut self.state.session.lock().unwrap().next_source)
    }

    pub(super) fn wait_to_retry(&self, delay: Duration) {
        let session = self.state.session.lock().unwrap();
        let _ = self
            .state
            .changed
            .wait_timeout_while(session, delay, |session| {
                !matches!(session.phase, Phase::Closing)
            });
    }
}

fn claim_selection(
    conn: &RustConnection,
    selection: Atom,
    window: Window,
    action: Atom,
) -> RemoteResult<bool> {
    let owner = conn.get_selection_owner(selection)?.reply()?.owner;
    if owner == NONE {
        conn.set_selection_owner(window, selection, CURRENT_TIME)?
            .check()?;
        Ok(true)
    } else {
        let event = ClientMessageEvent::new(32, owner, action, [0; 5]);
        conn.send_event(false, owner, EventMask::NO_EVENT, event)?
            .check()?;
        Ok(false)
    }
}

fn watch_actions(
    conn: &RustConnection,
    selection: Atom,
    cycle: Atom,
    window: Window,
    stopped: &UnixStream,
    state: &ControlState,
) -> RemoteResult<()> {
    loop {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::ClientMessage(event) if event.window == window && event.format == 32 => {
                    if event.type_ == selection {
                        state.close();
                    } else if event.type_ == cycle {
                        state.next_source();
                    }
                }
                Event::SelectionClear(event)
                    if event.owner == window && event.selection == selection =>
                {
                    state.close();
                }
                _ => {}
            }
        }
        let mut fds = [
            PollFd::new(conn.stream().as_fd(), PollFlags::POLLIN),
            PollFd::new(stopped.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            // Keep owning the slot until input and transport cleanup finish.
            Ok(_) if fds[1].revents().is_some_and(|events| !events.is_empty()) => return Ok(()),
            Ok(_)
                if fds[0].revents().is_some_and(|events| {
                    events.intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL)
                }) =>
            {
                return Err(io::Error::other("X11 toggle connection closed").into());
            }
            Ok(_) | Err(Errno::EINTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

impl ControlState {
    fn replace(&self, next: Phase) -> bool {
        let mut session = self.session.lock().unwrap();
        if matches!(session.phase, Phase::Closing) {
            return false;
        }
        session.phase = next;
        true
    }

    fn close(&self) {
        let previous = {
            let mut session = self.session.lock().unwrap();
            session.next_source = false;
            std::mem::replace(&mut session.phase, Phase::Closing)
        };
        self.changed.notify_all();
        match previous {
            Phase::Starting(Some(stream)) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            Phase::Viewing(wake) => {
                let _ = wake.signal();
            }
            _ => {}
        }
    }

    fn next_source(&self) {
        let mut session = self.session.lock().unwrap();
        if !matches!(session.phase, Phase::Closing) {
            session.next_source = true;
            if let Phase::Viewing(wake) = &session.phase {
                let _ = wake.signal();
            }
        }
    }
}

impl Drop for ConnectToggle {
    fn drop(&mut self) {
        let _ = self.stop.shutdown(Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn closing_interrupts_startup_and_prevents_another_attempt() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let state = ControlState::default();
        assert!(state.replace(Phase::Starting(Some(stream))));
        state.next_source();
        assert!(state.session.lock().unwrap().next_source);
        state.close();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        assert!(!state.replace(Phase::Starting(None)));
        state.next_source();
        assert!(!state.session.lock().unwrap().next_source);
    }
}
