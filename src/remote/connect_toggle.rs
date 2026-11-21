//! Local ownership and shortcuts for an opt-in direct X11 connection.

use super::RemoteResult;
use super::client::WakeSender;
use super::engine::{
    ClipboardSnapshot, MetricsSnapshot, SessionSnapshot, SessionSources, SessionState,
    WaylandGestureState, WaylandInputState,
};
use super::source_session::SourceId;
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
    stop: Option<UnixStream>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub(super) struct ControlState {
    session: Mutex<SessionControl>,
    changed: Condvar,
}

#[derive(Default)]
struct SessionControl {
    phase: Phase,
    next_source: bool,
    first_presentation: bool,
    sources: Option<SessionSources>,
    selected_source: Option<SourceId>,
    metrics: Option<MetricsSnapshot>,
    clipboard: Option<ClipboardSnapshot>,
    release_input: bool,
    request_input: Option<(u64, SourceId)>,
    input_grabbed: Option<bool>,
    wayland_input: Option<WaylandInputState>,
    wayland_source_control: bool,
    wayland_gestures: Option<WaylandGestureState>,
}

enum Phase {
    Starting(Option<TcpStream>),
    Viewing(WakeSender),
    WaitingRetry,
    Closing,
}

impl Default for Phase {
    fn default() -> Self {
        Self::Starting(None)
    }
}

