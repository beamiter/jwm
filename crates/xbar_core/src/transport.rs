//! Explicit adapter for the current JWM shared-memory protocol.

use std::io;
use std::sync::Arc;

use shared_structures::{SharedCommand, SharedMessage, SharedRingBuffer};

use crate::{MonitorGeometry, MonitorId, ShellRoute, TagState, WmCommand, WmSnapshot};

/// Result of submitting a command to the bounded shared command queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    Full,
}

/// Owned handle to the current JWM shared-memory transport.
///
/// The semantic model never holds this type. Frontends can therefore replace
/// the transport without changing state reduction or rendering code.
#[derive(Debug, Clone)]
pub struct SharedTransport {
    buffer: Arc<SharedRingBuffer>,
}

impl SharedTransport {
    fn new(buffer: Arc<SharedRingBuffer>) -> Self {
        Self { buffer }
    }

    /// Open an existing window-manager-owned ring buffer.
    ///
    /// This never creates the shared object: a bar must not become protocol
    /// owner merely because it happened to start before the window manager.
    /// Keeping ownership on the producer also prevents dropping a bar from
    /// destroying the ring used by every other consumer.
    pub fn open(path: &str) -> io::Result<Self> {
        SharedRingBuffer::open_auto(path, None)
            .map(Arc::new)
            .map(Self::new)
    }

    /// Drain transport messages and return only the newest semantic snapshot.
    pub fn drain_latest(&self) -> io::Result<Option<WmSnapshot>> {
        if self.buffer.is_destroyed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "window-manager shared transport was destroyed",
            ));
        }
        self.buffer
            .try_read_latest_message()
            .map(|message| message.map(snapshot_from_shared))
    }

    /// Submit a typed window-manager command without exposing protocol values
    /// or queue booleans to the frontend.
    pub fn execute(&self, command: WmCommand) -> io::Result<SendOutcome> {
        self.buffer
            .try_send_command(command_to_shared(command))
            .map(|sent| {
                if sent {
                    SendOutcome::Sent
                } else {
                    SendOutcome::Full
                }
            })
    }

    /// Start an owned Linux eventfd notifier for this transport.
    #[cfg(feature = "runtime-linux")]
    pub fn notifier(&self, non_blocking: bool) -> io::Result<crate::SharedEventNotifier> {
        crate::SharedEventNotifier::spawn(Arc::clone(&self.buffer), non_blocking)
    }
}

fn snapshot_from_shared(message: SharedMessage) -> WmSnapshot {
    let sequence = message.timestamp;
    let info = message.monitor_info;
    WmSnapshot {
        sequence: Some(sequence),
        monitor: MonitorId(info.monitor_num),
        geometry: MonitorGeometry::from_raw(
            info.monitor_x,
            info.monitor_y,
            info.monitor_width,
            info.monitor_height,
        ),
        layout_symbol: info.ltsymbol_lossy().into_owned(),
        client_name: info.client_name_lossy().into_owned(),
        tags: info
            .tag_status_vec
            .into_iter()
            .map(|tag| TagState {
                selected: tag.is_selected,
                urgent: tag.is_urg,
                filled: tag.is_filled,
                occupied: tag.is_occ,
            })
            .collect(),
    }
}

fn command_to_shared(command: WmCommand) -> SharedCommand {
    match command {
        WmCommand::ViewTag { tag, monitor } => SharedCommand::view_tag(tag.mask(), monitor.0),
        WmCommand::ToggleTag { tag, monitor } => SharedCommand::toggle_tag(tag.mask(), monitor.0),
        WmCommand::SetLayout { layout, monitor } => SharedCommand::set_layout(layout.0, monitor.0),
        WmCommand::OpenShellHub { route, monitor } => {
            SharedCommand::shell_hub(shell_route_to_shared(route), monitor.0)
        }
    }
}

