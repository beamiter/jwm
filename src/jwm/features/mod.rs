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
pub mod expose_plan;
pub mod idle;
pub mod launcher;
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
pub use expose_plan::ExposeAction;
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
pub use system_ui::{
    ControlCenterInputs, ControlKind, MonitorDirection, MonitorLayoutEntry, SystemUiState,
};

/// 所有特性的组合状态
#[derive(Debug, Default)]
pub struct FeatureStates {
    pub audio_recording: AudioRecordingState,
    pub capture: CaptureInteractionState,
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
    /// Audio devices in use at both ends, refreshed when the control center
    /// opens and after a switch.
    pub audio_defaults: system_controls::AudioDefaults,
    /// Latest Wi-Fi/Bluetooth reading, refreshed on the same poll and
    /// whenever the control center opens.
    pub connectivity: ConnectivityState,
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
