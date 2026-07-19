//! 交互式截图功能 - 区域选择和保存

use crate::backend::api::Backend;
use crate::backend::common_define::{EventMaskBits, StdCursorKind};
use crate::core::types::Rect;
use crate::jwm::features::capture::CaptureTarget;
use crate::jwm::types::WMArgEnum;
use image::{Rgba, RgbaImage};
use log::{error, info, warn};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotTool {
    Select,
    Pencil,
    Line,
    Arrow,
    Rectangle,
    Ellipse,
}

impl Default for ScreenshotTool {
    fn default() -> Self {
        Self::Select
    }
}

#[derive(Debug, Clone)]
pub enum ScreenshotAnnotation {
    Freehand {
        points: Vec<(f32, f32)>,
        color: [u8; 4],
        width: u32,
    },
    Line {
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
        width: u32,
    },
    Arrow {
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
        width: u32,
    },
    Rectangle {
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
        width: u32,
    },
    Ellipse {
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
        width: u32,
    },
}

impl ScreenshotAnnotation {
    fn translate(&mut self, dx: f32, dy: f32) {
        let translate_point = |point: &mut (f32, f32)| {
            point.0 += dx;
            point.1 += dy;
        };
        match self {
            Self::Freehand { points, .. } => {
                for point in points {
                    translate_point(point);
                }
            }
            Self::Line { from, to, .. }
            | Self::Arrow { from, to, .. }
            | Self::Rectangle { from, to, .. }
            | Self::Ellipse { from, to, .. } => {
                translate_point(from);
                translate_point(to);
            }
        }
    }
}

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
    /// 当前标注工具
    pub tool: ScreenshotTool,
    /// 当前标注颜色
    pub color: [u8; 4],
    /// 当前标注线宽
    pub line_width: u32,
    /// 已完成的标注
    pub annotations: Vec<ScreenshotAnnotation>,
    /// 正在绘制标注
    pub drawing_annotation: bool,
    /// 当前标注起点
    pub annotation_start: (f32, f32),
    /// 当前标注终点
    pub annotation_end: (f32, f32),
    /// 当前自由绘制点集
    pub current_points: Vec<(f32, f32)>,
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
        self.tool = ScreenshotTool::Select;
        self.color = [255, 70, 70, 255];
        self.line_width = 4;
        self.annotations.clear();
        self.drawing_annotation = false;
        self.current_points.clear();
    }

    pub fn reset_selection(&mut self) {
        self.dragging = false;
        self.committed = false;
        self.start = (0.0, 0.0);
        self.end = (0.0, 0.0);
        self.tool = ScreenshotTool::Select;
        self.annotations.clear();
        self.drawing_annotation = false;
        self.current_points.clear();
    }

    pub fn select_rect(&mut self, rect: Rect) {
        self.reset_selection();
        if rect.w <= 0 || rect.h <= 0 {
            return;
        }
        self.start = (rect.x as f64, rect.y as f64);
        self.end = (
            rect.x.saturating_add(rect.w) as f64,
            rect.y.saturating_add(rect.h) as f64,
        );
        self.committed = true;
        self.tool = ScreenshotTool::Pencil;
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

    pub fn set_tool(&mut self, tool: ScreenshotTool) {
        self.tool = tool;
    }

    pub fn set_palette_color(&mut self, idx: usize) {
        const COLORS: [[u8; 4]; 8] = [
            [255, 70, 70, 255],
            [255, 190, 60, 255],
            [85, 215, 110, 255],
            [80, 170, 255, 255],
            [180, 110, 255, 255],
            [255, 255, 255, 255],
            [30, 30, 30, 255],
            [255, 90, 180, 255],
        ];
        if let Some(color) = COLORS.get(idx) {
            self.color = *color;
        }
    }

    pub fn increase_line_width(&mut self) {
        self.line_width = (self.line_width + 1).min(24);
    }

    pub fn decrease_line_width(&mut self) {
        self.line_width = self.line_width.saturating_sub(1).max(1);
    }

    fn translate_selection(&mut self, dx: f64, dy: f64) {
        if !self.committed || (dx == 0.0 && dy == 0.0) {
            return;
        }
        self.start.0 += dx;
        self.start.1 += dy;
        self.end.0 += dx;
        self.end.1 += dy;

        let dx = dx as f32;
        let dy = dy as f32;
        for annotation in &mut self.annotations {
            annotation.translate(dx, dy);
        }
        if self.drawing_annotation {
            self.annotation_start.0 += dx;
            self.annotation_start.1 += dy;
            self.annotation_end.0 += dx;
            self.annotation_end.1 += dy;
        }
        for point in &mut self.current_points {
            point.0 += dx;
            point.1 += dy;
        }
    }

    pub fn move_selection(&mut self, dx: f64, dy: f64) {
        self.translate_selection(dx, dy);
    }

    pub fn move_selection_within(&mut self, dx: f64, dy: f64, bounds: Rect) {
        let Some(rect) = self.get_selection_rect() else {
            return;
        };
        if bounds.w <= 0 || bounds.h <= 0 {
            return;
        }

        let max_x = (bounds.x + bounds.w - rect.w).max(bounds.x);
        let max_y = (bounds.y + bounds.h - rect.h).max(bounds.y);
        let next_x = (f64::from(rect.x) + dx).clamp(f64::from(bounds.x), f64::from(max_x));
        let next_y = (f64::from(rect.y) + dy).clamp(f64::from(bounds.y), f64::from(max_y));
        self.translate_selection(next_x - f64::from(rect.x), next_y - f64::from(rect.y));
    }

    pub fn begin_annotation(&mut self, x: f32, y: f32) {
        self.drawing_annotation = true;
        self.annotation_start = (x, y);
        self.annotation_end = (x, y);
        self.current_points.clear();
        if self.tool == ScreenshotTool::Pencil {
            self.current_points.push((x, y));
        }
    }

    pub fn update_annotation(&mut self, x: f32, y: f32) {
        if !self.drawing_annotation {
            return;
        }
        self.annotation_end = (x, y);
        if self.tool == ScreenshotTool::Pencil {
            self.current_points.push((x, y));
        }
    }

    pub fn commit_annotation(&mut self) {
        if !self.drawing_annotation {
            return;
        }
        let annotation = self.current_annotation_preview();
        self.drawing_annotation = false;
        if let Some(annotation) = annotation {
            self.annotations.push(annotation);
        }
        self.current_points.clear();
    }

    pub fn current_annotation_preview(&self) -> Option<ScreenshotAnnotation> {
        if !self.drawing_annotation {
            return None;
        }
        let color = self.color;
        let width = self.line_width;
        let from = self.annotation_start;
        let to = self.annotation_end;
        match self.tool {
            ScreenshotTool::Pencil if self.current_points.len() > 1 => {
                Some(ScreenshotAnnotation::Freehand {
                    points: self.current_points.clone(),
                    color,
                    width,
                })
            }
            ScreenshotTool::Line => Some(ScreenshotAnnotation::Line {
                from,
                to,
                color,
                width,
            }),
            ScreenshotTool::Arrow => Some(ScreenshotAnnotation::Arrow {
                from,
                to,
                color,
                width,
            }),
            ScreenshotTool::Rectangle => Some(ScreenshotAnnotation::Rectangle {
                from,
                to,
                color,
                width,
            }),
            ScreenshotTool::Ellipse => Some(ScreenshotAnnotation::Ellipse {
                from,
                to,
                color,
                width,
            }),
            _ => None,
        }
    }

    pub fn undo_annotation(&mut self) {
        self.annotations.pop();
    }

    /// 获取选择区域矩形
    pub fn get_selection_rect(&self) -> Option<Rect> {
        if !self.committed && !self.dragging {
            return None;
        }

        let (x1, y1) = self.start;
        let (x2, y2) = self.end;

        if !x1.is_finite() || !y1.is_finite() || !x2.is_finite() || !y2.is_finite() {
            return None;
        }

        let left = x1.min(x2).floor();
        let top = y1.min(y2).floor();
        let right = x1.max(x2).ceil();
        let bottom = y1.max(y2).ceil();
        let x = left as i32;
        let y = top as i32;
        let w = (right - left) as i32;
        let h = (bottom - top) as i32;

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

// =================================================================================
// 截图处理函数（Jwm 方法扩展）
// =================================================================================

use crate::jwm::Jwm;

impl Jwm {
    /// 准备截图输出路径（交互式和全屏截图共用）
    fn prepare_screenshot_path() -> Option<String> {
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%6f");
        let pictures_dir = std::env::var_os("XDG_PICTURES_DIR")
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(dirs::picture_dir)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(std::path::PathBuf::from)
                    .map(|home| home.join("Pictures"))
            })
            .unwrap_or_else(std::env::temp_dir);
        let mut output_dir = pictures_dir;
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            warn!(
                "[take_screenshot] cannot create output dir '{}': {}, fallback to /tmp",
                output_dir.display(),
                e
            );
            output_dir = std::env::temp_dir();
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

        if !backend.has_compositor() {
            return Err("interactive screenshots require an active compositor".into());
        }

        let keyboard_grabbed = if let Some(root) = backend.root_window() {
            backend.key_ops().grab_keyboard(root)?;
            true
        } else {
            false
        };

        let crosshair_handle = backend
            .cursor_provider()
            .get(StdCursorKind::Crosshair)
            .ok()
            .map(|h| h.0);
        let pointer_mask = (EventMaskBits::BUTTON_PRESS
            | EventMaskBits::BUTTON_RELEASE
            | EventMaskBits::POINTER_MOTION)
            .bits();
        match backend
            .input_ops()
            .grab_pointer(pointer_mask, crosshair_handle)
        {
            Ok(true) => {}
            Ok(false) => {
                if keyboard_grabbed {
                    let _ = backend.key_ops().ungrab_keyboard();
                }
                return Err("could not grab pointer for screenshot selection".into());
            }
            Err(error) => {
                if keyboard_grabbed {
                    let _ = backend.key_ops().ungrab_keyboard();
                }
                return Err(error.into());
            }
        }

        self.features.screenshot.start();
        self.features.capture.screenshot = CaptureTarget::Region;
        self.features
            .screenshot
            .set_output_path(screenshot_path.clone());
        info!(
            "[take_screenshot] interactive capture → {} (G/W/M/D or Tab selects source)",
            screenshot_path
        );
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
        self.features.screenshot.cancel();
        backend.compositor_set_annotation_mode(false);
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
    /// 如果 `to_clipboard` 为 true，图片会复制到系统剪贴板而不是保存到文件。
    /// "做什么"由 `capture_plan` 中的纯策略决定，这里负责状态清理并把
    /// 计划执行到平台能力上。
    pub(crate) fn finish_screenshot_select(
        &mut self,
        backend: &mut dyn Backend,
        to_clipboard: bool,
    ) {
        use crate::jwm::features::capture_plan::{
            CaptureCompletion, CaptureExecution, clipboard_staging_path, execute_capture_plan,
            plan_capture_completion,
        };

        self.features.screenshot.commit_annotation();
        let annotations = self.features.screenshot.annotations.clone();
        let completion = plan_capture_completion(
            self.features.screenshot.output_path.take(),
            self.features.screenshot.get_selection_rect(),
            annotations.len(),
            to_clipboard,
            || {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros())
                    .unwrap_or_default();
                clipboard_staging_path(std::process::id(), stamp)
            },
        );

        let plan = match completion {
            CaptureCompletion::Cancel => {
                self.cancel_screenshot_select(backend);
                return;
            }
            other => {
                // Clear state before capturing
                self.features.screenshot.cancel();
                backend.compositor_set_annotation_mode(false);
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

                match other {
                    CaptureCompletion::TooSmall { width, height } => {
                        info!("[take_screenshot] selection too small ({width}x{height}), ignoring");
                        return;
                    }
                    CaptureCompletion::Capture(plan) => plan,
                    CaptureCompletion::Cancel => unreachable!("cancel is handled above"),
                }
            }
        };

        let (x, y, width, height) = plan.region;
        let captured = match execute_capture_plan(backend, &plan) {
            CaptureExecution::CapturedRegion => {
                info!(
                    "[take_screenshot] region screenshot → {} ({width}x{height} at {x},{y})",
                    plan.save_path
                );
                true
            }
            CaptureExecution::CapturedFullFallback => {
                info!(
                    "[take_screenshot] backend doesn't support region screenshots, falling back to full"
                );
                true
            }
            CaptureExecution::Unavailable => {
                info!(
                    "[take_screenshot] backend doesn't support region screenshots, falling back to full"
                );
                false
            }
            CaptureExecution::Failed(e) => {
                error!("[take_screenshot] region screenshot failed: {e}");
                false
            }
        };

        if plan.to_clipboard && captured {
            if plan.bake_annotations {
                Self::bake_annotations_then_maybe_copy(
                    backend,
                    plan.save_path,
                    (x, y),
                    annotations,
                    true,
                );
            } else {
                Self::copy_image_to_clipboard(backend, &plan.save_path);
            }
        } else if captured && plan.bake_annotations {
            Self::bake_annotations_then_maybe_copy(
                backend,
                plan.save_path,
                (x, y),
                annotations,
                false,
            );
        }
    }

    fn bake_annotations_then_maybe_copy(
        backend: &dyn Backend,
        png_path: String,
        region_origin: (i32, i32),
        annotations: Vec<ScreenshotAnnotation>,
        to_clipboard: bool,
    ) {
        let use_wl_copy = Self::is_udev_backend(backend);
        std::thread::spawn(move || {
            let mut ready = false;
            for _ in 0..60 {
                if std::fs::metadata(&png_path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
                {
                    ready = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if !ready {
                error!(
                    "[take_screenshot] screenshot file did not appear: {}",
                    png_path
                );
                return;
            }

            match Self::bake_annotations_into_png(&png_path, region_origin, &annotations) {
                Ok(()) => info!("[take_screenshot] annotations baked into {}", png_path),
                Err(e) => error!("[take_screenshot] failed to bake annotations: {e}"),
            }

            if to_clipboard {
                Self::copy_image_path_to_clipboard(&png_path, use_wl_copy);
            }
        });
    }

    fn bake_annotations_into_png(
        png_path: &str,
        region_origin: (i32, i32),
        annotations: &[ScreenshotAnnotation],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut image = image::open(png_path)?.to_rgba8();
        for annotation in annotations {
            Self::draw_annotation(&mut image, region_origin, annotation);
        }
        image.save(png_path)?;
        Ok(())
    }

    fn draw_annotation(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        annotation: &ScreenshotAnnotation,
    ) {
        match annotation {
            ScreenshotAnnotation::Freehand {
                points,
                color,
                width,
            } => {
                for pair in points.windows(2) {
                    Self::draw_line(image, region_origin, pair[0], pair[1], *color, *width);
                }
            }
            ScreenshotAnnotation::Line {
                from,
                to,
                color,
                width,
            } => Self::draw_line(image, region_origin, *from, *to, *color, *width),
            ScreenshotAnnotation::Arrow {
                from,
                to,
                color,
                width,
            } => Self::draw_arrow(image, region_origin, *from, *to, *color, *width),
            ScreenshotAnnotation::Rectangle {
                from,
                to,
                color,
                width,
            } => Self::draw_rect(image, region_origin, *from, *to, *color, *width),
            ScreenshotAnnotation::Ellipse {
                from,
                to,
                color,
                width,
            } => Self::draw_ellipse(image, region_origin, *from, *to, *color, *width),
        }
    }

    fn local_point(region_origin: (i32, i32), p: (f32, f32)) -> (i32, i32) {
        (
            (p.0.round() as i32) - region_origin.0,
            (p.1.round() as i32) - region_origin.1,
        )
    }

    fn put_brush(image: &mut RgbaImage, x: i32, y: i32, color: [u8; 4], width: u32) {
        let radius = (width as i32).max(1) / 2;
        let rgba = Rgba(color);
        for yy in y - radius..=y + radius {
            for xx in x - radius..=x + radius {
                let dx = xx - x;
                let dy = yy - y;
                if dx * dx + dy * dy > radius * radius + radius {
                    continue;
                }
                if xx >= 0 && yy >= 0 && xx < image.width() as i32 && yy < image.height() as i32 {
                    image.put_pixel(xx as u32, yy as u32, rgba);
                }
            }
        }
    }

    fn draw_line(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
        width: u32,
    ) {
        let (x0, y0) = Self::local_point(region_origin, from);
        let (x1, y1) = Self::local_point(region_origin, to);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let (mut x, mut y) = (x0, y0);
        loop {
            Self::put_brush(image, x, y, color, width);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    fn draw_arrow(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
        width: u32,
    ) {
        Self::draw_line(image, region_origin, from, to, color, width);
        let angle = (from.1 - to.1).atan2(from.0 - to.0);
        let head = (width as f32 * 4.0).max(14.0);
        for offset in [0.55_f32, -0.55_f32] {
            let p = (
                to.0 + (angle + offset).cos() * head,
                to.1 + (angle + offset).sin() * head,
            );
            Self::draw_line(image, region_origin, to, p, color, width);
        }
    }

    fn draw_rect(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
        width: u32,
    ) {
        let p1 = (from.0.min(to.0), from.1.min(to.1));
        let p2 = (from.0.max(to.0), from.1.max(to.1));
        Self::draw_line(image, region_origin, p1, (p2.0, p1.1), color, width);
        Self::draw_line(image, region_origin, (p2.0, p1.1), p2, color, width);
        Self::draw_line(image, region_origin, p2, (p1.0, p2.1), color, width);
        Self::draw_line(image, region_origin, (p1.0, p2.1), p1, color, width);
    }

    fn draw_ellipse(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
        width: u32,
    ) {
        let cx = (from.0 + to.0) * 0.5;
        let cy = (from.1 + to.1) * 0.5;
        let rx = ((from.0 - to.0).abs() * 0.5).max(1.0);
        let ry = ((from.1 - to.1).abs() * 0.5).max(1.0);
        let steps = ((rx.max(ry) * 6.0) as usize).clamp(32, 720);
        let mut prev = (cx + rx, cy);
        for i in 1..=steps {
            let t = i as f32 / steps as f32 * std::f32::consts::TAU;
            let next = (cx + rx * t.cos(), cy + ry * t.sin());
            Self::draw_line(image, region_origin, prev, next, color, width);
            prev = next;
        }
    }

    /// 使用 xclip 或 wl-copy 将 PNG 图片复制到系统剪贴板
    ///
    /// 截图由合成器在下一帧异步捕获，所以 PNG 文件在调用时还不存在。
    /// 我们启动一个 shell 脚本轮询等待文件出现后再运行剪贴板工具。
    fn copy_image_to_clipboard(backend: &dyn Backend, png_path: &str) {
        Self::copy_image_path_to_clipboard(png_path, Self::is_udev_backend(backend));
    }

    fn copy_image_path_to_clipboard(png_path: &str, use_wl_copy: bool) {
        let png_path = png_path.to_string();
        info!("[take_screenshot] clipboard copy scheduled: {}", png_path);

        std::thread::spawn(move || {
            let mut ready = false;
            for _ in 0..60 {
                if std::fs::metadata(&png_path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
                {
                    ready = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if !ready {
                error!(
                    "[take_screenshot] clipboard source file did not appear: {}",
                    png_path
                );
                return;
            }

            let wl_copy = use_wl_copy && Self::path_has_executable("wl-copy");
            let xclip = Self::path_has_executable("xclip");
            let (program, args): (&str, &[&str]) = if wl_copy {
                ("wl-copy", &["-t", "image/png"])
            } else if xclip {
                (
                    "xclip",
                    &["-selection", "clipboard", "-t", "image/png", "-i"],
                )
            } else {
                error!(
                    "[take_screenshot] clipboard copy failed: neither wl-copy nor xclip is available"
                );
                return;
            };

            if use_wl_copy && !wl_copy && xclip {
                warn!("[take_screenshot] wl-copy not found, falling back to xclip");
            }

            let file = match std::fs::File::open(&png_path) {
                Ok(file) => file,
                Err(e) => {
                    error!("[take_screenshot] clipboard source open failed: {e}");
                    return;
                }
            };

            let output = Command::new(program)
                .args(args)
                .stdin(Stdio::from(file))
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output();

            match output {
                Ok(output) if output.status.success() => {
                    info!("[take_screenshot] copied image to clipboard via {program}");
                    let _ = std::fs::remove_file(&png_path);
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    error!(
                        "[take_screenshot] clipboard copy via {program} failed: status={} stderr={}",
                        output.status,
                        stderr.trim()
                    );
                }
                Err(e) => {
                    error!("[take_screenshot] failed to run clipboard helper {program}: {e}");
                }
            }
        });
    }

    fn path_has_executable(bin: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(bin);
            candidate.is_file()
                && std::fs::metadata(&candidate)
                    .map(|m| {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            m.permissions().mode() & 0o111 != 0
                        }
                        #[cfg(not(unix))]
                        {
                            true
                        }
                    })
                    .unwrap_or(false)
        })
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

    #[test]
    fn test_annotation_workflow() {
        let mut state = ScreenshotState::new();
        state.start();
        state.set_tool(ScreenshotTool::Arrow);
        state.set_palette_color(3);
        state.increase_line_width();

        state.begin_annotation(10.0, 20.0);
        state.update_annotation(110.0, 80.0);
        state.commit_annotation();

        assert_eq!(state.annotations.len(), 1);
        match &state.annotations[0] {
            ScreenshotAnnotation::Arrow {
                from,
                to,
                color,
                width,
            } => {
                assert_eq!(*from, (10.0, 20.0));
                assert_eq!(*to, (110.0, 80.0));
                assert_eq!(*color, [80, 170, 255, 255]);
                assert_eq!(*width, 5);
            }
            other => panic!("expected arrow annotation, got {other:?}"),
        }

        state.undo_annotation();
        assert!(state.annotations.is_empty());
    }

    #[test]
    fn test_freehand_requires_multiple_points() {
        let mut state = ScreenshotState::new();
        state.start();
        state.set_tool(ScreenshotTool::Pencil);
        state.begin_annotation(10.0, 20.0);
        state.commit_annotation();
        assert!(state.annotations.is_empty());

        state.begin_annotation(10.0, 20.0);
        state.update_annotation(12.0, 24.0);
        state.commit_annotation();
        assert_eq!(state.annotations.len(), 1);
    }

    #[test]
    fn test_move_committed_selection() {
        let mut state = ScreenshotState::new();
        state.start();
        state.begin_drag(100.0, 120.0);
        state.update_drag(240.0, 260.0);
        state.commit();

        state.move_selection(5.0, -10.0);

        let rect = state.get_selection_rect().unwrap();
        assert_eq!(rect.x, 105);
        assert_eq!(rect.y, 110);
        assert_eq!(rect.w, 140);
        assert_eq!(rect.h, 140);
    }

    #[test]
    fn moving_selection_keeps_annotations_attached() {
        let mut state = ScreenshotState::new();
        state.start();
        state.select_rect(Rect::new(70, 20, 20, 30));
        state.set_tool(ScreenshotTool::Arrow);
        state.begin_annotation(72.0, 24.0);
        state.update_annotation(86.0, 42.0);
        state.commit_annotation();

        state.move_selection_within(50.0, 0.0, Rect::new(0, 0, 100, 100));

        assert_eq!(state.get_selection_rect(), Some(Rect::new(80, 20, 20, 30)));
        match &state.annotations[0] {
            ScreenshotAnnotation::Arrow { from, to, .. } => {
                assert_eq!(*from, (82.0, 24.0));
                assert_eq!(*to, (96.0, 42.0));
            }
            other => panic!("expected arrow annotation, got {other:?}"),
        }
    }

    #[test]
    fn fractional_selection_rounds_outward() {
        let mut state = ScreenshotState::new();
        state.start();
        state.begin_drag(10.8, 12.2);
        state.update_drag(20.2, 30.7);
        state.commit();

        assert_eq!(state.get_selection_rect(), Some(Rect::new(10, 12, 11, 19)));
    }
}
