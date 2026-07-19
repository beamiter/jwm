// src/backend/api.rs

use crate::backend::common_define::OutputId;
use crate::backend::common_define::{
    ColorScheme, CursorHandle, KeySym, Mods, Pixel, SchemeType, StdCursorKind, WindowId,
};
use crate::backend::error::BackendError;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::fmt::Debug;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositorMetrics {
    pub fps: f32,
    pub frame_count: u64,
    pub avg_frame_time_ms: f32,
    pub max_frame_time_ms: f32,
    pub min_frame_time_ms: f32,
    /// Recent frame-time tail latency, calculated over the compositor's
    /// bounded sample window. These distinguish smooth averages from jank.
    pub frame_time_p95_ms: f32,
    pub frame_time_p99_ms: f32,
    pub gpu_load_percent: u32,
    pub cpu_load_percent: u32,
    pub draw_calls: u32,
    pub texture_memory_bytes: u64,
    pub blur_cache_hits: u64,
    pub blur_cache_misses: u64,
    pub blur_cache_hit_rate: f32,
    // P4: Temporal blur reuse metrics
    pub temporal_blur_reuse_count: u64,
    pub temporal_blur_total_count: u64,
    pub temporal_blur_reuse_rate: f32,
    pub dirty_regions_count: usize,
    pub dirty_fraction_percent: f32,
    pub window_count: usize,
    pub blur_quality: String,
    pub vrr_enabled: bool,
    pub vrr_active: bool, // VRR currently active for focused game window
    pub current_refresh_rate: u32, // Current target refresh rate (Hz)
    // Task 8: Input latency metrics
    pub input_latency_avg_ms: f32,
    pub input_latency_p50_ms: f32,
    pub input_latency_p95_ms: f32,
    pub input_latency_p99_ms: f32,
    // Phase 2-3: Optimization statistics
    pub direct_scanout_active: bool,
    pub direct_scanout_count: u64,
    pub direct_scanout_bypass_time_ms: u64,
    pub gl_state_changes_avoided: u32,
    pub profiling_enabled: bool,
    pub dirty_region_merge_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    Surface(WindowId),
    Background { output: Option<OutputId> },
}

#[derive(Clone, Debug)]
pub struct OutputIdentity {
    pub connector: String,
    pub vendor: Option<String>,
    pub product_code: Option<u16>,
    pub serial_number: Option<u32>,
    pub monitor_name: Option<String>,
    pub monitor_serial: Option<String>,
    pub stable_key: String,
}

