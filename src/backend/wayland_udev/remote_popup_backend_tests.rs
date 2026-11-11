//! Actual system-backend reentrancy regression. CI only; no GL required.

use crate::backend::wayland::state::JwmWaylandState;
use crate::backend::wayland_udev::image_copy_capture::wire_test_client::{Client, Event, Server};
use smithay::reexports::wayland_server::{
    Resource,
    backend::{ClientData, ClientId, GlobalHandler, GlobalId, Handle, ObjectData, ObjectId},
    protocol::wl_seat::WlSeat,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Reentrant {
    handle: Handle,
    nested: bool,
    visits: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
    children: Arc<Mutex<Vec<GlobalId>>>,
}
impl GlobalHandler<JwmWaylandState> for Reentrant {
    fn can_view(&self, _: ClientId, _: &Arc<dyn ClientData>, id: GlobalId) -> bool {
        self.visits.fetch_add(1, Ordering::SeqCst);
        // Pending mutation must be inert; getters must reject before raw access.
        self.handle.remove_global::<JwmWaylandState>(id.clone());
        self.handle.disable_global::<JwmWaylandState>(id.clone());
        if self
            .handle
            .get_global_handler::<JwmWaylandState>(id.clone())
            .is_ok()
            || self.handle.global_info(id.clone()).is_ok()
        {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }
        if self.nested {
            let child = self.handle.create_global::<JwmWaylandState>(
                WlSeat::interface(),
                1,
                Arc::new(Self {
                    handle: self.handle.clone(),
                    nested: false,
                    visits: self.visits.clone(),
                    failures: self.failures.clone(),
                    children: self.children.clone(),
                }),
            );
            self.children.lock().unwrap().push(child);
            // Nested TLS must restore the outer creation identity.
            if self
                .handle
                .get_global_handler::<JwmWaylandState>(id)
                .is_ok()
            {
                self.failures.fetch_add(1, Ordering::SeqCst);
            }
        }
        false
    }
    fn bind(
        self: Arc<Self>,
        _: &Handle,
        _: &mut JwmWaylandState,
        _: ClientId,
        _: GlobalId,
        _: ObjectId,
    ) -> Arc<dyn ObjectData<JwmWaylandState>> {
        panic!("hidden reentrancy-test global must never bind")
    }
}

#[test]
fn actual_filtered_global_pending_reentrancy_and_registered_getter_are_safe() {
    let mut server = Server::new();
    let mut client = server.connect();
    fence(&mut server, &mut client);
    let handle = server.display.handle().backend_handle();
    let visits = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let children = Arc::new(Mutex::new(Vec::new()));
    let id = handle.create_global::<JwmWaylandState>(
        WlSeat::interface(),
        1,
        Arc::new(Reentrant {
            handle: handle.clone(),
            nested: true,
            visits: visits.clone(),
            failures: failures.clone(),
            children: children.clone(),
        }),
    );
    assert_eq!(visits.load(Ordering::SeqCst), 2);
    assert_eq!(failures.load(Ordering::SeqCst), 0);
    assert!(!handle.global_info(id.clone()).unwrap().disabled);
    let first = handle
        .get_global_handler::<JwmWaylandState>(id.clone())
        .unwrap();
    let second = handle
        .get_global_handler::<JwmWaylandState>(id.clone())
        .unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    drop(first);
    drop(second);
    assert!(handle.global_info(id.clone()).is_ok());
    assert!(
        fence(&mut server, &mut client)
            .iter()
            .all(|event| !(event.sender == 2 && event.opcode == 0))
    );
    // Destruction invokes can_view too; use a guard so that callback checks
    // remain pending/no-op, without creating another nested global on removal.
    // Removing the child exercises the same pending lifecycle protection.
    for child in children.lock().unwrap().clone() {
        handle.remove_global::<JwmWaylandState>(child.clone());
        assert!(handle.global_info(child).is_err());
    }
    // Outer handler is intentionally retained until display shutdown, where
    // backend filtering is disabled. No unsafe C lifecycle is driven recursively.
    let _ = id;
}

fn fence(server: &mut Server, client: &mut Client) -> Vec<Event> {
    let done = client.new_id();
    client.request(1, 0, &[done]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut all = Vec::new();
    loop {
        server.roundtrip();
        let events = client.events();
        let complete = events.iter().any(|e| e.sender == done && e.opcode == 0);
        all.extend(events);
        if complete {
            return all;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "display sync deadline"
        );
    }
}
