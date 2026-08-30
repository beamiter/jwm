//! 特殊功能模块
//!
//! 这个模块包含了窗口管理器的各种特殊功能：
//! - screenshot: 交互式截图选择
//! - overview: 3D 窗口切换器
//! - recording: 屏幕录制
//! - audio_recording: 内置麦克风录音
//! - magnifier: 放大镜
//! - toggles: 所有特性的切换函数

// Without media-audio only the state surface is exercised; the capture
// helpers stay for the gated engines.
#[cfg_attr(not(feature = "media-audio"), allow(dead_code))]
pub mod audio_recording;
pub mod calendar;
pub mod capture;
pub mod capture_plan;
pub mod clipboard;
pub mod connectivity;
pub mod deferred_grab;
pub mod expose_plan;
mod external_command;
pub mod idle;
pub mod launcher;
pub mod layout_picker;
pub mod magnifier;
pub mod media;
pub mod notifications;
pub mod overview;
pub mod overview_plan;
pub mod power;
pub mod recording;
pub mod recording_plan;
pub mod resources;
pub mod screenshot;
pub mod session;
pub mod shell_hub;
pub mod system_controls;
pub mod system_ui;
pub mod toggles;
pub mod wallpaper;
pub mod wallpaper_colors;

pub use audio_recording::AudioRecordingState;
pub use calendar::CalendarView;
pub use capture::{CaptureInteractionState, CaptureTarget};
pub use capture_plan::{
    CaptureCompletion, CaptureExecution, CapturePlan, execute_capture_plan, plan_capture_completion,
};
pub use clipboard::{ClipboardEntry, ClipboardHistory};
pub use connectivity::{
    BluetoothDevice, BluetoothState, ConnectivityState, LinkKind, NetworkState, WifiNetwork,
};
pub use deferred_grab::{DeferredGrab, DeferredGrabAction};
pub use expose_plan::ExposeAction;
pub use layout_picker::LayoutPickerState;
pub use magnifier::MagnifierState;
pub use media::{MediaCommand, MediaState, MediaStatus, PlaybackStatus};
pub use notifications::{NotificationCenter, NotificationRecord, NotificationRequest};
pub use overview::OverviewState;
pub use overview_plan::CyclePlan;
pub use power::{BatteryState, ChargeStatus, LowBatteryWarner};
pub use recording::RecordingState;
pub use recording_plan::FinalizationPlan;
pub use resources::{MemoryUsage, ResourceState, Throughput};
pub use screenshot::ScreenshotState;
pub use session::SessionAction;
pub use shell_hub::ShellHubRoute;
pub use system_ui::{
    ControlCenterInputs, ControlKind, MonitorDirection, MonitorLayoutEntry, SystemUiState,
};

/// Durable diagnostics for runtime compositor hand-offs.
///
/// This is deliberately WM-owned rather than renderer-owned: a failed enable
/// can leave no renderer from which IPC could retrieve the failure details.
#[derive(Debug, Default)]
pub struct CompositorTransitionState {
    pub attempts: u64,
    pub last_requested_active: Option<bool>,
    pub last_attempt_unix_ms: Option<u64>,
    pub last_success: Option<bool>,
    pub last_error: Option<String>,
}

impl CompositorTransitionState {
    pub(crate) fn begin(&mut self, requested_active: bool, unix_ms: Option<u64>) {
        self.attempts = self.attempts.saturating_add(1);
        self.last_requested_active = Some(requested_active);
        self.last_attempt_unix_ms = unix_ms;
        self.last_success = None;
        self.last_error = None;
    }

    pub(crate) fn succeed(&mut self) {
        self.last_success = Some(true);
        self.last_error = None;
    }

    pub(crate) fn fail(&mut self, error: impl Into<String>) {
        self.last_success = Some(false);
        self.last_error = Some(error.into());
    }
}

