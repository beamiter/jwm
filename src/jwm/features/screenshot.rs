//! 交互式截图功能 - 区域选择和保存

use crate::backend::api::Backend;
use crate::backend::common_define::{EventMaskBits, StdCursorKind};
use crate::core::types::Rect;
use crate::jwm::types::WMArgEnum;
use log::{error, info, warn};
use std::process::{Command, Stdio};

/// 截图选择状态
#[derive(Debug, Default, Clone)]
pub struct ScreenshotState {
    /// 截图选择模式是否激活
    pub active: bool,
    /// 是否正在拖动选择区域
    pub dragging: bool,
    /// 选择已完成，等待保存操作
    pub committed: bool,
    /// 选择起始点 (x, y)
    pub start: (f64, f64),
    /// 选择结束点 (x, y)
    pub end: (f64, f64),
    /// 保存路径
    pub output_path: Option<String>,
}

impl ScreenshotState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开始截图选择模式
    pub fn start(&mut self) {
        self.active = true;
        self.dragging = false;
        self.committed = false;
        self.start = (0.0, 0.0);
        self.end = (0.0, 0.0);
    }

    /// 开始拖动选择
    pub fn begin_drag(&mut self, x: f64, y: f64) {
        self.dragging = true;
        self.start = (x, y);
        self.end = (x, y);
    }

    /// 更新拖动位置
    pub fn update_drag(&mut self, x: f64, y: f64) {
        if self.dragging {
            self.end = (x, y);
        }
    }

    /// 完成选择
    pub fn commit(&mut self) {
        if self.dragging {
            self.dragging = false;
            self.committed = true;
        }
    }

    /// 取消截图
    pub fn cancel(&mut self) {
        *self = Self::default();
    }

    /// 获取选择区域矩形
    pub fn get_selection_rect(&self) -> Option<Rect> {
        if !self.committed && !self.dragging {
            return None;
        }

        let (x1, y1) = self.start;
        let (x2, y2) = self.end;

        let x = x1.min(x2) as i32;
        let y = y1.min(y2) as i32;
        let w = (x1 - x2).abs() as i32;
        let h = (y1 - y2).abs() as i32;

        if w > 0 && h > 0 {
            Some(Rect { x, y, w, h })
        } else {
            None
        }
    }

    /// 设置输出路径
    pub fn set_output_path(&mut self, path: String) {
        self.output_path = Some(path);
    }

    /// 获取输出路径
    pub fn take_output_path(&mut self) -> Option<String> {
        self.output_path.take()
    }

    /// 是否需要渲染选择框
    pub fn should_render_selection(&self) -> bool {
        self.active && (self.dragging || self.committed)
    }

    /// 是否正在选择中
    pub fn is_selecting(&self) -> bool {
        self.active && !self.committed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screenshot_workflow() {
        let mut state = ScreenshotState::new();

        // 开始截图
        state.start();
        assert!(state.active);
        assert!(!state.committed);

        // 开始拖动
        state.begin_drag(100.0, 100.0);
        assert!(state.dragging);

        // 更新位置
        state.update_drag(200.0, 200.0);
        assert_eq!(state.end, (200.0, 200.0));

        // 完成选择
        state.commit();
        assert!(!state.dragging);
        assert!(state.committed);

        // 获取选择区域
        let rect = state.get_selection_rect().unwrap();
        assert_eq!(rect.x, 100);
        assert_eq!(rect.y, 100);
        assert_eq!(rect.w, 100);
        assert_eq!(rect.h, 100);
    }

    #[test]
    fn test_cancel() {
        let mut state = ScreenshotState::new();
        state.start();
        state.begin_drag(10.0, 10.0);

        state.cancel();
        assert!(!state.active);
        assert!(!state.dragging);
    }

    #[test]
    fn test_empty_selection() {
        let mut state = ScreenshotState::new();
        state.start();
        state.begin_drag(100.0, 100.0);
        state.update_drag(100.0, 100.0); // 同一点
        state.commit();

        // 零尺寸选择应该返回 None
        assert!(state.get_selection_rect().is_none());
    }
}