impl OutputIdentity {
    pub fn connector_only(connector: impl Into<String>) -> Self {
        let connector = connector.into();
        Self {
            stable_key: connector.clone(),
            connector,
            vendor: None,
            product_code: None,
            serial_number: None,
            monitor_name: None,
            monitor_serial: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub id: OutputId,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f32,
    pub refresh_rate: u32,
    pub hdr_capable: bool,
    pub hdr_metadata: Option<crate::backend::edid::EdidHdrCapabilities>,
    pub identity: OutputIdentity,
}

#[derive(Clone, Debug)]
pub struct VrrCapabilities {
    pub supported: bool,
    pub current_enabled: bool,
    pub min_refresh_hz: u32,
    pub max_refresh_hz: u32,
}

/// Per-CRTC KMS color pipeline capabilities. A `_size` of 0 indicates the LUT
/// hardware is absent (and the matching `_supported` flag will be false). When
/// `_supported` is true, `_size` is the number of `drm_color_lut` entries the
/// kernel expects in a `*_LUT` blob. Future SOTA work uses these to offload
/// the encode/decode/CTM passes from the GL shader to fixed-function hardware.
#[derive(Clone, Debug, Default)]
pub struct KmsColorPipelineCaps {
    pub degamma_lut_supported: bool,
    pub degamma_lut_size: u32,
    pub gamma_lut_supported: bool,
    pub gamma_lut_size: u32,
    pub ctm_supported: bool,
}

/// Snapshot of one surface's wp-color-management-v1 image description, used by
/// the diagnostic IPC. All numeric fields are taken directly from the protocol
/// (named enums as u32, luminances in the protocol's scaled form).
#[derive(Clone, Debug)]
pub struct ColorManagedSurfaceInfo {
    /// Stringified wl_surface ObjectId.
    pub surface_object_id: String,
    /// Compositor-assigned image-description identity (monotonic).
    pub identity: u64,
    pub tf_named: Option<u32>,
    pub tf_power: Option<u32>,
    pub primaries_named: Option<u32>,
    pub primaries: Option<[i32; 8]>,
    pub min_lum: Option<u32>,
    pub max_lum: Option<u32>,
    pub reference_lum: Option<u32>,
    pub mastering_primaries: Option<[i32; 8]>,
    pub mastering_min_lum: Option<u32>,
    pub mastering_max_lum: Option<u32>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

/// Snapshot of the compositor's blur pipeline state, used by the diagnostic
/// IPC. Lets you verify Hz→strength selection and reuse rate without HW.
#[derive(Clone, Debug, Default)]
pub struct BlurStatus {
    /// Current blur strength (downsample levels).
    pub current_strength: u32,
    /// Whether temporal blur reuse is enabled.
    pub temporal_enabled: bool,
    /// EMA of frames where prior blur was reused (0-100).
    pub temporal_reuse_rate_pct: f32,
    /// Live `blur_strength_by_hz` lookup table, sorted ascending by Hz.
    pub hz_table: Vec<(u32, u32)>,
    /// Live per-output refresh rates: (monitor_id, hz). Monitor 0 == primary.
    pub per_monitor_hz: Vec<(u32, u32)>,
    /// Per-monitor blur-quality overrides: (monitor_id, "Full"|"Reduced"|"Minimal").
    pub blur_quality_by_monitor: Vec<(u32, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectScanoutOutputStatus {
    pub output_name: String,
    pub eligible: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectScanoutStatus {
    pub enabled: bool,
    pub active: bool,
    pub current_window: Option<u64>,
    pub scanout_count: u64,
    pub bypass_time_ms: u64,
    pub candidate_count: usize,
    pub compositor_reason: String,
    pub kms_outputs: Vec<DirectScanoutOutputStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresentationTimingOutputStatus {
    pub output_name: String,
    pub refresh_interval_ms: f64,
    pub last_vblank_monotonic_ms: Option<u64>,
    pub last_vblank_ago_ms: Option<u64>,
    pub frame_pending: bool,
    pub frame_pending_for_ms: Option<u64>,
    pub watchdog_timeout_ms: u64,
    pub frame_callback_roots: usize,
    pub visible_surface_count: usize,
    pub send_frame_callbacks: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresentationTimingStatus {
    pub any_frame_pending: bool,
    pub outputs: Vec<PresentationTimingOutputStatus>,
}

#[derive(Clone, Copy, Debug)]
pub struct ScreenInfo {
    pub width: i32,
    pub height: i32,
}

/// Backend-neutral compositor UI drawn above every client.  Input and policy
/// live in JWM; backends only present this snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemUiOverlay {
    pub text: String,
    /// A lock overlay is opaque; other system UI dims the current desktop.
    pub locked: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Capabilities {
    pub can_warp_pointer: bool,
    pub supports_client_list: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetWmState {
    Fullscreen,
    MaximizedVert,
    MaximizedHorz,
    Hidden,
    Above,
    Below,
    DemandsAttention,
    Sticky,
    SkipTaskbar,
    SkipPager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeEdge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// A backend-owned interactive window operation.
///
/// X11 transports use this while tracking an active pointer grab. Keeping the
/// type in the platform contract prevents transports from depending on JWM
/// policy modules.
#[derive(Debug, Clone, Copy)]
pub enum InteractionAction {
    Move,
    Resize(ResizeEdge),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetWmAction {
    Add,
    Remove,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackMode {
    Above,
    Below,
    TopIf,
    BottomIf,
    Opposite,
}

#[derive(Debug, Clone, Default)]
pub struct WindowChanges {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub border_width: Option<u32>,
    pub sibling: Option<WindowId>,
    pub stack_mode: Option<StackMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowType {
    Normal,
    Desktop,
    Dock,
    Toolbar,
    Menu,
    Utility,
    Splash,
    Dialog,
    DropdownMenu,
    PopupMenu,
    Tooltip,
    Notification,
    Combo,
    Dnd,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    Title,
    Class,
    TransientFor,
    SizeHints,
    Urgency,
    WindowType,
    Protocols,
    Strut,
    MotifHints,
    GtkFrameExtents,
    BypassCompositor,
    OpaqueRegion,
    NetWmIcon,
    UserTime,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyMode {
    Normal,
    Grab,
    Ungrab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseResult {
    Graceful,
    Forced,
}

#[derive(Debug, Clone)]
pub struct WindowAttributes {
    pub override_redirect: bool,
    pub map_state_viewable: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub border: u32,
}

/// A single output's requested configuration, produced by the
/// wlr-output-management protocol and applied by the backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfigChange {
    pub name: String,
    pub enabled: bool,
    /// Requested mode as `(width, height, refresh_mhz)`; `None` keeps the current mode.
    pub mode: Option<(i32, i32, i32)>,
    pub position: Option<(i32, i32)>,
    /// wl_output transform numeric value (0..=7); `None` keeps the current transform.
    pub transform: Option<i32>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementFailure {
    pub output_name: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drm_property: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_value: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementOutputSnapshot {
    pub name: String,
    pub stable_key: String,
    pub enabled: bool,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f32,
    pub refresh_rate: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementTransactionStatus {
    pub id: u64,
    pub requested_at_unix_ms: u64,
    pub success: bool,
    pub changes: Vec<OutputConfigChange>,
    pub outputs_before: Vec<OutputManagementOutputSnapshot>,
    pub outputs_after: Vec<OutputManagementOutputSnapshot>,
    pub failed_outputs: Vec<OutputManagementFailure>,
    pub rollback_attempted: bool,
    pub rollback_succeeded: bool,
    pub rollback_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementRejectedConfig {
    pub attempted_at_unix_ms: u64,
    pub serial: u32,
    pub action: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drm_property: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_value: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementStatus {
    pub pending_ack_count: usize,
    pub soft_disabled_outputs: Vec<String>,
    pub last_transaction: Option<OutputManagementTransactionStatus>,
    pub last_rejected: Option<OutputManagementRejectedConfig>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CaptureProtocolStatus {
    pub enabled: bool,
    pub pending_frames: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub screencopy: CaptureProtocolStatus,
    pub image_copy_capture: CaptureProtocolStatus,
    pub image_copy_output_pending_frames: usize,
    pub image_copy_toplevel_pending_frames: usize,
    pub screencopy_queued_total: u64,
    pub screencopy_failed_total: u64,
    pub screencopy_fulfilled_total: u64,
    pub screencopy_render_failed_total: u64,
    pub image_copy_sessions_total: u64,
    pub image_copy_queued_total: u64,
    pub image_copy_failed_total: u64,
    pub image_copy_fulfilled_total: u64,
    pub image_copy_render_failed_total: u64,
    pub image_copy_output_queued_total: u64,
    pub image_copy_toplevel_queued_total: u64,
    pub last_queued_unix_ms: Option<u64>,
    pub last_fulfilled_unix_ms: Option<u64>,
    pub last_failed_unix_ms: Option<u64>,
    pub last_failure_reason: Option<String>,
    pub dmabuf_advertised: bool,
    pub dmabuf_format_count: usize,
    pub cursor_capture_supported: bool,
    pub sensitive_content_masking: bool,
    pub policy: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct XWaylandStatus {
    pub available: bool,
    pub wm_ready: bool,
    pub display: Option<String>,
    pub mapped_window_count: usize,
    pub associated_surface_count: usize,
    pub pending_association_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProtocolBindStatus {
    pub protocol: String,
    pub bind_count: u64,
    pub last_bound_unix_ms: Option<u64>,
}

// --- 事件定义 ---

#[derive(Debug, Clone)]
pub enum BackendEvent {
    // === 硬件与输出 ===
    OutputAdded(OutputInfo),
    OutputRemoved(OutputId),
    OutputChanged(OutputInfo),
    /// Apply a client-requested output configuration (wlr-output-management).
    OutputConfigure {
        changes: Vec<OutputConfigChange>,
    },
    ScreenLayoutChanged,
    ChildProcessExited,
    ConfigChanged,

    // === 窗口生命周期 ===
    WindowCreated(WindowId),
    WindowDestroyed(WindowId),
    WindowMapped(WindowId),
    WindowUnmapped(WindowId),
    WindowConfigured {
        window: WindowId,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },

    ButtonPress {
        target: HitTarget,
        state: u16,
        detail: u8,
        time: u32,
        root_x: f64,
        root_y: f64,
    },
    ButtonRelease {
        target: HitTarget,
        time: u32,
    },
    MotionNotify {
        target: HitTarget,
        root_x: f64,
        root_y: f64,
        time: u32,
    },
    KeyPress {
        keycode: u8,
        state: u16,
        time: u32,
    },
    KeyRelease {
        keycode: u8,
        state: u16,
        time: u32,
    },

    // === 焦点与状态 ===
    EnterNotify {
        window: WindowId,
        subwindow: Option<WindowId>,
        mode: NotifyMode,
        root_x: f64,
        root_y: f64,
    },
    LeaveNotify {
        window: WindowId,
        mode: NotifyMode,
    },
    FocusIn {
        window: WindowId,
    },
    FocusOut {
        window: WindowId,
    },

    // === 客户端请求 (Policy) ===
    ConfigureRequest {
        window: WindowId,
        changes: WindowChanges,
        mask_bits: u16,
    },
    WindowStateRequest {
        window: WindowId,
        action: NetWmAction,
        state: NetWmState,
    },
    PropertyChanged {
        window: WindowId,
        kind: PropertyKind,
    },
    WmKeyboardShortcut {
        keysym: KeySym,
        mods: Mods,
    },
    Expose {
        window: WindowId,
    },
    ActiveWindowMessage {
        window: WindowId,
    },
    /// A pager/taskbar requested graceful close of a window (_NET_CLOSE_WINDOW).
    CloseWindowRequest {
        window: WindowId,
    },
    PingResponse {
        window: WindowId,
    },
    ShapeChanged {
        window: WindowId,
        shaped: bool,
    },
    ClientMessage {
        window: WindowId,
        type_: u32,
        data: [u32; 5],
        format: u8,
    },
    MoveResizeRequest {
        window: WindowId,
        direction: u32,
        button: u32,
    },
    MappingNotify,
    DamageNotify {
        drawable: WindowId,
    },

    // === Touchpad gesture events (Wayland only) ===
    /// A configured 3+ finger swipe gesture has completed and was intercepted
    /// by the compositor (not forwarded to clients).
    GestureSwipeAction {
        fingers: u32,
        /// One of: "left", "right", "up", "down".
        direction: &'static str,
    },

    // === Workspace protocol events ===
    WorkspaceActivate {
        monitor: usize,
        tag_mask: u32,
    },

    // === Output power (DPMS) ===
    OutputPowerSet {
        output_name: String,
        on: bool,
    },

    // === Gamma LUT (night light) ===
    GammaSet {
        output_name: String,
        gamma_size: u32,
        ramp: Vec<u16>,
    },

    // === Foreign toplevel management (taskbar window control) ===
    ForeignToplevelActivate(WindowId),
    ForeignToplevelClose(WindowId),
    ForeignToplevelSetMaximized(WindowId, bool),
    ForeignToplevelSetMinimized(WindowId, bool),
    ForeignToplevelSetFullscreen(WindowId, bool),

    // === Present extension events ===
    PresentComplete {
        window: WindowId,
        serial: u32,
        msc: u64,
        ust: u64,
    },
    PresentIdle {
        window: WindowId,
        serial: u32,
        pixmap: u32,
    },
}

pub trait WindowOps: Send {
    fn set_position(&self, win: WindowId, x: i32, y: i32) -> Result<(), BackendError>;
    fn configure(
        &self,
        win: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        border: u32,
    ) -> Result<(), BackendError>;
    fn set_decoration_style(
        &self,
        win: WindowId,
        border_width: u32,
        border_color: Pixel,
    ) -> Result<(), BackendError>;
    fn raise_window(&self, win: WindowId) -> Result<(), BackendError>;
    fn map_window(&self, win: WindowId) -> Result<(), BackendError>;
    fn unmap_window(&self, win: WindowId) -> Result<(), BackendError>;
    fn close_window(&self, win: WindowId) -> Result<CloseResult, BackendError>;
    fn set_input_focus(&self, win: WindowId) -> Result<(), BackendError>;
    fn set_input_focus_root(&self) -> Result<(), BackendError>;
    fn get_window_attributes(&self, win: WindowId) -> Result<WindowAttributes, BackendError>;
    fn get_geometry(&self, win: WindowId) -> Result<Geometry, BackendError>;
    fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError>;

    fn flush(&self) -> Result<(), BackendError>;

    fn kill_client(&self, win: WindowId) -> Result<(), BackendError>;

    fn apply_window_changes(
        &self,
        win: WindowId,
        changes: WindowChanges,
    ) -> Result<(), BackendError>;

    fn ungrab_all_buttons(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn grab_button_any_anymod(&self, _win: WindowId, _mask: u32) -> Result<(), BackendError> {
        Ok(())
    }
    fn grab_button(
        &self,
        _win: WindowId,
        _btn: u8,
        _mask: u32,
        _mods: Mods,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn change_event_mask(&self, _win: WindowId, _mask: u32) -> Result<(), BackendError> {
        Ok(())
    }
    fn get_tree_child(&self, _win: WindowId) -> Result<Vec<WindowId>, BackendError> {
        Ok(vec![])
    }
    /// Send WM_TAKE_FOCUS client message if the window supports it.
    /// Returns true if the message was sent.
    fn send_take_focus(&self, _win: WindowId) -> Result<bool, BackendError> {
        Ok(false)
    }

    /// Restack windows in order (first = bottom, last = top).
    /// Uses sibling stacking for fewer X11 round-trips.
    /// Default implementation falls back to sequential raise_window.
    fn restack_windows(&self, windows: &[WindowId]) -> Result<(), BackendError> {
        for &win in windows {
            self.raise_window(win)?;
        }
        Ok(())
    }

    fn shape_select_input(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }

    fn get_window_shaped(&self, _win: WindowId) -> bool {
        false
    }
}

pub trait InputOps: Send {
    fn set_cursor(&self, kind: StdCursorKind) -> Result<(), BackendError>;

    fn get_pointer_position(&self) -> Result<(f64, f64), BackendError>;

    /// Return the top-level window directly under the pointer when the backend
    /// can query it independently of the currently grabbed event target.
    ///
    /// X11 active grabs report the grab window as the event target, so modal
    /// capture source selection uses this hook to recover the actual child.
    fn window_under_pointer(&self) -> Result<Option<WindowId>, BackendError> {
        Ok(None)
    }

    fn grab_pointer(&self, mask: u32, cursor: Option<u64>) -> Result<bool, BackendError>;

    fn ungrab_pointer(&self) -> Result<(), BackendError>;

    fn warp_pointer(&self, _x: f64, _y: f64) -> Result<(), BackendError> {
        Ok(())
    }

    fn query_pointer_root(&self) -> Result<(i32, i32, u16, u16), BackendError>;
    fn warp_pointer_to_window(&self, _win: WindowId, _x: i16, _y: i16) -> Result<(), BackendError> {
        Ok(())
    }
    fn allow_events(
        &self,
        _mode: crate::backend::api::AllowMode,
        _time: u32,
    ) -> Result<(), BackendError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerSurfaceInfo {
    /// wlr-layer-shell exclusive zone semantics.
    /// - `0`: does not reserve space
    /// - `-1`: reserve the full surface dimension along the anchored edge
    /// - `>0`: reserve that many logical pixels
    pub exclusive_zone: i32,
    pub anchor_top: bool,
    pub anchor_bottom: bool,
    pub anchor_left: bool,
    pub anchor_right: bool,
}

pub trait PropertyOps: Send {
    fn get_title(&self, win: WindowId) -> String;
    fn get_class(&self, win: WindowId) -> (String, String); // (instance, class)
    fn get_window_types(&self, win: WindowId) -> Vec<WindowType>;

    fn is_fullscreen(&self, win: WindowId) -> bool;
    fn set_fullscreen_state(&self, win: WindowId, on: bool) -> Result<(), BackendError>;

    fn transient_for(&self, win: WindowId) -> Option<WindowId>;

    // Hints
    fn get_wm_hints(&self, win: WindowId) -> Option<crate::backend::api::WmHints>;
    fn set_urgent_hint(&self, win: WindowId, urgent: bool) -> Result<(), BackendError>;
    fn fetch_normal_hints(
        &self,
        win: WindowId,
    ) -> Result<Option<crate::backend::api::NormalHints>, BackendError>;

    fn set_window_strut_top(
        &self,
        win: WindowId,
        top: u32,
        start_x: u32,
        end_x: u32,
    ) -> Result<(), BackendError>;
    fn set_window_type_dock(&self, win: WindowId) -> Result<(), BackendError>;
    fn clear_window_strut(&self, win: WindowId) -> Result<(), BackendError>;

    fn get_wm_state(&self, win: WindowId) -> Result<i64, BackendError>;
    fn set_wm_state(&self, win: WindowId, state: i64) -> Result<(), BackendError>;

    fn set_client_info_props(
        &self,
        win: WindowId,
        tags: u32,
        monitor_num: u32,
    ) -> Result<(), BackendError>;

    fn get_window_strut_partial(&self, _win: WindowId) -> Option<StrutPartial> {
        None
    }

    fn get_layer_surface_info(&self, _win: WindowId) -> Option<LayerSurfaceInfo> {
        None
    }

    /// Get the PID of the process that owns this window
    fn get_window_pid(&self, _win: WindowId) -> Option<u32> {
        None
    }

    // --- Phase 1: EWMH compliance ---

    fn set_net_wm_state_flag(
        &self,
        _win: WindowId,
        _state: NetWmState,
        _on: bool,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn set_frame_extents(
        &self,
        _win: WindowId,
        _left: u32,
        _right: u32,
        _top: u32,
        _bottom: u32,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn set_allowed_actions(
        &self,
        _win: WindowId,
        _actions: &[AllowedAction],
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn send_ping(&self, _win: WindowId, _timestamp: u32) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn get_user_time(&self, _win: WindowId) -> Option<u32> {
        None
    }

    fn get_net_wm_icon(&self, _win: WindowId) -> Option<Vec<IconData>> {
        None
    }

    fn get_bypass_compositor(&self, _win: WindowId) -> Option<u32> {
        None
    }

    fn get_opaque_region(&self, _win: WindowId) -> Option<Vec<(i32, i32, u32, u32)>> {
        None
    }

    fn get_motif_hints(&self, _win: WindowId) -> Option<MotifWmHints> {
        None
    }

    fn get_gtk_frame_extents(&self, _win: WindowId) -> Option<[u32; 4]> {
        None
    }

    fn get_sync_counter(&self, _win: WindowId) -> Option<u32> {
        None
    }

    fn send_sync_request(
        &self,
        _win: WindowId,
        _counter: u32,
        _value: u64,
    ) -> Result<(), BackendError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrutPartial {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
    pub left_start_y: u32,
    pub left_end_y: u32,
    pub right_start_y: u32,
    pub right_end_y: u32,
    pub top_start_x: u32,
    pub top_end_x: u32,
    pub bottom_start_x: u32,
    pub bottom_end_x: u32,
}

pub struct WmHints {
    pub urgent: bool,
    pub input: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NormalHints {
    pub base_w: i32,
    pub base_h: i32,
    pub inc_w: i32,
    pub inc_h: i32,
    pub max_w: i32,
    pub max_h: i32,
    pub min_w: i32,
    pub min_h: i32,
    pub min_aspect: f32,
    pub max_aspect: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedAction {
    Move,
    Resize,
    Minimize,
    MaximizeHorz,
    MaximizeVert,
    Fullscreen,
    Close,
    Stick,
    Above,
    Below,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MotifWmHints {
    pub flags: u32,
    pub functions: u32,
    pub decorations: u32,
    pub input_mode: i32,
    pub status: u32,
}

impl MotifWmHints {
    pub fn has_decorations_hint(&self) -> bool {
        self.flags & 0x2 != 0
    }
    pub fn decorations_none(&self) -> bool {
        self.has_decorations_hint() && self.decorations == 0
    }
}

#[derive(Debug, Clone)]
pub struct IconData {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

pub trait OutputOps: Send {
    /// 获取当前所有连接的输出设备
    fn enumerate_outputs(&self) -> Vec<OutputInfo>;
    /// 获取主屏幕信息 (兼容旧接口)
    fn screen_info(&self) -> ScreenInfo;

    fn output_at(&self, x: i32, y: i32) -> Option<OutputId>;

    /// Invalidate cached output layout (no-op for backends that don't cache)
    fn invalidate_output_cache(&self) {}

    /// Set hardware gamma ramp for an output (XRandR CRTC gamma)
    fn set_gamma_ramp(
        &self,
        _output: OutputId,
        _red: &[u16],
        _green: &[u16],
        _blue: &[u16],
    ) -> Result<(), BackendError> {
        Ok(())
    }

    /// Get current gamma ramp for an output
    fn get_gamma_ramp(&self, _output: OutputId) -> Option<(Vec<u16>, Vec<u16>, Vec<u16>)> {
        None
    }
}

pub trait KeyOps: Send {
    // 注册全局快捷键
    fn grab_keys(&self, root: WindowId, bindings: &[(Mods, KeySym)]) -> Result<(), BackendError>;
    fn clear_key_grabs(&self, root: WindowId) -> Result<(), BackendError>;

    /// Grab the entire keyboard so all key events are delivered to the WM.
    /// Used for modal states like overview mode.
    fn grab_keyboard(&self, _root: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    /// Release the keyboard grab.
    fn ungrab_keyboard(&self) -> Result<(), BackendError> {
        Ok(())
    }

    // 辅助转换
    fn clean_mods(&self, raw_state: u16) -> Mods;
    fn keysym_from_keycode(&mut self, keycode: u8) -> Result<KeySym, BackendError>;
    fn clear_cache(&mut self);
}

pub trait EwmhFacade: Send {
    fn set_active_window(&self, win: WindowId) -> Result<(), BackendError>;
    fn clear_active_window(&self) -> Result<(), BackendError>;
    fn set_client_list(&self, list: &[WindowId]) -> Result<(), BackendError>;
    fn set_client_list_stacking(&self, list: &[WindowId]) -> Result<(), BackendError>;
    fn setup_supporting_wm_check(&self, wm_name: &str) -> Result<WindowId, BackendError>;
    fn declare_supported(&self, features: &[EwmhFeature]) -> Result<(), BackendError>;
    fn reset_root_properties(&self) -> Result<(), BackendError>;
    fn set_desktop_info(
        &self,
        current: u32,
        total: u32,
        names: &[&str],
    ) -> Result<(), BackendError>;
    fn set_workarea(&self, _areas: &[(i32, i32, u32, u32)]) -> Result<(), BackendError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EwmhFeature {
    ActiveWindow,
    Supported,
    WmName,
    WmState,
    SupportingWmCheck,
    WmStateFullscreen,
    WmStateMaximizedVert,
    WmStateMaximizedHorz,
    WmStateHidden,
    WmStateAbove,
    WmStateBelow,
    WmStateDemandsAttention,
    WmStateSticky,
    WmStateSkipTaskbar,
    WmStateSkipPager,
    ClientList,
    ClientInfo,
    WmWindowType,
    WmWindowTypeDialog,
    CurrentDesktop,
    NumberOfDesktops,
    DesktopNames,
    DesktopViewport,
    WmMoveResize,
    FrameExtents,
    WmAllowedActions,
    Workarea,
    CloseWindow,
    RestackWindow,
    WmPing,
    WmUserTime,
    WmIcon,
    WmBypassCompositor,
    WmOpaqueRegion,
}

pub trait ColorAllocator: Send {
    fn set_scheme(&mut self, t: SchemeType, s: ColorScheme);
    fn allocate_schemes_pixels(&mut self) -> Result<(), BackendError>;
    fn get_border_pixel_of(&mut self, t: SchemeType) -> Result<Pixel, BackendError>;
    fn free_all_theme_pixels(&mut self) -> Result<(), BackendError>;
}

pub trait CursorProvider: Send {
    fn preload_common(&mut self) -> Result<(), BackendError>;
    fn get(&mut self, kind: StdCursorKind) -> Result<CursorHandle, BackendError>;
    fn apply(&mut self, window_id: WindowId, kind: StdCursorKind) -> Result<(), BackendError>;
    fn cleanup(&mut self) -> Result<(), BackendError>;
}

/// Benchmark capability exposed by compositor-backed platforms.
///
/// Keeping this separate lets orchestration and IPC depend on a focused port
/// and allows non-compositing backends to use the no-op defaults.
pub trait CompositorBenchmark: Send {
    /// Start collecting `frames` samples after `warmup` frames.
    fn compositor_benchmark_start(&mut self, _frames: u32, _warmup: u32) -> bool {
        false
    }

    fn compositor_benchmark_stop(&mut self) -> Option<String> {
        None
    }

    fn compositor_benchmark_report(&self) -> Option<String> {
        None
    }

    fn compositor_benchmark_is_complete(&self) -> bool {
        false
    }

    fn compositor_benchmark_set_auto_exit(&mut self, _enabled: bool) {}
}

/// Read-only operational information exposed by a backend.
///
/// This focused interface starts with performance telemetry. Protocol and
/// output status snapshots can migrate here incrementally without growing the
/// control surface of `Backend` further.
pub trait BackendDiagnostics: Send {
    fn compositor_fps(&self) -> f32 {
        0.0
    }

    fn compositor_get_metrics(&self) -> Option<CompositorMetrics> {
        None
    }

    fn compositor_tearing_hint_count(&self) -> usize {
        0
    }

    fn compositor_session_lock_surface_count(&self) -> usize {
        0
    }

    fn compositor_session_locked(&self) -> bool {
        false
    }

    fn compositor_color_managed_surfaces(&self) -> Vec<ColorManagedSurfaceInfo> {
        Vec::new()
    }

    fn compositor_blur_status(&self) -> Option<BlurStatus> {
        None
    }

    fn compositor_direct_scanout_status(&self) -> Option<DirectScanoutStatus> {
        None
    }

    fn compositor_presentation_timing_status(&self) -> Option<PresentationTimingStatus> {
        None
    }

    fn compositor_output_management_status(&self) -> Option<OutputManagementStatus> {
        None
    }

    fn compositor_capture_status(&self) -> Option<CaptureStatus> {
        None
    }

    fn compositor_xwayland_status(&self) -> Option<XWaylandStatus> {
        None
    }

    fn compositor_protocol_bind_counts(&self) -> Vec<ProtocolBindStatus> {
        Vec::new()
    }
}

/// Runtime controls for compositor-wide visual state.
pub trait CompositorControl: Send {
    fn compositor_set_color_temperature(&mut self, _temperature: f32) {}
    fn compositor_set_saturation(&mut self, _saturation: f32) {}
    fn compositor_set_brightness(&mut self, _brightness: f32) {}
    fn compositor_set_contrast(&mut self, _contrast: f32) {}
    fn compositor_set_invert_colors(&mut self, _invert: bool) {}
    fn compositor_set_grayscale(&mut self, _grayscale: bool) {}
    fn compositor_set_debug_hud(&mut self, _enabled: bool) {}
    fn compositor_set_debug_hud_extended(&mut self, _enabled: bool) {}

    fn compositor_toggle_waterlily_effect(&mut self) -> Option<bool> {
        None
    }

    fn compositor_set_transition_mode(&mut self, _mode: &str) {}
    fn compositor_apply_config(&mut self) {}
}

/// Capture, thumbnail, recording and media-timing operations.
pub trait CompositorMedia: Send {
    fn take_screenshot_to_file(&mut self, _path: &std::path::Path) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn take_screenshot_region_to_file(
        &mut self,
        _path: &std::path::Path,
        _x: i32,
        _y: i32,
        _width: u32,
        _height: u32,
    ) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn compositor_capture_thumbnail(
        &self,
        _window: WindowId,
        _max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        None
    }

    fn compositor_request_live_thumbnail(
        &mut self,
        _window: u32,
        _max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        None
    }

    fn compositor_start_recording(&mut self, _path: &str) {}
    fn compositor_start_recording_region(&mut self, path: &str, region: (i32, i32, u32, u32)) {
        self.compositor_set_recording_region(region);
        self.compositor_start_recording(path);
    }
    fn compositor_set_recording_region(&mut self, _region: (i32, i32, u32, u32)) {}
    fn compositor_set_recording_region_overlay(&mut self, _region: Option<(i32, i32, u32, u32)>) {}
    fn compositor_stop_recording(&mut self) {}

    fn compositor_notify_audio_timing(
        &mut self,
        _window: WindowId,
        _fps: f32,
        _buffer_latency_ms: u32,
    ) {
    }
}

/// Workspace transition and interactive preview effects.
pub trait CompositorWorkspaceEffects: Send {
    fn compositor_set_system_ui(&mut self, _overlay: Option<SystemUiOverlay>) {}
    fn compositor_notify_tag_switch(
        &mut self,
        _duration: std::time::Duration,
        _direction: i32,
        _exclude_top: u32,
        _monitor_rect: (i32, i32, u32, u32),
    ) {
    }

    fn compositor_set_magnifier(&mut self, _enabled: bool) {}
    fn compositor_set_snap_preview(&mut self, _preview: Option<(f32, f32, f32, f32)>) {}
    fn compositor_clear_snap_preview_immediate(&mut self) {}

    fn compositor_set_overview_mode(
        &mut self,
        _active: bool,
        _windows: &[(WindowId, f32, f32, f32, f32, bool, String)],
    ) {
    }

    fn compositor_set_overview_monitor(&mut self, _x: i32, _y: i32, _width: u32, _height: u32) {}
    fn compositor_set_monitors(&mut self, _monitors: &[(u32, i32, i32, u32, u32, u32)]) {}
    fn compositor_set_overview_selection(&mut self, _window: WindowId) {}

    fn compositor_set_expose_mode(
        &mut self,
        _active: bool,
        _windows: Vec<(WindowId, i32, i32, u32, u32)>,
    ) {
    }

    fn compositor_expose_click(&mut self, _x: f32, _y: f32) -> Option<WindowId> {
        None
    }
}

/// Per-window compositor visual state.
pub trait CompositorWindowEffects: Send {
    fn compositor_set_frame_extents(
        &mut self,
        _window: WindowId,
        _left: u32,
        _right: u32,
        _top: u32,
        _bottom: u32,
    ) {
    }

    fn compositor_set_window_shaped(&mut self, _window: WindowId, _shaped: bool) {}
    fn compositor_set_window_urgent(&mut self, _window: WindowId, _urgent: bool) {}
    fn compositor_set_window_pip(&mut self, _window: WindowId, _pip: bool) {}
    fn compositor_force_full_redraw(&mut self) {}
    fn compositor_set_mouse_position(&mut self, _x: f32, _y: f32) {}
    fn compositor_deactivate_edge_glow(&mut self) {}
    fn compositor_unsuppress_edge_glow(&mut self) {}
    fn compositor_notify_window_move_start(&mut self, _window: WindowId) {}
    fn compositor_notify_window_move_delta(&mut self, _window: WindowId, _dx: f32, _dy: f32) {}
    fn compositor_notify_window_move_end(&mut self, _window: WindowId) {}
    fn compositor_set_window_minimized(&mut self, _window: WindowId, _minimized: bool) {}
    fn compositor_set_dock_position(&mut self, _x: f32, _y: f32) {}
    fn compositor_set_peek_mode(&mut self, _active: bool) {}
    fn compositor_set_window_groups(&mut self, _groups: Vec<(u32, Vec<(u32, String, bool)>)>) {}
    fn compositor_zoom_to_fit(&mut self, _window: Option<u32>) {}
}

/// Accessibility color correction and interactive screen annotations.
pub trait CompositorAnnotation: Send {
    fn compositor_set_colorblind_mode(&mut self, _mode: &str) {}
    fn compositor_set_annotation_mode(&mut self, _active: bool) {}
    fn compositor_set_annotation_color(&mut self, _rgba: [f32; 4]) {}
    fn compositor_set_annotation_line_width(&mut self, _width: f32) {}
    fn compositor_annotation_add_point(&mut self, _x: f32, _y: f32) {}
    fn compositor_annotation_begin_stroke(&mut self) {}
}

/// Output hardware capabilities and runtime display controls.
pub trait DisplayControl: Send {
    fn query_vrr_capabilities(&self, _output: OutputId) -> Option<VrrCapabilities> {
        None
    }
    fn query_kms_color_pipeline_caps(&self, _output: OutputId) -> Option<KmsColorPipelineCaps> {
        None
    }
    fn set_vrr_enabled(&mut self, _output: OutputId, _enabled: bool) -> Result<(), BackendError> {
        Ok(())
    }
    fn set_hdr_metadata(&mut self, _output: OutputId, _enabled: bool) -> Result<(), BackendError> {
        Err(BackendError::Unsupported(
            "HDR metadata push not implemented",
        ))
    }
}

/// Lightweight compositor scheduling and state queries.
pub trait RenderScheduler: Send {
    fn request_render(&mut self) {}
    fn has_compositor(&self) -> bool {
        false
    }
    fn compositor_needs_render(&self) -> bool {
        false
    }
    fn compositor_overlay_window(&self) -> Option<WindowId> {
        None
    }
}

pub trait EventHandler {
    fn handle_event(
        &mut self,
        backend: &mut dyn Backend,
        event: BackendEvent,
    ) -> Result<(), BackendError>;

    fn update(&mut self, backend: &mut dyn Backend) -> Result<(), BackendError>;

    fn should_exit(&self) -> bool;

    /// Returns true when the handler has active work or a deadline that is due
    /// and needs the event loop to tick now.
    fn needs_tick(&self) -> bool {
        false
    }

    /// Returns the maximum duration an event loop may sleep before calling
    /// [`EventHandler::update`] again. Event loops with their own periodic
    /// timers may ignore this; loops that otherwise block indefinitely should
    /// include it in their dispatch timeout. `Duration::ZERO` means the update
    /// is due now.
    fn next_wakeup(&self) -> Option<std::time::Duration> {
        None
    }

    /// Immediately render the compositor if it has pending damage.
    /// Called from the event loop right after processing X events to
    /// minimise visual latency for rapidly-updating overlay windows
    /// (e.g. flameshot screenshot selection).  The default is a no-op.
    fn render_compositor_immediate(&mut self, _backend: &mut dyn Backend) {}
}

pub trait Backend:
    CompositorBenchmark
    + BackendDiagnostics
    + CompositorControl
    + CompositorMedia
    + CompositorWorkspaceEffects
    + CompositorWindowEffects
    + CompositorAnnotation
    + DisplayControl
    + RenderScheduler
{
    fn capabilities(&self) -> Capabilities;
    fn root_window(&self) -> Option<WindowId>;
    fn as_any(&self) -> &dyn Any;
    fn check_existing_wm(&self) -> Result<(), BackendError>;

    // Ops Getters
    fn window_ops(&self) -> &dyn WindowOps;
    fn input_ops(&self) -> &dyn InputOps;
    fn property_ops(&self) -> &dyn PropertyOps;
    fn output_ops(&self) -> &dyn OutputOps;
    fn key_ops(&self) -> &dyn KeyOps;
    fn key_ops_mut(&mut self) -> &mut dyn KeyOps;
    fn cursor_provider(&mut self) -> &mut dyn CursorProvider;
    fn color_allocator(&mut self) -> &mut dyn ColorAllocator;

    fn register_wm(&self, _name: &str) -> Result<(), BackendError> {
        Ok(())
    }

    // 通用清理接口
    fn cleanup(&mut self) -> Result<(), BackendError> {
        Ok(())
    }

    fn on_focused_client_changed(&mut self, _win: Option<WindowId>) -> Result<(), BackendError> {
        Ok(())
    }
    fn on_client_list_changed(
        &mut self,
        _clients: &[WindowId],
        _stack: &[WindowId],
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn on_desktop_changed(
        &mut self,
        _current: u32,
        _total: u32,
        _names: &[&str],
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn set_workarea(&mut self, _areas: &[(i32, i32, u32, u32)]) -> Result<(), BackendError> {
        Ok(())
    }

    fn begin_move(&mut self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }

    fn begin_resize(&mut self, _win: WindowId, _edge: ResizeEdge) -> Result<(), BackendError> {
        Ok(())
    }

    fn handle_motion(&mut self, _x: f64, _y: f64, _time: u32) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn handle_button_release(&mut self, _time: u32) -> Result<bool, BackendError> {
        Ok(false)
    }

    /// Return the current geometry of the window being dragged/resized, if any.
    /// Used to keep JWM's client.geometry in sync during interactive move/resize.
    fn interaction_geometry(&self) -> Option<(WindowId, i32, i32, u32, u32)> {
        None
    }

    fn run(&mut self, handler: &mut dyn EventHandler) -> Result<(), BackendError>;

    fn compositor_render_frame(
        &mut self,
        _scene: &[(u64, i32, i32, u32, u32)],
        _focused_window: Option<u64>,
    ) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn set_compositor_enabled(&mut self, _enabled: bool) -> Result<bool, BackendError> {
        Ok(false)
    }
    fn has_partial_damage(&self) -> bool {
        false
    }
    fn set_partial_damage(&mut self, _enabled: bool) -> Result<bool, BackendError> {
        Ok(false)
    }
}

// 兼容性定义
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllowMode {
    AsyncPointer,
    ReplayPointer,
    SyncPointer,
    AsyncKeyboard,
    SyncKeyboard,
    ReplayKeyboard,
    AsyncBoth,
    SyncBoth,
}
