//! Transport-free translation of backend events into compositor operations.
//!
//! Both X11 transports feed their `BackendEvent` stream into the shared
//! compositor. Which windows the compositor tracks, when tracking ends, which
//! events merely record activity, and which windows are exempt (the root and
//! the compositor's own overlay) are policy decisions that were maintained as
//! two parallel match statements. The decision is now this pure function
//! returning operations; `Compositor::apply_event_op` executes them.

use crate::backend::api::{BackendEvent, NetWmAction, NetWmState, PropertyKind};
use crate::backend::common_define::WindowId;

/// One compositor-facing effect of a backend event.
#[derive(Clone, Debug, PartialEq)]
pub enum CompositorEventOp {
    AddWindow {
        window: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    SetWindowClass {
        window: u32,
        class: String,
    },
    SetOverrideRedirect {
        window: u32,
    },
    RemoveWindow {
        window: u32,
    },
    UpdateGeometry {
        window: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    SetFullscreen {
        window: u32,
        fullscreen: bool,
    },
    MarkDamaged {
        window: u32,
    },
    PresentComplete {
        window: u32,
        serial: u32,
        msc: u64,
        ust: u64,
    },
    PresentIdle {
        window: u32,
        serial: u32,
        pixmap: u32,
    },
    SetMousePosition {
        x: f32,
        y: f32,
    },
    RecordInput,
    /// Root geometry may have changed: resize the compositor viewport to the
    /// current root size and rebuild the per-monitor rect/refresh maps.
    RefreshLayout,
}

/// Lookups the plan needs from the owning backend. Every closure receives the
/// transport-neutral [`WindowId`]; resolution to raw X11 ids and the property
/// queries stay on the backend side of the boundary.
pub struct CompositorEventSources<'a> {
    /// Transport window-id → raw X11 id; `None` for ids the registry no
    /// longer knows.
    pub resolve: &'a dyn Fn(WindowId) -> Option<u32>,
    /// Current window geometry as `(x, y, width, height)`, if the window
    /// still exists.
    pub geometry: &'a dyn Fn(WindowId) -> Option<(i32, i32, u32, u32)>,
    /// Window class (instance part discarded); empty when unknown.
    pub class: &'a dyn Fn(WindowId) -> String,
    /// Whether the window is override-redirect.
    pub override_redirect: &'a dyn Fn(WindowId) -> bool,
}

/// Plan the compositor-facing effects of one backend event.
///
/// `root` and `overlay` are the raw ids of the root window and the
/// compositor's overlay window; both are exempt from tracking, and geometry
/// updates and damage for the overlay are dropped so the compositor never
/// treats its own output as content.
#[must_use]
pub fn compositor_event_ops(
    event: &BackendEvent,
    root: u32,
    overlay: u32,
    sources: &CompositorEventSources,
) -> Vec<CompositorEventOp> {
    let mut ops = Vec::new();
    match event {
        BackendEvent::WindowMapped(win) => {
            if let Some(x11w) = (sources.resolve)(*win) {
                if x11w != root && x11w != overlay {
                    if let Some((x, y, width, height)) = (sources.geometry)(*win) {
                        ops.push(CompositorEventOp::AddWindow {
                            window: x11w,
                            x,
                            y,
                            width,
                            height,
                        });
                    }
                    // Class drives per-window rules; override-redirect windows
                    // are marked so the compositor can skip backdrop blur for
                    // large overlays (screen sharing, etc.).
                    let class = (sources.class)(*win);
                    if !class.is_empty() {
                        ops.push(CompositorEventOp::SetWindowClass {
                            window: x11w,
                            class,
                        });
                    }
                    if (sources.override_redirect)(*win) {
                        ops.push(CompositorEventOp::SetOverrideRedirect { window: x11w });
                    }
                }
            }
        }
        BackendEvent::WindowUnmapped(win) | BackendEvent::WindowDestroyed(win) => {
            if let Some(x11w) = (sources.resolve)(*win) {
                ops.push(CompositorEventOp::RemoveWindow { window: x11w });
            }
        }
        BackendEvent::WindowConfigured {
            window,
            x,
            y,
            width,
            height,
        } => {
            if let Some(x11w) = (sources.resolve)(*window) {
                if x11w != overlay {
                    ops.push(CompositorEventOp::UpdateGeometry {
                        window: x11w,
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                    });
                }
            }
        }
        BackendEvent::WindowStateRequest {
            window,
            state,
            action,
        } => {
            // Track fullscreen state changes for unredirect optimisation.
            if *state == NetWmState::Fullscreen {
                if let Some(x11w) = (sources.resolve)(*window) {
                    let fullscreen = matches!(action, NetWmAction::Add | NetWmAction::Toggle);
                    ops.push(CompositorEventOp::SetFullscreen {
                        window: x11w,
                        fullscreen,
                    });
                }
            }
        }
        BackendEvent::PropertyChanged { window, kind } => {
            if matches!(kind, PropertyKind::Class) {
                if let Some(x11w) = (sources.resolve)(*window) {
                    let class = (sources.class)(*window);
                    if !class.is_empty() {
                        ops.push(CompositorEventOp::SetWindowClass {
                            window: x11w,
                            class,
                        });
                    }
                }
            }
        }
        BackendEvent::DamageNotify { drawable } => {
            if let Some(x11w) = (sources.resolve)(*drawable) {
                if x11w != overlay {
                    ops.push(CompositorEventOp::MarkDamaged { window: x11w });
                }
            }
        }
        BackendEvent::PresentComplete {
            window,
            serial,
            msc,
            ust,
        } => {
            if let Some(x11w) = (sources.resolve)(*window) {
                ops.push(CompositorEventOp::PresentComplete {
                    window: x11w,
                    serial: *serial,
                    msc: *msc,
                    ust: *ust,
                });
            }
        }
        BackendEvent::PresentIdle {
            window,
            serial,
            pixmap,
        } => {
            if let Some(x11w) = (sources.resolve)(*window) {
                ops.push(CompositorEventOp::PresentIdle {
                    window: x11w,
                    serial: *serial,
                    pixmap: *pixmap,
                });
            }
        }
        BackendEvent::MotionNotify { root_x, root_y, .. } => {
            ops.push(CompositorEventOp::SetMousePosition {
                x: *root_x as f32,
                y: *root_y as f32,
            });
            ops.push(CompositorEventOp::RecordInput);
        }
        BackendEvent::ButtonPress { .. } | BackendEvent::ButtonRelease { .. } => {
            ops.push(CompositorEventOp::RecordInput);
        }
        BackendEvent::ScreenLayoutChanged => {
            ops.push(CompositorEventOp::RefreshLayout);
        }
        _ => {}
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::{CompositorEventOp, CompositorEventSources, compositor_event_ops};
    use crate::backend::api::{BackendEvent, HitTarget, NetWmAction, NetWmState, PropertyKind};
    use crate::backend::common_define::WindowId;

    const ROOT: u32 = 1;
    const OVERLAY: u32 = 2;

    fn win(raw: u64) -> WindowId {
        WindowId::from_raw(raw)
    }

    /// Sources where every id resolves to its raw value, geometry is a fixed
    /// rectangle, and the class/override-redirect answers are configurable.
    fn sources<'a>(
        resolve: &'a dyn Fn(WindowId) -> Option<u32>,
        class: &'a dyn Fn(WindowId) -> String,
        override_redirect: &'a dyn Fn(WindowId) -> bool,
    ) -> CompositorEventSources<'a> {
        CompositorEventSources {
            resolve,
            geometry: &|_| Some((10, 20, 300, 400)),
            class,
            override_redirect,
        }
    }

