//! Explicit adapter for the current JWM shared-memory protocol.

use std::io;
use std::sync::Arc;

use shared_structures::{SharedCommand, SharedMessage, SharedRingBuffer};

use crate::{
    DockItemGeometry, MinimizedWindow, MonitorGeometry, MonitorId, ShellRoute, TagState,
    WindowToken, WmCommand, WmSnapshot,
};

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
    let sequence = if message.minimized_generation != 0 {
        message.minimized_generation
    } else {
        message.timestamp
    };
    let wm_session_id = message.wm_session_id;
    let minimized_overflow =
        message.minimized_flags & shared_structures::MINIMIZED_LIST_FLAG_OVERFLOW != 0;
    let minimized_windows = message
        .minimized_windows()
        .iter()
        .filter(|window| window.window_id != 0)
        .map(|window| MinimizedWindow {
            token: WindowToken(window.window_id),
            monitor: MonitorId(window.monitor_id),
            title: window.title_lossy().into_owned(),
            app_id: window.app_id_lossy().into_owned(),
            flags: window.flags,
        })
        .collect();
    let info = message.monitor_info;
    WmSnapshot {
        sequence: Some(sequence),
        wm_session_id,
        monitor: MonitorId(info.monitor_num),
        geometry: MonitorGeometry::from_raw(
            info.monitor_x,
            info.monitor_y,
            info.monitor_width,
            info.monitor_height,
        ),
        layout_symbol: info.ltsymbol_lossy().into_owned(),
        // A window manager that predates these fields sends zeroes, which is
        // "did not say" rather than "no layouts and layout 0".
        layout: (info.layout_count != 0).then_some(crate::LayoutId(info.layout_id)),
        layout_count: (info.layout_count != 0).then_some(info.layout_count as usize),
        client_name: info.client_name_lossy().into_owned(),
        client_app_id: info.client_app_id_lossy().into_owned(),
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
        minimized_windows,
        minimized_overflow,
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
        WmCommand::RestoreWindow {
            window,
            wm_session_id,
            minimized_generation,
            monitor,
            geometry,
        } => SharedCommand::restore_minimized(
            window.get(),
            wm_session_id,
            minimized_generation,
            monitor.0,
            geometry_to_shared(geometry),
        ),
        WmCommand::PreviewWindow {
            window,
            wm_session_id,
            minimized_generation,
            monitor,
            visible,
            renewal,
            geometry,
        } => SharedCommand::preview_minimized(
            window.get(),
            wm_session_id,
            minimized_generation,
            monitor.0,
            (u32::from(visible) * shared_structures::PREVIEW_MINIMIZED_FLAG_VISIBLE)
                | (u32::from(renewal) * shared_structures::PREVIEW_MINIMIZED_FLAG_RENEWAL),
            geometry_to_shared(geometry),
        ),
        WmCommand::SetDockGeometry {
            window,
            wm_session_id,
            minimized_generation,
            monitor,
            geometry,
        } => SharedCommand::set_minimized_geometry(
            window.map_or(0, WindowToken::get),
            wm_session_id,
            minimized_generation,
            monitor.0,
            geometry_to_shared(geometry),
        ),
    }
}