/// The two enums stay separate types on purpose: `ShellRoute` belongs to the
/// semantic model and must compile without the shared-memory feature, while
/// `ShellHubRoute` is the wire encoding. Matching exhaustively on both makes a
/// page added to either crate a compile error here rather than a command that
/// is silently dropped or misrouted.
fn shell_route_to_shared(route: ShellRoute) -> shared_structures::ShellHubRoute {
    use shared_structures::ShellHubRoute as Wire;
    match route {
        ShellRoute::Hub => Wire::Hub,
        ShellRoute::Applications => Wire::Applications,
        ShellRoute::Notifications => Wire::Notifications,
        ShellRoute::Clipboard => Wire::Clipboard,
        ShellRoute::Calendar => Wire::Calendar,
        ShellRoute::Wallpaper => Wire::Wallpaper,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn dropping_transport_never_unlinks_the_owner_ring() {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!("/tmp/xbar-core-transport-{}-{sequence}", std::process::id());
        let owner = SharedRingBuffer::create_aux(&path, Some(8), Some(0)).unwrap();

        let transport = SharedTransport::open(&path).unwrap();
        drop(transport);

        let reopened = SharedRingBuffer::open_auto(&path, Some(0))
            .expect("consumer drop must leave the owner link intact");
        drop(reopened);
        owner.destroy().unwrap();
    }

    #[test]
    fn transport_converts_messages_and_submits_typed_commands() {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!("/tmp/xbar-core-transport-{}-{sequence}", std::process::id());
        let owner = SharedRingBuffer::create_aux(&path, Some(8), Some(0)).unwrap();
        let transport = SharedTransport::open(&path).unwrap();
        let mut monitor_info = shared_structures::MonitorInfo {
            monitor_num: 3,
            ..shared_structures::MonitorInfo::default()
        };
        monitor_info.set_client_name("terminal");
        monitor_info.set_ltsymbol("[M]");
        monitor_info.tag_status_vec[2].is_selected = true;
        monitor_info.tag_status_vec[2].is_occ = true;
        let message = shared_structures::SharedMessage {
            timestamp: 44,
            monitor_info,
        };
        assert!(owner.try_write_message(&message).unwrap());

        let snapshot = transport.drain_latest().unwrap().unwrap();
        assert_eq!(snapshot.sequence, Some(44));
        assert_eq!(snapshot.monitor, crate::MonitorId(3));
        assert_eq!(snapshot.client_name, "terminal");
        assert_eq!(snapshot.layout_symbol, "[M]");
        assert!(snapshot.tags[2].selected);
        assert!(snapshot.tags[2].occupied);

        let command = WmCommand::SetLayout {
            layout: crate::LayoutId(2),
            monitor: crate::MonitorId(3),
        };
        assert_eq!(transport.execute(command).unwrap(), SendOutcome::Sent);
        assert_eq!(
            owner.try_receive_command().unwrap(),
            Some(command_to_shared(command))
        );
        owner.destroy().unwrap();
    }

    #[test]
    fn every_shell_route_survives_the_wire_round_trip() {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!("/tmp/xbar-core-transport-{}-{sequence}", std::process::id());
        let owner = SharedRingBuffer::create_aux(&path, Some(8), Some(0)).unwrap();
        let transport = SharedTransport::open(&path).unwrap();

        for route in ShellRoute::ALL {
            let command = WmCommand::OpenShellHub {
                route,
                monitor: MonitorId(1),
            };
            assert_eq!(transport.execute(command).unwrap(), SendOutcome::Sent);
            let received = owner.try_receive_command().unwrap().unwrap();
            assert_eq!(
                received.shell_hub_route(),
                Some(shell_route_to_shared(route))
            );
            assert_eq!(received.get_monitor_id(), 1);
        }
        owner.destroy().unwrap();
    }

    #[test]
    fn destroyed_transport_is_a_broken_pipe() {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!("/tmp/xbar-core-transport-{}-{sequence}", std::process::id());
        let owner = SharedRingBuffer::create_aux(&path, Some(8), Some(0)).unwrap();
        let transport = SharedTransport::open(&path).unwrap();
        owner.destroy().unwrap();

        assert_eq!(
            transport.drain_latest().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn bounded_command_queue_reports_full_without_hiding_it() {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!("/tmp/xbar-core-transport-{}-{sequence}", std::process::id());
        let owner = SharedRingBuffer::create_aux(&path, Some(8), Some(0)).unwrap();
        let transport = SharedTransport::open(&path).unwrap();
        let command = WmCommand::SetLayout {
            layout: crate::LayoutId(1),
            monitor: crate::MonitorId(0),
        };

        for _ in 0..owner.command_capacity() {
            assert_eq!(transport.execute(command).unwrap(), SendOutcome::Sent);
        }
        assert_eq!(transport.execute(command).unwrap(), SendOutcome::Full);
        owner.destroy().unwrap();
    }
}