    #[test]
    fn a_mapped_window_is_added_with_class_and_override_redirect() {
        let s = sources(&|w| Some(w.raw() as u32), &|_| "xterm".to_string(), &|_| {
            true
        });
        let ops = compositor_event_ops(&BackendEvent::WindowMapped(win(7)), ROOT, OVERLAY, &s);
        assert_eq!(
            ops,
            vec![
                CompositorEventOp::AddWindow {
                    window: 7,
                    x: 10,
                    y: 20,
                    width: 300,
                    height: 400
                },
                CompositorEventOp::SetWindowClass {
                    window: 7,
                    class: "xterm".to_string()
                },
                CompositorEventOp::SetOverrideRedirect { window: 7 },
            ]
        );
    }

    #[test]
    fn the_root_and_overlay_windows_are_never_tracked() {
        let s = sources(&|w| Some(w.raw() as u32), &|_| String::new(), &|_| false);
        for exempt in [u64::from(ROOT), u64::from(OVERLAY)] {
            let ops =
                compositor_event_ops(&BackendEvent::WindowMapped(win(exempt)), ROOT, OVERLAY, &s);
            assert_eq!(ops, vec![]);
        }
    }

    #[test]
    fn an_empty_class_and_a_plain_window_add_nothing_extra() {
        let s = sources(&|w| Some(w.raw() as u32), &|_| String::new(), &|_| false);
        let ops = compositor_event_ops(&BackendEvent::WindowMapped(win(7)), ROOT, OVERLAY, &s);
        assert_eq!(
            ops,
            vec![CompositorEventOp::AddWindow {
                window: 7,
                x: 10,
                y: 20,
                width: 300,
                height: 400
            }]
        );
    }

