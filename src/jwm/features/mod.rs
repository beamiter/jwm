//! 特殊功能模块
//!
//! 这个模块包含了窗口管理器的各种特殊功能：
//! - screenshot: 交互式截图选择
//! - overview: 3D 窗口切换器
//! - recording: 屏幕录制
//! - audio_recording: 内置麦克风录音
//! - magnifier: 放大镜
//! - toggles: 所有特性的切换函数

pub mod audio_recording;
pub mod capture;
pub mod magnifier;
pub mod overview;
pub mod recording;
pub mod screenshot;
pub mod system_ui;
pub mod toggles;

pub use audio_recording::AudioRecordingState;
pub use capture::{CaptureInteractionState, CaptureTarget};
pub use magnifier::MagnifierState;
pub use overview::OverviewState;
pub use recording::RecordingState;
pub use screenshot::ScreenshotState;
pub use system_ui::{MonitorDirection, MonitorLayoutEntry, SystemUiState};

/// 所有特性的组合状态
#[derive(Debug, Default)]
pub struct FeatureStates {
    pub audio_recording: AudioRecordingState,
    pub capture: CaptureInteractionState,
    pub screenshot: ScreenshotState,
    pub overview: OverviewState,
    pub recording: RecordingState,
    pub magnifier: MagnifierState,
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