// =================================================================================
// 截图处理函数（Jwm 方法扩展）
// =================================================================================

use crate::jwm::Jwm;

impl Jwm {
    /// 准备截图输出路径（交互式和全屏截图共用）
    fn prepare_screenshot_path() -> Option<String> {
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let pictures_dir = std::env::var("XDG_PICTURES_DIR")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{}/Pictures", h)))
            .unwrap_or_else(|_| "/tmp".to_string());
        let mut output_dir = std::path::PathBuf::from(&pictures_dir);
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            warn!(
                "[take_screenshot] cannot create output dir '{}': {}, fallback to /tmp",
                output_dir.display(),
                e
            );
            output_dir = std::path::PathBuf::from("/tmp");
            if let Err(e2) = std::fs::create_dir_all(&output_dir) {
                error!(
                    "[take_screenshot] cannot create fallback dir '{}': {}",
                    output_dir.display(),
                    e2
                );
                return None;
            }
        }
        Some(
            output_dir
                .join(format!("screenshot-{}.png", timestamp))
                .to_string_lossy()
                .to_string(),
        )
    }

    /// Alt+S: 进入交互式区域选择模式
    pub fn take_screenshot(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // If already in selection mode, cancel it first
        if self.features.screenshot.active {
            self.cancel_screenshot_select(backend);
            return Ok(());
        }

        let screenshot_path = match Self::prepare_screenshot_path() {
            Some(p) => p,
            None => return Ok(()),
        };

        info!(
            "[take_screenshot] entering interactive region selection mode → {}",
            screenshot_path
        );
        self.features.screenshot.active = true;
        self.features.screenshot.dragging = false;
        self.features.screenshot.committed = false;
        self.features.screenshot.start = (0.0, 0.0);
        self.features.screenshot.end = (0.0, 0.0);
        self.features.screenshot.output_path = Some(screenshot_path);

        // Grab keyboard (to intercept Escape)
        if let Some(root) = backend.root_window() {
            let _ = backend.key_ops().grab_keyboard(root);
        }
        // Grab pointer with crosshair cursor
        let crosshair_handle = backend
            .cursor_provider()
            .get(StdCursorKind::Crosshair)
            .ok()
            .map(|h| h.0);
        let pointer_mask = (EventMaskBits::BUTTON_PRESS
            | EventMaskBits::BUTTON_RELEASE
            | EventMaskBits::POINTER_MOTION)
            .bits();
        let _ = backend
            .input_ops()
            .grab_pointer(pointer_mask, crosshair_handle);

        Ok(())
    }

    /// Alt+Shift+S: 立即截取全屏
    pub fn take_screenshot_fullscreen(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let screenshot_path = match Self::prepare_screenshot_path() {
            Some(p) => p,
            None => return Ok(()),
        };

        let path = std::path::PathBuf::from(&screenshot_path);
        match backend.take_screenshot_to_file(&path) {
            Ok(true) => {
                info!(
                    "[take_screenshot_fullscreen] compositor screenshot → {}",
                    path.display()
                );
            }
            Ok(false) => {
                info!(
                    "[take_screenshot_fullscreen] backend doesn't support compositor screenshots"
                );
            }
            Err(e) => {
                error!("[take_screenshot_fullscreen] compositor screenshot failed: {e}");
            }
        }
        Ok(())
    }

    /// 取消交互式截图选择模式
    pub(crate) fn cancel_screenshot_select(&mut self, backend: &mut dyn Backend) {
        info!("[take_screenshot] cancelling region selection");
        self.features.screenshot.active = false;
        self.features.screenshot.dragging = false;
        self.features.screenshot.committed = false;
        self.features.screenshot.output_path = None;
        if backend.has_compositor() {
            backend.compositor_set_snap_preview(None);
        }
        let _ = backend.key_ops().ungrab_keyboard();
        let _ = backend.input_ops().ungrab_pointer();
        // Restore default cursor
        if let Some(root) = backend.root_window() {
            let _ = backend
                .cursor_provider()
                .apply(root, StdCursorKind::LeftPtr);
        }
    }

    /// 完成交互式截图选择：捕获选中的区域
    ///
    /// 如果 `to_clipboard` 为 true，图片会复制到系统剪贴板而不是保存到文件
    pub(crate) fn finish_screenshot_select(&mut self, backend: &mut dyn Backend, to_clipboard: bool) {
        let path_str = match self.features.screenshot.output_path.take() {
            Some(p) => p,
            None => {
                self.cancel_screenshot_select(backend);
                return;
            }
        };

        let (sx, sy) = self.features.screenshot.start;
        let (ex, ey) = self.features.screenshot.end;

        // Compute normalized rectangle
        let x = sx.min(ex) as i32;
        let y = sy.min(ey) as i32;
        let w = (sx - ex).abs() as u32;
        let h = (sy - ey).abs() as u32;

        // Clear state before capturing
        self.features.screenshot.active = false;
        self.features.screenshot.dragging = false;
        self.features.screenshot.committed = false;
        if backend.has_compositor() {
            backend.compositor_clear_snap_preview_immediate();
        }
        let _ = backend.key_ops().ungrab_keyboard();
        let _ = backend.input_ops().ungrab_pointer();
        if let Some(root) = backend.root_window() {
            let _ = backend
                .cursor_provider()
                .apply(root, StdCursorKind::LeftPtr);
        }

        if w < 3 || h < 3 {
            info!(
                "[take_screenshot] selection too small ({}x{}), ignoring",
                w, h
            );
            return;
        }

        // When copying to clipboard, use a temp file as an intermediate
        let save_path = if to_clipboard {
            format!("/tmp/.jwm-screenshot-clipboard-{}.png", std::process::id())
        } else {
            path_str.clone()
        };

        let path = std::path::PathBuf::from(&save_path);
        let captured = match backend.take_screenshot_region_to_file(&path, x, y, w, h) {
            Ok(true) => {
                info!(
                    "[take_screenshot] region screenshot → {} ({}x{} at {},{})",
                    path.display(),
                    w,
                    h,
                    x,
                    y
                );
                true
            }
            Ok(false) => {
                info!(
                    "[take_screenshot] backend doesn't support region screenshots, falling back to full"
                );
                backend.take_screenshot_to_file(&path).unwrap_or(false)
            }
            Err(e) => {
                error!("[take_screenshot] region screenshot failed: {e}");
                false
            }
        };

        if to_clipboard && captured {
            Self::copy_image_to_clipboard(backend, &save_path);
        }
    }

    /// 使用 xclip 或 wl-copy 将 PNG 图片复制到系统剪贴板
    ///
    /// 截图由合成器在下一帧异步捕获，所以 PNG 文件在调用时还不存在。
    /// 我们启动一个 shell 脚本轮询等待文件出现后再运行剪贴板工具。
    fn copy_image_to_clipboard(backend: &dyn Backend, png_path: &str) {
        let copy_cmd = if Self::is_udev_backend(backend) {
            format!("wl-copy -t image/png < '{}'", png_path)
        } else {
            format!("xclip -selection clipboard -t image/png -i '{}'", png_path)
        };

        // Poll up to 3 s for the file to appear (compositor writes it next frame),
        // then copy to clipboard and remove the temp file.
        let script = format!(
            r#"for i in $(seq 1 60); do [ -s '{}' ] && {{ {}; rm -f '{}'; exit 0; }}; sleep 0.05; done"#,
            png_path, copy_cmd, png_path,
        );

        info!("[take_screenshot] clipboard copy scheduled: {}", copy_cmd);

        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        match command.spawn() {
            Ok(_) => {
                info!("[take_screenshot] clipboard copy helper spawned");
            }
            Err(e) => {
                error!("[take_screenshot] failed to spawn clipboard helper: {e}");
            }
        }
    }
}