    #[test]
    fn unmap_and_destroy_both_remove_the_window() {
        let s = sources(&|w| Some(w.raw() as u32), &|_| String::new(), &|_| false);
        for event in [
            BackendEvent::WindowUnmapped(win(9)),
            BackendEvent::WindowDestroyed(win(9)),
        ] {
            let ops = compositor_event_ops(&event, ROOT, OVERLAY, &s);
            assert_eq!(ops, vec![CompositorEventOp::RemoveWindow { window: 9 }]);
        }
    }

    #[test]
    fn an_unknown_id_plans_nothing() {
        let s = sources(&|_| None, &|_| String::new(), &|_| false);
        let ops = compositor_event_ops(&BackendEvent::WindowUnmapped(win(9)), ROOT, OVERLAY, &s);
        assert_eq!(ops, vec![]);
    }

    #[test]
    fn geometry_updates_skip_the_overlay_but_not_the_root() {
        let s = sources(&|w| Some(w.raw() as u32), &|_| String::new(), &|_| false);
        let configured = |raw| BackendEvent::WindowConfigured {
            window: win(raw),
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        assert_eq!(
            compositor_event_ops(&configured(u64::from(OVERLAY)), ROOT, OVERLAY, &s),
            vec![]
        );
        // The root resize path is handled by ScreenLayoutChanged, not by
        // filtering configure events; a root configure passes through.
        assert_eq!(
            compositor_event_ops(&configured(u64::from(ROOT)), ROOT, OVERLAY, &s),
            vec![CompositorEventOp::UpdateGeometry {
                window: ROOT,
                x: 1,
                y: 2,
                width: 3,
                height: 4
            }]
        );
    }

    #[test]
    fn fullscreen_requests_map_add_and_toggle_to_fullscreen() {
        let s = sources(&|w| Some(w.raw() as u32), &|_| String::new(), &|_| false);
        for (action, expected) in [
            (NetWmAction::Add, true),
            (NetWmAction::Toggle, true),
            (NetWmAction::Remove, false),
        ] {
            let ops = compositor_event_ops(
                &BackendEvent::WindowStateRequest {
                    window: win(5),
                    state: NetWmState::Fullscreen,
                    action,
                },
                ROOT,
                OVERLAY,
                &s,
            );
            assert_eq!(
                ops,
                vec![CompositorEventOp::SetFullscreen {
                    window: 5,
                    fullscreen: expected
                }]
            );
        }
    }

    #[test]
    fn only_class_property_changes_update_the_class() {
        let s = sources(&|w| Some(w.raw() as u32), &|_| "mpv".to_string(), &|_| {
            false
        });
        let class_change = BackendEvent::PropertyChanged {
            window: win(5),
            kind: PropertyKind::Class,
        };
        assert_eq!(
            compositor_event_ops(&class_change, ROOT, OVERLAY, &s),
            vec![CompositorEventOp::SetWindowClass {
                window: 5,
                class: "mpv".to_string()
            }]
        );
        let title_change = BackendEvent::PropertyChanged {
            window: win(5),
            kind: PropertyKind::Title,
        };
        assert_eq!(
            compositor_event_ops(&title_change, ROOT, OVERLAY, &s),
            vec![]
        );
    }

    #[test]
    fn pointer_activity_moves_the_mouse_and_records_input() {
        let s = sources(&|w| Some(w.raw() as u32), &|_| String::new(), &|_| false);
        let motion = BackendEvent::MotionNotify {
            target: HitTarget::Background { output: None },
            root_x: 640.5,
            root_y: 400.25,
            time: 0,
        };
        assert_eq!(
            compositor_event_ops(&motion, ROOT, OVERLAY, &s),
            vec![
                CompositorEventOp::SetMousePosition {
                    x: 640.5,
                    y: 400.25
                },
                CompositorEventOp::RecordInput,
            ]
        );
    }

    #[test]
    fn a_screen_layout_change_refreshes_the_viewport_and_monitor_maps() {
        let s = sources(&|w| Some(w.raw() as u32), &|_| String::new(), &|_| false);
        assert_eq!(
            compositor_event_ops(&BackendEvent::ScreenLayoutChanged, ROOT, OVERLAY, &s),
            vec![CompositorEventOp::RefreshLayout]
        );
    }
}