fn geometry_to_shared(geometry: DockItemGeometry) -> shared_structures::MinimizedWindowAnchor {
    shared_structures::MinimizedWindowAnchor::new(
        geometry.x,
        geometry.y,
        geometry.width,
        geometry.height,
    )
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
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .unwrap();

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
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .unwrap();
        let transport = SharedTransport::open(&path).unwrap();
        let mut monitor_info = shared_structures::MonitorInfo {
            monitor_num: 3,
            ..shared_structures::MonitorInfo::default()
        };
        monitor_info.set_client_name("terminal");
        monitor_info.set_ltsymbol("[M]");
        monitor_info.tag_status_vec[2].is_selected = true;
        monitor_info.tag_status_vec[2].is_occ = true;
        let mut minimized = shared_structures::MinimizedWindowInfo::new(
            0x1234_5678_9abc_def0,
            -2,
            shared_structures::MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE
                | shared_structures::MINIMIZED_WINDOW_FLAG_URGENT,
        );
        minimized.set_title("Terminal");
        minimized.set_app_id("foot");
        let mut message = shared_structures::SharedMessage {
            timestamp: 44,
            wm_session_id: 901,
            minimized_generation: 45,
            monitor_info,
            ..shared_structures::SharedMessage::default()
        };
        assert_eq!(message.set_minimized_windows(&[minimized]), 1);
        message.minimized_flags |= shared_structures::MINIMIZED_LIST_FLAG_OVERFLOW;
        assert!(owner.try_write_message(&message).unwrap());

        let snapshot = transport.drain_latest().unwrap().unwrap();
        assert_eq!(snapshot.sequence, Some(45));
        assert_eq!(snapshot.monitor, crate::MonitorId(3));
        assert_eq!(snapshot.client_name, "terminal");
        assert_eq!(snapshot.layout_symbol, "[M]");
        assert!(snapshot.tags[2].selected);
        assert!(snapshot.tags[2].occupied);
        assert_eq!(snapshot.wm_session_id, 901);
        assert!(snapshot.minimized_overflow);
        assert_eq!(
            snapshot.minimized_windows,
            vec![MinimizedWindow {
                token: WindowToken(0x1234_5678_9abc_def0),
                monitor: MonitorId(-2),
                title: "Terminal".into(),
                app_id: "foot".into(),
                flags: shared_structures::MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE
                    | shared_structures::MINIMIZED_WINDOW_FLAG_URGENT,
            }]
        );

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
    fn minimized_commands_preserve_session_window_monitor_flags_and_anchor() {
        let geometry = DockItemGeometry::new(-100, 240, 54, 36);
        let anchor = geometry_to_shared(geometry);
        let token = WindowToken(0xdead_beef_cafe_babe);

        let restore = command_to_shared(WmCommand::RestoreWindow {
            window: token,
            wm_session_id: 71,
            minimized_generation: 171,
            monitor: MonitorId(-3),
            geometry,
        });
        assert_eq!(
            restore.get_command_type(),
            shared_structures::CommandType::RestoreMinimized
        );
        assert_eq!(restore.get_window_id(), token.get());
        assert_eq!(restore.get_wm_session_id(), 71);
        assert_eq!(restore.get_minimized_generation(), 171);
        assert_eq!(restore.get_monitor_id(), -3);
        assert_eq!(restore.anchor(), anchor);

        for (visible, renewal) in [(false, false), (true, false), (true, true)] {
            let preview = command_to_shared(WmCommand::PreviewWindow {
                window: token,
                wm_session_id: 72,
                minimized_generation: 172,
                monitor: MonitorId(4),
                visible,
                renewal,
                geometry,
            });
            assert_eq!(
                preview.get_command_type(),
                shared_structures::CommandType::PreviewMinimized
            );
            assert_eq!(preview.minimized_window_id(), Some(token.get()));
            assert_eq!(preview.get_wm_session_id(), 72);
            assert_eq!(preview.get_minimized_generation(), 172);
            assert_eq!(preview.get_monitor_id(), 4);
            assert_eq!(preview.anchor(), anchor);
            assert_eq!(
                preview.get_flags(),
                (u32::from(visible) * shared_structures::PREVIEW_MINIMIZED_FLAG_VISIBLE)
                    | (u32::from(renewal) * shared_structures::PREVIEW_MINIMIZED_FLAG_RENEWAL)
            );
        }

        for window in [None, Some(token)] {
            let report = command_to_shared(WmCommand::SetDockGeometry {
                window,
                wm_session_id: 73,
                minimized_generation: 173,
                monitor: MonitorId(5),
                geometry,
            });
            assert_eq!(
                report.get_command_type(),
                shared_structures::CommandType::SetMinimizedGeometry
            );
            assert_eq!(report.get_window_id(), window.map_or(0, WindowToken::get));
            assert_eq!(report.minimized_window_id(), window.map(WindowToken::get));
            assert_eq!(report.get_wm_session_id(), 73);
            assert_eq!(report.get_minimized_generation(), 173);
            assert_eq!(report.get_monitor_id(), 5);
            assert_eq!(report.anchor(), anchor);
        }
    }

    #[test]
    fn legacy_snapshot_sequence_falls_back_to_timestamp() {
        let snapshot = snapshot_from_shared(SharedMessage {
            timestamp: 77,
            minimized_generation: 0,
            ..SharedMessage::default()
        });
        assert_eq!(snapshot.sequence, Some(77));
    }

    #[test]
    fn every_shell_route_survives_the_wire_round_trip() {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!("/tmp/xbar-core-transport-{}-{sequence}", std::process::id());
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .unwrap();
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
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .unwrap();
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
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .unwrap();
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
