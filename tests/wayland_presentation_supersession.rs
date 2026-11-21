#![cfg(all(feature = "wayland-backends", feature = "remote-wayland"))]

use smithay::reexports::wayland_server::{
    Client, Display,
    backend::ClientData,
    protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
};
use smithay::wayland::{
    buffer::BufferHandler,
    compositor::{CompositorClientState, CompositorHandler, CompositorState},
    presentation::PresentationState,
    shm::{ShmHandler, ShmState},
};
use std::{
    os::unix::net::UnixStream,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
        wl_subsurface, wl_surface,
    },
};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};

struct ServerState {
    compositor: CompositorState,
    shm: ShmState,
}

#[derive(Default)]
struct SocketState(CompositorClientState);

impl ClientData for SocketState {}

impl CompositorHandler for ServerState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<SocketState>().unwrap().0
    }

    fn commit(&mut self, _: &WlSurface) {}
}

impl BufferHandler for ServerState {
    fn buffer_destroyed(&mut self, _: &WlBuffer) {}
}

impl ShmHandler for ServerState {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

smithay::delegate_dispatch2!(ServerState);

#[derive(Default)]
struct ClientState {
    discarded: Vec<u8>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
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

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u8> for ClientState {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        label: &u8,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Discarded => state.discarded.push(*label),
            _ => panic!("server must not present a frame in a cache-only test"),
        }
    }
}

wayland_client::delegate_noop!(ClientState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(ClientState: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(ClientState: ignore wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(ClientState: ignore wl_subsurface::WlSubsurface);
wayland_client::delegate_noop!(ClientState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(ClientState: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(ClientState: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(ClientState: ignore wp_presentation::WpPresentation);

fn buffer_pair(globals: &GlobalList, qh: &QueueHandle<ClientState>) -> [wl_buffer::WlBuffer; 2] {
    use std::os::fd::AsFd;

    let fd = nix::sys::memfd::memfd_create(
        c"presentation-commits",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
    )
    .unwrap();
    nix::unistd::ftruncate(&fd, 8).unwrap();
    let shm: wl_shm::WlShm = globals.bind(qh, 1..=1, ()).unwrap();
    let pool = shm.create_pool(fd.as_fd(), 8, qh, ());
    [0, 4].map(|offset| pool.create_buffer(offset, 1, 1, 4, wl_shm::Format::Xrgb8888, qh, ()))
}

fn test_supersession(synchronized: bool) {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let (stop, running) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut display = Display::<ServerState>::new().unwrap();
        let mut handle = display.handle();
        let mut state = ServerState {
            compositor: CompositorState::new::<ServerState>(&handle),
            shm: ShmState::new::<ServerState>(&handle, []),
        };
        let _presentation =
            PresentationState::new::<ServerState>(&handle, libc::CLOCK_MONOTONIC as u32);
        handle
            .insert_client(server_socket, Arc::new(SocketState::default()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            display.dispatch_clients(&mut state).unwrap();
            display.flush_clients().unwrap();
            match running.recv_timeout(Duration::from_millis(1)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
        }
        panic!("Wayland protocol test timed out");
    });

    let connection = Connection::from_socket(client_socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<ClientState>(&connection).unwrap();
    let qh = queue.handle();
    let mut state = ClientState::default();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=1, ()).unwrap();
    let parent = compositor.create_surface(&qh, ());
    let surface = if synchronized {
        compositor.create_surface(&qh, ())
    } else {
        parent.clone()
    };
    let _role = synchronized.then(|| {
        let sub: wl_subcompositor::WlSubcompositor = globals.bind(&qh, 1..=1, ()).unwrap();
        sub.get_subsurface(&surface, &parent, &qh, ())
    });
    let presentation: wp_presentation::WpPresentation = globals.bind(&qh, 1..=1, ()).unwrap();
    let [a, b] = buffer_pair(&globals, &qh);

    let _feedback = presentation.feedback(&surface, &qh, 1);
    if synchronized {
        // Feedback for an uncommitted child update must survive a parent commit.
        parent.commit();
    }
    surface.attach(Some(&a), 0, 0);
    surface.commit();
    if synchronized {
        parent.commit();
        // Applying a parent again must not discard the unchanged child update.
        parent.commit();
    }
    queue.roundtrip(&mut state).unwrap();
    assert!(state.discarded.is_empty());

    // Commit a different buffer with no new feedback request. The old
    // callback must never become evidence for the second buffer.
    surface.attach(Some(&b), 0, 0);
    surface.commit();
    if synchronized {
        queue.roundtrip(&mut state).unwrap();
        assert!(
            state.discarded.is_empty(),
            "child update is still synchronized"
        );
        parent.commit();
    }
    queue.roundtrip(&mut state).unwrap();
    assert_eq!(state.discarded, [1]);

    stop.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn feedbackless_root_replacement_discards_the_previous_commit() {
    test_supersession(false);
}

#[test]
fn synchronized_child_keeps_feedback_until_its_replacement_applies() {
    test_supersession(true);
}