impl ConnectToggle {
    /// A process-local controller shares session cancellation and wakeups
    /// without claiming the X11 selection used by the CLI shortcut.
    pub(super) fn local() -> (Self, Arc<ControlState>) {
        let state = Arc::new(ControlState::default());
        (
            Self {
                state: Arc::clone(&state),
                stop: None,
                worker: None,
            },
            state,
        )
    }

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
            stop: Some(stop),
            worker: Some(worker),
        }))
    }

    pub(super) fn owns_shortcut(&self) -> bool {
        self.stop.is_some()
    }

    pub(super) fn presented_source(&self, sources: Option<SessionSources>) {
        let mut session = self.state.session.lock().unwrap();
        if matches!(session.phase, Phase::Viewing(_)) {
            session.first_presentation = true;
            session.sources = sources;
        }
    }

    pub(super) fn invalidate_sources(&self) {
        let mut session = self.state.session.lock().unwrap();
        session.first_presentation = false;
        session.sources = None;
        if let Some(gestures) = &mut session.wayland_gestures {
            gestures.ready_frame = None;
        }
        session.selected_source = None;
        session.request_input = None;
    }

    pub(super) fn publish_metrics(&self, metrics: MetricsSnapshot) {
        self.state.session.lock().unwrap().metrics = Some(metrics);
    }

    pub(super) fn publish_clipboard(&self, clipboard: Option<ClipboardSnapshot>) {
        let mut session = self.state.session.lock().unwrap();
        if matches!(session.phase, Phase::Viewing(_)) {
            session.clipboard = clipboard;
        }
    }

    pub(super) fn take_release_input(&self) -> bool {
        std::mem::take(&mut self.state.session.lock().unwrap().release_input)
    }

    pub(super) fn take_request_input(&self) -> Option<(u64, SourceId)> {
        self.state.session.lock().unwrap().request_input.take()
    }

    pub(super) fn publish_input(&self, grabbed: Option<bool>) {
        let mut session = self.state.session.lock().unwrap();
        if matches!(session.phase, Phase::Viewing(_)) {
            session.input_grabbed = grabbed;
        }
    }

    pub(super) fn publish_wayland_input(&self, input: WaylandInputState) {
        let mut session = self.state.session.lock().unwrap();
        if matches!(session.phase, Phase::Viewing(_)) {
            session.wayland_input = Some(input);
            if input != WaylandInputState::Active
                && let Some(gestures) = &mut session.wayland_gestures
            {
                gestures.ready_frame = None;
            }
        }
    }

    pub(super) fn publish_wayland_gesture_ready(&self, frame: Option<u64>) {
        let mut session = self.state.session.lock().unwrap();
        if matches!(session.phase, Phase::Viewing(_)) {
            let ready_frame = frame.filter(|_| {
                session.sources.is_some()
                    && session.wayland_input == Some(WaylandInputState::Active)
            });
            session.wayland_gestures = Some(WaylandGestureState { ready_frame });
        }
    }

    pub(super) fn enable_wayland_source_control(&self) {
        self.state.session.lock().unwrap().wayland_source_control = true;
    }

    pub(super) fn begin_backoff(&self) -> bool {
        self.state.replace(Phase::WaitingRetry)
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
        let session = self.state.session.lock().unwrap();
        session.next_source || session.selected_source.is_some()
    }

    pub(super) fn take_next_source(&self) -> (Option<SourceId>, bool) {
        let mut session = self.state.session.lock().unwrap();
        let selected = session.selected_source.take();
        if selected.is_some() {
            session.sources = None;
        }
        (selected, std::mem::take(&mut session.next_source))
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
        if matches!(next, Phase::Starting(_) | Phase::WaitingRetry) {
            session.first_presentation = false;
            session.sources = None;
            session.selected_source = None;
            session.metrics = None;
            session.clipboard = None;
            session.release_input = false;
            session.request_input = None;
            session.input_grabbed = None;
            session.wayland_input = None;
            session.wayland_source_control = false;
            session.wayland_gestures = None;
        }
        session.phase = next;
        true
    }

    pub(super) fn close(&self) {
        let previous = {
            let mut session = self.session.lock().unwrap();
            session.next_source = false;
            session.sources = None;
            session.selected_source = None;
            session.clipboard = None;
            session.release_input = false;
            session.request_input = None;
            session.input_grabbed = None;
            session.wayland_input = None;
            session.wayland_source_control = false;
            session.wayland_gestures = None;
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

    pub(super) fn next_source(&self) {
        let mut session = self.session.lock().unwrap();
        if !matches!(session.phase, Phase::Closing) {
            session.next_source = true;
            if let Phase::Viewing(wake) = &session.phase {
                let _ = wake.signal();
            }
        }
    }

    pub(super) fn release_input(&self) -> io::Result<()> {
        let mut session = self.session.lock().unwrap();
        let Phase::Viewing(wake) = &session.phase else {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "viewer is not active",
            ));
        };
        if session.input_grabbed.is_none()
            && session.wayland_input != Some(WaylandInputState::Active)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "viewer has no active input authority",
            ));
        }
        wake.signal()?;
        session.release_input = true;
        Ok(())
    }

    pub(super) fn select_source(
        &self,
        session_id: u128,
        source_epoch: u64,
        target: SourceId,
    ) -> io::Result<()> {
        let mut session = self.session.lock().unwrap();
        let available = session.sources.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "no presented source catalog")
        })?;
        if available.session_id != session_id || available.catalog.current.epoch != source_epoch {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source selection belongs to an earlier session or source",
            ));
        }
        if available.catalog.current.source == target {
            return Ok(());
        }
        if session.wayland_input.is_some() && !session.wayland_source_control {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "scoped Wayland control has a fixed source",
            ));
        }
        if session.wayland_input == Some(WaylandInputState::Pending) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "wait for the input request to finish",
            ));
        }
        if !available
            .catalog
            .entries
            .iter()
            .any(|entry| entry.id == target)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source was not granted by the host",
            ));
        }
        if session.selected_source.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "source selection already queued",
            ));
        }
        if let Phase::Viewing(wake) = &session.phase {
            wake.signal()?;
        }
        session.selected_source = Some(target);
        if session.wayland_source_control {
            // Reserve this transition while the viewer waits for native release.
            session.sources = None;
            session.first_presentation = false;
            if let Some(gestures) = &mut session.wayland_gestures {
                gestures.ready_frame = None;
            }
        }
        Ok(())
    }

    pub(super) fn request_input(
        &self,
        session_id: u128,
        source_epoch: u64,
        target: SourceId,
    ) -> io::Result<()> {
        let mut session = self.session.lock().unwrap();
        let available = session.sources.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "no presented source catalog")
        })?;
        if available.session_id != session_id
            || available.catalog.current.epoch != source_epoch
            || available.catalog.current.source != target
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input request belongs to an earlier session or source",
            ));
        }
        if session.selected_source.is_some()
            || session.wayland_input == Some(WaylandInputState::Pending)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "source selection or input request already queued",
            ));
        }
        if session.wayland_input != Some(WaylandInputState::Available) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no unused Wayland input grant for this source",
            ));
        }
        let Phase::Viewing(wake) = &session.phase else {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "viewer is not active",
            ));
        };
        wake.signal()?;
        session.request_input = Some((source_epoch, target));
        session.wayland_input = Some(WaylandInputState::Pending);
        Ok(())
    }

    pub(super) fn sources(&self) -> Option<SessionSources> {
        self.session.lock().unwrap().sources.clone()
    }

    pub(super) fn metrics(&self) -> Option<MetricsSnapshot> {
        self.session.lock().unwrap().metrics
    }

    pub(super) fn state(&self) -> SessionState {
        self.session.lock().unwrap().state()
    }

    pub(super) fn snapshot(&self) -> SessionSnapshot {
        let session = self.session.lock().unwrap();
        SessionSnapshot {
            state: session.state(),
            sources: session.sources.clone(),
            metrics: session.metrics,
            clipboard: session.clipboard,
            input_grabbed: session.input_grabbed,
            wayland_input: session.wayland_input,
            wayland_source_control: session.wayland_source_control,
            wayland_gestures: session.wayland_gestures,
        }
    }
}

impl SessionControl {
    fn state(&self) -> SessionState {
        match self.phase {
            Phase::Closing => SessionState::Closing,
            Phase::Viewing(_) if self.first_presentation => SessionState::Viewing,
            Phase::Viewing(_) => SessionState::WaitingForPresentation,
            Phase::WaitingRetry => SessionState::WaitingToRetry,
            Phase::Starting(_) => SessionState::Connecting,
        }
    }
}

impl Drop for ConnectToggle {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            let _ = stop.shutdown(Shutdown::Both);
        }
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
    fn process_local_controller_has_no_shortcut_and_cancels_retry() {
        use crate::remote::engine::SessionState;
        let (toggle, commands) = ConnectToggle::local();
        assert!(!toggle.owns_shortcut());
        assert_eq!(commands.state(), SessionState::Connecting);
        assert!(toggle.begin_backoff());
        assert_eq!(commands.state(), SessionState::WaitingToRetry);
        commands.close();
        assert_eq!(commands.state(), SessionState::Closing);
        assert!(!toggle.begin_attempt());
    }

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