/// 所有特性的组合状态
#[derive(Debug, Default)]
pub struct FeatureStates {
    /// Runtime compositor hand-off diagnostics retained even while the
    /// renderer is absent.
    pub compositor_transition: CompositorTransitionState,
    pub audio_recording: AudioRecordingState,
    pub capture: CaptureInteractionState,
    /// A request that needs the pointer, parked until whoever holds it — a
    /// status bar mid-click, most often — lets go. See `deferred_grab`.
    pub deferred_grab: Option<DeferredGrab>,
    pub screenshot: ScreenshotState,
    pub overview: OverviewState,
    pub recording: RecordingState,
    pub magnifier: MagnifierState,
    /// Last known MPRIS player, pushed in by the bridge. Not a mode.
    pub media: MediaStatus,
    /// Latest battery reading, refreshed by the poll in `tick_animations`.
    pub battery: Option<BatteryState>,
    /// Latest CPU/memory/network reading, refreshed on its own interval by
    /// the same tick.
    pub resources: ResourceState,
    pub resource_sampler: resources::ResourceSampler,
    /// Clipboard history. Memory only, never written to disk.
    pub clipboard: ClipboardHistory,
    /// Last complete set of slow Shell Hub controls. The panel itself only
    /// reads this memory; external tools are sampled by `control_snapshot_job`.
    pub control_snapshot: Option<system_controls::ControlCenterSnapshot>,
    /// One coalesced background refresh. The epoch travels with the value so
    /// a read started before a user mutation cannot roll that mutation back.
    pub control_snapshot_job:
        Option<connectivity::BackgroundJob<(u64, system_controls::ControlCenterSnapshot)>>,
    pub control_snapshot_refreshed_at: Option<std::time::Instant>,
    pub control_snapshot_epoch: u64,
    /// Latest Wi-Fi/Bluetooth reading, refreshed on the same poll and
    /// whenever the control center opens.
    pub connectivity: ConnectivityState,
    /// Background Wi-Fi/Bluetooth state read, if one is in flight. nmcli can
    /// block for seconds, so the poll never runs on the event loop.
    pub connectivity_poll: Option<connectivity::BackgroundJob<ConnectivityState>>,
    /// Scan running for an open Wi-Fi picker, if any.
    pub wifi_scan: Option<connectivity::BackgroundJob<Vec<WifiNetwork>>>,
    /// Connection attempt running for an open Wi-Fi picker, if any.
    pub wifi_connect: Option<connectivity::BackgroundJob<Result<String, String>>>,
    /// Device list being read for an open Bluetooth picker, if any.
    pub bluetooth_scan: Option<connectivity::BackgroundJob<Vec<BluetoothDevice>>>,
    /// Connect/disconnect running for an open Bluetooth picker, if any.
    pub bluetooth_action: Option<connectivity::BackgroundJob<Result<String, String>>>,
    /// Colour extraction running for a wallpaper, if any.
    pub wallpaper_theme: Option<connectivity::BackgroundJob<Option<wallpaper_colors::Palette>>>,
    /// The wallpaper the last extraction was started for, so a config apply
    /// that changed something else does not decode the same picture again.
    pub themed_wallpaper: String,
    /// The palette in use and the wallpaper it came from, published over IPC
    /// so the status bar can match it. The two travel together because a
    /// wallpaper with no colour to take leaves the previous palette in place,
    /// and reporting the new picture beside the old colours would be a lie.
    pub wallpaper_palette: Option<(String, wallpaper_colors::Palette)>,
    /// Which low-battery warning has already been posted.
    pub low_battery: LowBatteryWarner,
    /// Bounded history behind the notification center. Not a mode: it
    /// survives `disable_all` and never counts as an active feature.
    pub notifications: NotificationCenter,
    /// Built-in lock screen, application launcher, and display layout UI.
    pub system_ui: SystemUiState,
    /// Last complete application catalog. Immutable sharing makes opening the
    /// launcher and cloning its render state independent of catalog size.
    pub launcher_catalog: std::sync::Arc<[system_ui::LaunchEntry]>,
    /// A desktop-entry/PATH scan in flight. It deliberately survives closing
    /// the launcher so the completed catalog is ready on the next opening.
    pub launcher_catalog_job:
        Option<connectivity::BackgroundJob<std::sync::Arc<[system_ui::LaunchEntry]>>>,
    /// Completion time of the last catalog scan, for the bounded stale-while-
    /// revalidate policy in the launcher opener.
    pub launcher_catalog_refreshed_at: Option<std::time::Instant>,
    /// A page opened from the Shell Hub returns there on Escape. Directly
    /// opened panels still close normally.
    pub system_ui_return_to_hub: bool,
    /// The compositor was started only to render the current built-in system
    /// UI. Closing the panel (or unlocking) returns to non-composited mode.
    pub system_ui_temporary_compositor: bool,
    /// Peek 模式 (Boss Key) - 所有窗口淡出
    pub peek_active: bool,
    /// Expose / Mission Control 模式
    pub expose_active: bool,
    /// Annotation (屏幕标注) 模式
    pub annotation_active: bool,
    /// Annotation 正在绘制中（鼠标按住）
    pub annotation_drawing: bool,
}

impl FeatureStates {
    pub fn new() -> Self {
        Self::default()
    }

    /// 检查是否有任何特殊模式激活
    pub fn has_active_feature(&self) -> bool {
        self.screenshot.active
            || self.recording.selecting_region
            || self.system_ui.is_active()
            || self.overview.active
            || self.recording.active
            || self.audio_recording.active
            || self.magnifier.enabled
            || self.peek_active
            || self.expose_active
            || self.annotation_active
    }

    /// 禁用所有特性（紧急退出）
    pub fn disable_all(&mut self) {
        self.screenshot.cancel();
        self.capture = CaptureInteractionState::default();
        self.system_ui.cancel();
        self.system_ui_return_to_hub = false;
        self.overview.deactivate();
        self.recording.cancel();
        let _ = self.audio_recording.stop();
        self.magnifier.disable();
        self.peek_active = false;
        self.expose_active = false;
        self.annotation_active = false;
        self.annotation_drawing = false;
    }

    /// 切换 Peek 模式
    pub fn toggle_peek(&mut self) {
        self.peek_active = !self.peek_active;
    }

    /// 切换 Expose 模式
    pub fn toggle_expose(&mut self) {
        self.expose_active = !self.expose_active;
    }
}

#[cfg(test)]
mod tests {
    use super::CompositorTransitionState;

    #[test]
    fn compositor_transition_state_replaces_stale_failure_on_next_attempt() {
        let mut state = CompositorTransitionState::default();

        state.begin(true, Some(10));
        state.fail("renderer unavailable");
        assert_eq!(state.attempts, 1);
        assert_eq!(state.last_success, Some(false));
        assert_eq!(state.last_error.as_deref(), Some("renderer unavailable"));

        state.begin(false, Some(20));
        assert_eq!(state.attempts, 2);
        assert_eq!(state.last_requested_active, Some(false));
        assert_eq!(state.last_attempt_unix_ms, Some(20));
        assert_eq!(state.last_success, None);
        assert_eq!(state.last_error, None);

        state.succeed();
        assert_eq!(state.last_success, Some(true));
        assert_eq!(state.last_error, None);
    }

    #[test]
    fn compositor_transition_attempt_counter_saturates() {
        let mut state = CompositorTransitionState {
            attempts: u64::MAX,
            ..CompositorTransitionState::default()
        };
        state.begin(true, None);
        assert_eq!(state.attempts, u64::MAX);
    }
}
