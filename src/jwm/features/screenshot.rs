//! 交互式截图功能 - 区域选择和保存

use crate::backend::api::Backend;
use crate::backend::common_define::{EventMaskBits, StdCursorKind};
use crate::backend::compositor_common::screenshot_toolbar::{
    ScreenshotToolbar, ToolbarButton, ToolbarIcon,
};
use crate::core::types::Rect;
use crate::jwm::features::capture::CaptureTarget;
use crate::jwm::types::WMArgEnum;
use image::{Rgba, RgbaImage};
use log::{error, info, warn};
use std::process::{Command, Stdio};

/// Stroke width bounds. The floor keeps a stroke visible; the ceiling keeps a
/// held-down key from turning the whole selection into one blob.
pub const MIN_LINE_WIDTH: u32 = 1;
pub const MAX_LINE_WIDTH: u32 = 24;

/// The same ink at highlighter transparency.
#[must_use]
pub fn marker_ink(color: [u8; 4]) -> [u8; 4] {
    [color[0], color[1], color[2], MARKER_ALPHA]
}

/// What a drag (or a click) inside a committed selection draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotTool {
    /// Not drawing — the state the selection drag itself runs in.
    Select,
    Pencil,
    Line,
    Arrow,
    Rectangle,
    FilledRectangle,
    Ellipse,
    /// Translucent highlighter: a freehand stroke that lets the pixels under
    /// it show through, which is what makes it read as marker rather than
    /// paint.
    Marker,
    /// Click to place a typed label.
    Text,
    /// Click to place the next number in a sequence.
    Counter,
    /// Drag a region down to blocks — the tool for redacting a password field
    /// without leaving a black hole that says "something was here".
    Pixelate,
    /// Drag a region to invert its colors.
    Invert,
}

impl Default for ScreenshotTool {
    fn default() -> Self {
        Self::Select
    }
}

impl ScreenshotTool {
    /// Whether this tool is placed with a single click rather than a drag.
    /// Click-placed tools commit on press, so they never wait for a motion
    /// that a careful click will not produce.
    #[must_use]
    pub const fn is_click_placed(self) -> bool {
        matches!(self, Self::Text | Self::Counter)
    }

    /// The toolbar icon that stands for this tool.
    #[must_use]
    pub const fn icon(self) -> ToolbarIcon {
        match self {
            // `Select` never appears in the toolbar; the pencil is the tool the
            // editor opens in, so it is the honest stand-in.
            Self::Select | Self::Pencil => ToolbarIcon::Pencil,
            Self::Line => ToolbarIcon::Line,
            Self::Arrow => ToolbarIcon::Arrow,
            Self::Rectangle => ToolbarIcon::RectOutline,
            Self::FilledRectangle => ToolbarIcon::RectFilled,
            Self::Ellipse => ToolbarIcon::Ellipse,
            Self::Marker => ToolbarIcon::Marker,
            Self::Text => ToolbarIcon::Text,
            Self::Counter => ToolbarIcon::Counter,
            Self::Pixelate => ToolbarIcon::Pixelate,
            Self::Invert => ToolbarIcon::Invert,
        }
    }

    /// The tools the toolbar offers, left to right. `Select` is absent by
    /// design — it is a mode, not something to draw with.
    pub const PALETTE: [Self; 11] = [
        Self::Pencil,
        Self::Line,
        Self::Arrow,
        Self::Rectangle,
        Self::FilledRectangle,
        Self::Ellipse,
        Self::Marker,
        Self::Text,
        Self::Counter,
        Self::Pixelate,
        Self::Invert,
    ];
}

/// What clicking a toolbar button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarCommand {
    SelectTool(ScreenshotTool),
    Thinner,
    Thicker,
    NextColor,
    Undo,
    Redo,
    Copy,
    Save,
    Cancel,
}

/// One toolbar cell: what it looks like, and what it does. Built together so
/// the index a click resolves to cannot drift from the index that was painted.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolbarEntry {
    pub button: ToolbarButton,
    /// `None` for the read-only size readout.
    pub command: Option<ToolbarCommand>,
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
    /// A solid rectangle — the redaction bar.
    FilledRectangle {
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
    },
    /// A freehand stroke laid down translucently, so what is under it stays
    /// readable.
    Marker {
        points: Vec<(f32, f32)>,
        color: [u8; 4],
        width: u32,
    },
    /// A region reduced to blocks of `block` pixels a side.
    Pixelate {
        from: (f32, f32),
        to: (f32, f32),
        block: u32,
    },
    /// A region with its colors inverted.
    Invert { from: (f32, f32), to: (f32, f32) },
    /// A filled disc carrying the next number in the sequence.
    Counter {
        at: (f32, f32),
        number: u32,
        color: [u8; 4],
        radius: f32,
    },
    /// A typed label with its baseline-left corner at `at`.
    Text {
        at: (f32, f32),
        text: String,
        color: [u8; 4],
        size: f32,
    },
}

/// Marker ink is laid down at this alpha regardless of the palette, which is
/// what separates "highlight" from "paint over".
pub const MARKER_ALPHA: u8 = 96;
/// Marker strokes are this many times the nominal line width — a highlighter
/// that was as thin as a pen would just look like a faded pen.
pub const MARKER_WIDTH_FACTOR: u32 = 4;
/// Counter bubbles scale with the line width so they stay proportional to
/// whatever else is being drawn.
pub const COUNTER_RADIUS_FACTOR: f32 = 2.6;
pub const COUNTER_MIN_RADIUS: f32 = 9.0;
/// Text is sized off the line width for the same reason.
pub const TEXT_SIZE_FACTOR: f32 = 4.5;
pub const TEXT_MIN_SIZE: f32 = 11.0;
/// Mosaic block size, likewise derived from the line width so the thickness
/// control means "coarser" for the pixelate tool too.
pub const PIXELATE_BLOCK_FACTOR: u32 = 3;
pub const PIXELATE_MIN_BLOCK: u32 = 4;

impl ScreenshotAnnotation {
    fn translate(&mut self, dx: f32, dy: f32) {
        let translate_point = |point: &mut (f32, f32)| {
            point.0 += dx;
            point.1 += dy;
        };
        match self {
            Self::Freehand { points, .. } | Self::Marker { points, .. } => {
                for point in points {
                    translate_point(point);
                }
            }
            Self::Line { from, to, .. }
            | Self::Arrow { from, to, .. }
            | Self::Rectangle { from, to, .. }
            | Self::Ellipse { from, to, .. }
            | Self::FilledRectangle { from, to, .. }
            | Self::Pixelate { from, to, .. }
            | Self::Invert { from, to } => {
                translate_point(from);
                translate_point(to);
            }
            Self::Counter { at, .. } | Self::Text { at, .. } => translate_point(at),
        }
    }
}

/// The ink ring, reachable with `1`..`8` and by stepping with the toolbar's
/// swatch. Ordered warm-to-cool then neutral, so the two most-reached-for
/// colors (red, then yellow) are the first two keys.
pub const PALETTE: [[u8; 4]; 8] = [
    [255, 70, 70, 255],
    [255, 190, 60, 255],
    [85, 215, 110, 255],
    [80, 170, 255, 255],
    [180, 110, 255, 255],
    [255, 255, 255, 255],
    [30, 30, 30, 255],
    [255, 90, 180, 255],
];

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
    /// Annotations taken back by undo, newest last, waiting for a redo. Any
    /// fresh annotation clears them — the usual editor contract.
    pub undone: Vec<ScreenshotAnnotation>,
    /// 正在绘制标注
    pub drawing_annotation: bool,
    /// 当前标注起点
    pub annotation_start: (f32, f32),
    /// 当前标注终点
    pub annotation_end: (f32, f32),
    /// 当前自由绘制点集
    pub current_points: Vec<(f32, f32)>,
    /// Which palette entry `color` came from, so the toolbar's swatch can walk
    /// the ring rather than guessing from the RGBA.
    pub palette_index: usize,
    /// The number the next counter bubble will carry.
    pub counter_next: u32,
    /// The label being typed, if the text tool has an open draft.
    pub text_draft: Option<TextDraft>,
    /// The toolbar as last published to the compositor. Kept here because the
    /// hit test must run against exactly the rectangles that were painted.
    pub toolbar: Option<ScreenshotToolbar>,
    /// Which toolbar button the pointer is over, if any.
    pub hovered_button: Option<usize>,
}

/// A label under construction: where it goes and what has been typed so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextDraft {
    pub at: (f32, f32),
    pub buffer: String,
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
        self.palette_index = 0;
        self.color = PALETTE[0];
        self.line_width = 4;
        self.clear_annotations();
    }

    pub fn reset_selection(&mut self) {
        self.dragging = false;
        self.committed = false;
        self.start = (0.0, 0.0);
        self.end = (0.0, 0.0);
        self.tool = ScreenshotTool::Select;
        self.clear_annotations();
    }

    /// Drop every mark and everything that was mid-flight, including the
    /// toolbar: a selection that is being redrawn has nothing to annotate yet.
    fn clear_annotations(&mut self) {
        self.annotations.clear();
        self.undone.clear();
        self.drawing_annotation = false;
        self.current_points.clear();
        self.counter_next = 1;
        self.text_draft = None;
        self.toolbar = None;
        self.hovered_button = None;
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
        if let Some(color) = PALETTE.get(idx) {
            self.color = *color;
            self.palette_index = idx;
        }
    }

    /// Step to the next ink. This is what the toolbar's swatch does, since a
    /// full color picker would be a second popup to dismiss.
    pub fn next_palette_color(&mut self) {
        self.set_palette_color((self.palette_index + 1) % PALETTE.len());
    }

    pub fn increase_line_width(&mut self) {
        self.line_width = (self.line_width + 1).min(MAX_LINE_WIDTH);
    }

    pub fn decrease_line_width(&mut self) {
        self.line_width = self.line_width.saturating_sub(1).max(MIN_LINE_WIDTH);
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

    /// Freehand tools accumulate points; everything else works from the two
    /// endpoints, so only the freehand pair seeds the point list.
    fn is_freehand(tool: ScreenshotTool) -> bool {
        matches!(tool, ScreenshotTool::Pencil | ScreenshotTool::Marker)
    }

    pub fn begin_annotation(&mut self, x: f32, y: f32) {
        // A click-placed tool has nothing to drag: it lands the mark now, so a
        // click that never moves still produces one.
        if self.tool.is_click_placed() {
            self.place_at(x, y);
            return;
        }
        self.drawing_annotation = true;
        self.annotation_start = (x, y);
        self.annotation_end = (x, y);
        self.current_points.clear();
        if Self::is_freehand(self.tool) {
            self.current_points.push((x, y));
        }
    }

    /// Land a click-placed mark: the next counter bubble, or an open text
    /// draft. An existing draft is committed first, so clicking elsewhere
    /// finishes the label you were typing instead of losing it.
    fn place_at(&mut self, x: f32, y: f32) {
        match self.tool {
            ScreenshotTool::Counter => {
                let annotation = ScreenshotAnnotation::Counter {
                    at: (x, y),
                    number: self.counter_next,
                    color: self.color,
                    radius: self.counter_radius(),
                };
                self.counter_next = self.counter_next.saturating_add(1);
                self.push_annotation(annotation);
            }
            ScreenshotTool::Text => {
                self.commit_text_draft();
                self.text_draft = Some(TextDraft {
                    at: (x, y),
                    buffer: String::new(),
                });
            }
            _ => {}
        }
    }

    /// Radius of the next counter bubble, and the point size of the next
    /// label: both track the line width so one control scales every tool.
    #[must_use]
    pub fn counter_radius(&self) -> f32 {
        (self.line_width as f32 * COUNTER_RADIUS_FACTOR).max(COUNTER_MIN_RADIUS)
    }

    #[must_use]
    pub fn text_size(&self) -> f32 {
        (self.line_width as f32 * TEXT_SIZE_FACTOR).max(TEXT_MIN_SIZE)
    }

    /// The width a stroke of the *current* tool is drawn at. Only the
    /// highlighter differs, and it differs a lot — a marker as thin as the pen
    /// would just look like faded ink.
    #[must_use]
    pub fn stroke_width(&self) -> u32 {
        if self.tool == ScreenshotTool::Marker {
            self.line_width * MARKER_WIDTH_FACTOR
        } else {
            self.line_width
        }
    }

    #[must_use]
    pub fn pixelate_block(&self) -> u32 {
        (self.line_width * PIXELATE_BLOCK_FACTOR).max(PIXELATE_MIN_BLOCK)
    }

    /// Add a finished mark. Doing this in one place is what guarantees a new
    /// mark always invalidates the redo stack.
    fn push_annotation(&mut self, annotation: ScreenshotAnnotation) {
        self.annotations.push(annotation);
        self.undone.clear();
    }

    /// Append one character to the open label.
    pub fn text_input(&mut self, ch: char) {
        if let Some(draft) = self.text_draft.as_mut() {
            draft.buffer.push(ch);
        }
    }

    /// Remove the last character of the open label. Pops a whole `char`, not a
    /// byte, so a CJK label does not end up half-deleted and invalid.
    pub fn text_backspace(&mut self) {
        if let Some(draft) = self.text_draft.as_mut() {
            draft.buffer.pop();
        }
    }

    /// Whether keystrokes are currently going into a label rather than into
    /// the tool shortcuts.
    #[must_use]
    pub fn is_typing(&self) -> bool {
        self.text_draft.is_some()
    }

    /// Finish the open label. An empty one is discarded rather than committed,
    /// so a stray click with the text tool leaves nothing behind.
    pub fn commit_text_draft(&mut self) {
        let Some(draft) = self.text_draft.take() else {
            return;
        };
        if draft.buffer.trim().is_empty() {
            return;
        }
        let annotation = ScreenshotAnnotation::Text {
            at: draft.at,
            text: draft.buffer,
            color: self.color,
            size: self.text_size(),
        };
        self.push_annotation(annotation);
    }

    /// Abandon the open label without committing it.
    pub fn cancel_text_draft(&mut self) -> bool {
        self.text_draft.take().is_some()
    }

    pub fn update_annotation(&mut self, x: f32, y: f32) {
        if !self.drawing_annotation {
            return;
        }
        self.annotation_end = (x, y);
        if Self::is_freehand(self.tool) {
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
            self.push_annotation(annotation);
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
            ScreenshotTool::Marker if self.current_points.len() > 1 => {
                Some(ScreenshotAnnotation::Marker {
                    points: self.current_points.clone(),
                    color: marker_ink(color),
                    width: width * MARKER_WIDTH_FACTOR,
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
            ScreenshotTool::FilledRectangle => {
                Some(ScreenshotAnnotation::FilledRectangle { from, to, color })
            }
            ScreenshotTool::Ellipse => Some(ScreenshotAnnotation::Ellipse {
                from,
                to,
                color,
                width,
            }),
            ScreenshotTool::Pixelate => Some(ScreenshotAnnotation::Pixelate {
                from,
                to,
                block: self.pixelate_block(),
            }),
            ScreenshotTool::Invert => Some(ScreenshotAnnotation::Invert { from, to }),
            _ => None,
        }
    }

    /// The label being typed, shown live so you can see what you are writing.
    #[must_use]
    pub fn text_draft_preview(&self) -> Option<ScreenshotAnnotation> {
        let draft = self.text_draft.as_ref()?;
        if draft.buffer.is_empty() {
            return None;
        }
        Some(ScreenshotAnnotation::Text {
            at: draft.at,
            text: draft.buffer.clone(),
            color: self.color,
            size: self.text_size(),
        })
    }

    /// Take back the last mark. An open label counts as the newest thing on
    /// the canvas, so undo cancels it first rather than reaching past it.
    pub fn undo_annotation(&mut self) {
        if self.cancel_text_draft() {
            return;
        }
        if let Some(annotation) = self.annotations.pop() {
            // A counter that was undone should hand its number back, or the
            // sequence gains a gap the user never sees a reason for.
            if let ScreenshotAnnotation::Counter { number, .. } = &annotation {
                self.counter_next = (*number).max(1);
            }
            self.undone.push(annotation);
        }
    }

    /// Put back the last undone mark.
    pub fn redo_annotation(&mut self) {
        if let Some(annotation) = self.undone.pop() {
            if let ScreenshotAnnotation::Counter { number, .. } = &annotation {
                self.counter_next = number.saturating_add(1);
            }
            self.annotations.push(annotation);
        }
    }

    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.annotations.is_empty() || self.is_typing()
    }

    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// The toolbar's cells, left to right: the tool ring, the selection's
    /// size, the stroke and ink controls, then the four things that end the
    /// capture. Built in one pass so a cell's look and its action cannot drift
    /// apart.
    #[must_use]
    pub fn toolbar_entries(&self) -> Vec<ToolbarEntry> {
        let mut entries = Vec::with_capacity(ScreenshotTool::PALETTE.len() + 9);
        for tool in ScreenshotTool::PALETTE {
            entries.push(ToolbarEntry {
                button: ToolbarButton::icon(tool.icon()).selected(self.tool == tool),
                command: Some(ToolbarCommand::SelectTool(tool)),
            });
        }

        let size = self
            .get_selection_rect()
            .map_or_else(|| "—".to_owned(), |rect| format!("{}×{}", rect.w, rect.h));
        entries.push(ToolbarEntry {
            button: ToolbarButton::label(size),
            command: None,
        });

        // Stroke and ink together, then the four ways out. A control that
        // cannot do anything right now is present but disabled, so the row
        // never reflows under the pointer mid-edit.
        for (icon, command, enabled) in [
            (
                ToolbarIcon::Thinner,
                ToolbarCommand::Thinner,
                self.line_width > MIN_LINE_WIDTH,
            ),
            (
                ToolbarIcon::Thicker,
                ToolbarCommand::Thicker,
                self.line_width < MAX_LINE_WIDTH,
            ),
            (ToolbarIcon::Color, ToolbarCommand::NextColor, true),
            (ToolbarIcon::Undo, ToolbarCommand::Undo, self.can_undo()),
            (ToolbarIcon::Redo, ToolbarCommand::Redo, self.can_redo()),
            (ToolbarIcon::Copy, ToolbarCommand::Copy, true),
            (ToolbarIcon::Save, ToolbarCommand::Save, true),
            (ToolbarIcon::Close, ToolbarCommand::Cancel, true),
        ] {
            let mut button = ToolbarButton::icon(icon).available(enabled);
            if icon == ToolbarIcon::Color {
                button = button.tinted(self.color);
            }
            entries.push(ToolbarEntry {
                button,
                command: Some(command),
            });
        }
        entries
    }

    /// What the button at `index` of the last published toolbar does.
    #[must_use]
    pub fn toolbar_command(&self, index: usize) -> Option<ToolbarCommand> {
        self.toolbar_entries().get(index)?.command
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
        backend.compositor_set_screenshot_toolbar(None);
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

        // A label still being typed counts as part of the capture — finishing
        // with text half-entered should keep the text, not discard it.
        self.features.screenshot.commit_annotation();
        self.features.screenshot.commit_text_draft();
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
                // Clear state before capturing. The toolbar has to go with
                // it: the compositor captures the very next frame, and a strip
                // still on screen would be baked into the PNG.
                self.features.screenshot.cancel();
                backend.compositor_set_annotation_mode(false);
                backend.compositor_set_screenshot_toolbar(None);
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
            ScreenshotAnnotation::FilledRectangle { from, to, color } => {
                Self::fill_region(image, region_origin, *from, *to, |pixel| {
                    *pixel = Rgba(*color);
                });
            }
            ScreenshotAnnotation::Marker {
                points,
                color,
                width,
            } => {
                // A highlighter that overlapped itself would darken at every
                // crossing, so the stroke is masked first and composited once.
                Self::draw_translucent_polyline(image, region_origin, points, *color, *width);
            }
            ScreenshotAnnotation::Pixelate { from, to, block } => {
                Self::pixelate_region(image, region_origin, *from, *to, *block);
            }
            ScreenshotAnnotation::Invert { from, to } => {
                Self::fill_region(image, region_origin, *from, *to, |pixel| {
                    pixel[0] = 255 - pixel[0];
                    pixel[1] = 255 - pixel[1];
                    pixel[2] = 255 - pixel[2];
                });
            }
            ScreenshotAnnotation::Counter {
                at,
                number,
                color,
                radius,
            } => Self::draw_counter(image, region_origin, *at, *number, *color, *radius),
            ScreenshotAnnotation::Text {
                at,
                text,
                color,
                size,
            } => Self::draw_text(image, region_origin, *at, text, *color, *size),
        }
    }

    /// Clip a global-coordinate rectangle to the captured image and apply
    /// `f` to every pixel inside it. The clip is what keeps a drag that ran
    /// off the selection from panicking on an out-of-bounds put.
    fn fill_region(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        from: (f32, f32),
        to: (f32, f32),
        mut f: impl FnMut(&mut Rgba<u8>),
    ) {
        let Some((x0, y0, x1, y1)) = Self::clipped_region(image, region_origin, from, to) else {
            return;
        };
        for y in y0..y1 {
            for x in x0..x1 {
                f(image.get_pixel_mut(x, y));
            }
        }
    }

    /// The half-open pixel range a global rectangle covers in the captured
    /// image, or `None` when it misses the image entirely.
    fn clipped_region(
        image: &RgbaImage,
        region_origin: (i32, i32),
        from: (f32, f32),
        to: (f32, f32),
    ) -> Option<(u32, u32, u32, u32)> {
        let (ax, ay) = Self::local_point(region_origin, from);
        let (bx, by) = Self::local_point(region_origin, to);
        let x0 = ax.min(bx).max(0);
        let y0 = ay.min(by).max(0);
        let x1 = ax.max(bx).min(image.width() as i32);
        let y1 = ay.max(by).min(image.height() as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some((x0 as u32, y0 as u32, x1 as u32, y1 as u32))
    }

    /// Average each `block`×`block` tile and paint the tile that color.
    fn pixelate_region(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        from: (f32, f32),
        to: (f32, f32),
        block: u32,
    ) {
        let Some((x0, y0, x1, y1)) = Self::clipped_region(image, region_origin, from, to) else {
            return;
        };
        let block = block.max(1);
        let mut ty = y0;
        while ty < y1 {
            let mut tx = x0;
            let bottom = (ty + block).min(y1);
            while tx < x1 {
                let right = (tx + block).min(x1);
                let mut sum = [0u64; 3];
                let mut count = 0u64;
                for y in ty..bottom {
                    for x in tx..right {
                        let pixel = image.get_pixel(x, y);
                        sum[0] += u64::from(pixel[0]);
                        sum[1] += u64::from(pixel[1]);
                        sum[2] += u64::from(pixel[2]);
                        count += 1;
                    }
                }
                if count > 0 {
                    let average = [
                        (sum[0] / count) as u8,
                        (sum[1] / count) as u8,
                        (sum[2] / count) as u8,
                    ];
                    for y in ty..bottom {
                        for x in tx..right {
                            let pixel = image.get_pixel_mut(x, y);
                            pixel[0] = average[0];
                            pixel[1] = average[1];
                            pixel[2] = average[2];
                        }
                    }
                }
                tx = right;
            }
            ty = bottom;
        }
    }

    /// Lay a translucent stroke down exactly once per pixel.
    ///
    /// Compositing segment by segment would darken every place the stroke
    /// crosses itself — and a freehand stroke crosses itself constantly — so
    /// coverage is collected into a mask first and blended in one pass.
    fn draw_translucent_polyline(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        points: &[(f32, f32)],
        color: [u8; 4],
        width: u32,
    ) {
        if points.len() < 2 {
            return;
        }
        let (w, h) = (image.width(), image.height());
        let mut mask = vec![false; (w as usize) * (h as usize)];
        for pair in points.windows(2) {
            Self::trace_line(region_origin, pair[0], pair[1], width, |x, y| {
                if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                    mask[y as usize * w as usize + x as usize] = true;
                }
            });
        }
        let alpha = f32::from(color[3]) / 255.0;
        for y in 0..h {
            for x in 0..w {
                if !mask[y as usize * w as usize + x as usize] {
                    continue;
                }
                let pixel = image.get_pixel_mut(x, y);
                for c in 0..3 {
                    pixel[c] = (f32::from(pixel[c]) * (1.0 - alpha) + f32::from(color[c]) * alpha)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                }
            }
        }
    }

    /// A numbered bubble: a filled disc with the number centred in it.
    fn draw_counter(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        at: (f32, f32),
        number: u32,
        color: [u8; 4],
        radius: f32,
    ) {
        let (cx, cy) = Self::local_point(region_origin, at);
        let radius = radius.max(1.0);
        let r = radius.ceil() as i32;
        let rgba = Rgba(color);
        for y in cy - r..=cy + r {
            for x in cx - r..=cx + r {
                let dx = (x - cx) as f32;
                let dy = (y - cy) as f32;
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
                    image.put_pixel(x as u32, y as u32, rgba);
                }
            }
        }
        // The numeral goes on in the ink that reads against the bubble.
        let ink = if Self::is_light(color) {
            [20, 20, 20, 255]
        } else {
            [255, 255, 255, 255]
        };
        let label = number.to_string();
        let scale = ((radius * 1.1) as u32 / crate::backend::compositor_font::GLYPH_H).max(1);
        let (pixels, tw, th) =
            crate::backend::compositor_font::render_text_to_rgba(&label, scale, ink);
        Self::blend_rgba(
            image,
            pixels.as_slice(),
            tw,
            th,
            cx - tw as i32 / 2,
            cy - th as i32 / 2,
        );
    }

    /// A typed label, rasterised with the same UI font the compositor draws it
    /// in so the baked PNG matches the live preview.
    fn draw_text(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        at: (f32, f32),
        text: &str,
        color: [u8; 4],
        size: f32,
    ) {
        if text.is_empty() {
            return;
        }
        let (x, y) = Self::local_point(region_origin, at);
        let config = crate::config::CONFIG.load();
        let (pixels, w, h) = crate::backend::compositor_font::render_ui_text_to_rgba(
            text,
            config.system_ui_font(),
            size,
            color,
        );
        Self::blend_rgba(image, pixels.as_slice(), w, h, x, y);
    }

    /// Source-over composite of a straight-alpha RGBA buffer at `(ox, oy)`,
    /// clipped to the image.
    fn blend_rgba(image: &mut RgbaImage, pixels: &[u8], w: u32, h: u32, ox: i32, oy: i32) {
        if w == 0 || h == 0 || pixels.len() < (w * h * 4) as usize {
            return;
        }
        for y in 0..h {
            let ty = oy + y as i32;
            if ty < 0 || ty as u32 >= image.height() {
                continue;
            }
            for x in 0..w {
                let tx = ox + x as i32;
                if tx < 0 || tx as u32 >= image.width() {
                    continue;
                }
                let offset = ((y * w + x) * 4) as usize;
                let alpha = f32::from(pixels[offset + 3]) / 255.0;
                if alpha <= 0.0 {
                    continue;
                }
                let pixel = image.get_pixel_mut(tx as u32, ty as u32);
                for c in 0..3 {
                    pixel[c] = (f32::from(pixel[c]) * (1.0 - alpha)
                        + f32::from(pixels[offset + c]) * alpha)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                }
            }
        }
    }

    /// Rec. 601 luma, used only to pick black or white ink for a bubble.
    fn is_light(color: [u8; 4]) -> bool {
        let luma =
            0.299 * f32::from(color[0]) + 0.587 * f32::from(color[1]) + 0.114 * f32::from(color[2]);
        luma > 140.0
    }

    fn local_point(region_origin: (i32, i32), p: (f32, f32)) -> (i32, i32) {
        (
            (p.0.round() as i32) - region_origin.0,
            (p.1.round() as i32) - region_origin.1,
        )
    }

    /// Visit every pixel a round brush of `width` covers, centred on `(x, y)`.
    fn visit_brush(x: i32, y: i32, width: u32, visit: &mut impl FnMut(i32, i32)) {
        let radius = (width as i32).max(1) / 2;
        for yy in y - radius..=y + radius {
            for xx in x - radius..=x + radius {
                let dx = xx - x;
                let dy = yy - y;
                if dx * dx + dy * dy > radius * radius + radius {
                    continue;
                }
                visit(xx, yy);
            }
        }
    }

    /// Walk the brushed pixels of a line without deciding what to do with
    /// them. Opaque strokes paint as they go; the highlighter instead collects
    /// coverage into a mask so overlaps do not stack.
    fn trace_line(
        region_origin: (i32, i32),
        from: (f32, f32),
        to: (f32, f32),
        width: u32,
        mut visit: impl FnMut(i32, i32),
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
            Self::visit_brush(x, y, width, &mut visit);
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

    fn draw_line(
        image: &mut RgbaImage,
        region_origin: (i32, i32),
        from: (f32, f32),
        to: (f32, f32),
        color: [u8; 4],
        width: u32,
    ) {
        let (w, h) = (image.width() as i32, image.height() as i32);
        let rgba = Rgba(color);
        let mut writes = Vec::new();
        Self::trace_line(region_origin, from, to, width, |x, y| {
            if x >= 0 && y >= 0 && x < w && y < h {
                writes.push((x as u32, y as u32));
            }
        });
        for (x, y) in writes {
            image.put_pixel(x, y, rgba);
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
    use crate::backend::compositor_common::screenshot_toolbar::ButtonFace;

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

    /// Set up an editor with a committed selection — the state every toolbar
    /// test starts from, since the strip only exists once there is something
    /// to edit.
    fn editing(w: i32, h: i32) -> ScreenshotState {
        let mut state = ScreenshotState::new();
        state.start();
        state.select_rect(Rect::new(100, 100, w, h));
        state
    }

    #[test]
    fn undo_and_redo_walk_the_same_marks_in_both_directions() {
        let mut state = editing(300, 200);
        for tool in [ScreenshotTool::Line, ScreenshotTool::Arrow] {
            state.set_tool(tool);
            state.begin_annotation(110.0, 110.0);
            state.update_annotation(200.0, 180.0);
            state.commit_annotation();
        }
        assert_eq!(state.annotations.len(), 2);
        assert!(state.can_undo() && !state.can_redo());

        state.undo_annotation();
        state.undo_annotation();
        assert!(state.annotations.is_empty());
        assert!(!state.can_undo() && state.can_redo());

        state.redo_annotation();
        state.redo_annotation();
        assert_eq!(state.annotations.len(), 2);
        assert!(!state.can_redo());
    }

    /// The usual editor contract: branching off an undone history throws the
    /// forward history away rather than leaving a redo that would reappear
    /// out of order.
    #[test]
    fn drawing_after_an_undo_drops_the_redo_history() {
        let mut state = editing(300, 200);
        state.set_tool(ScreenshotTool::Line);
        state.begin_annotation(110.0, 110.0);
        state.update_annotation(200.0, 180.0);
        state.commit_annotation();
        state.undo_annotation();
        assert!(state.can_redo());

        state.begin_annotation(120.0, 120.0);
        state.update_annotation(210.0, 190.0);
        state.commit_annotation();
        assert!(!state.can_redo());
        assert_eq!(state.annotations.len(), 1);
    }

    #[test]
    fn counters_number_themselves_and_give_the_number_back_on_undo() {
        let mut state = editing(300, 200);
        state.set_tool(ScreenshotTool::Counter);
        for expected in 1..=3u32 {
            state.begin_annotation(110.0 + expected as f32 * 10.0, 120.0);
            match state.annotations.last().expect("counter placed") {
                ScreenshotAnnotation::Counter { number, .. } => assert_eq!(*number, expected),
                other => panic!("expected a counter, got {other:?}"),
            }
        }
        assert_eq!(state.counter_next, 4);

        state.undo_annotation();
        assert_eq!(state.counter_next, 3, "an undone number must be reusable");
        state.redo_annotation();
        assert_eq!(state.counter_next, 4);
    }

    /// A click-placed tool has no drag, so it must land its mark on press —
    /// waiting for a motion would lose every careful click.
    #[test]
    fn a_click_placed_tool_needs_no_drag() {
        let mut state = editing(300, 200);
        state.set_tool(ScreenshotTool::Counter);
        state.begin_annotation(150.0, 150.0);
        assert_eq!(state.annotations.len(), 1);
        assert!(!state.drawing_annotation, "no drag is in flight");
    }

    #[test]
    fn typing_a_label_commits_it_and_an_empty_one_leaves_nothing_behind() {
        let mut state = editing(300, 200);
        state.set_tool(ScreenshotTool::Text);

        state.begin_annotation(140.0, 160.0);
        assert!(state.is_typing());
        for ch in "hi!".chars() {
            state.text_input(ch);
        }
        state.text_backspace();
        assert!(state.text_draft_preview().is_some());
        state.commit_text_draft();
        assert!(!state.is_typing());
        match state.annotations.last().expect("label committed") {
            ScreenshotAnnotation::Text { at, text, .. } => {
                assert_eq!(*at, (140.0, 160.0));
                assert_eq!(text, "hi");
            }
            other => panic!("expected text, got {other:?}"),
        }

        // A stray click with the text tool must not leave an empty label.
        state.begin_annotation(200.0, 160.0);
        state.commit_text_draft();
        assert_eq!(state.annotations.len(), 1);
    }

    /// Undo reaches the open draft before it reaches the canvas — the draft is
    /// the newest thing on screen, so anything else would feel like a skip.
    #[test]
    fn undo_cancels_an_open_label_before_touching_committed_marks() {
        let mut state = editing(300, 200);
        state.set_tool(ScreenshotTool::Text);
        state.begin_annotation(140.0, 160.0);
        state.text_input('a');
        state.commit_text_draft();

        state.begin_annotation(180.0, 160.0);
        state.text_input('b');
        assert!(state.is_typing());

        state.undo_annotation();
        assert!(!state.is_typing());
        assert_eq!(state.annotations.len(), 1, "the committed label survives");

        state.undo_annotation();
        assert!(state.annotations.is_empty());
    }

    #[test]
    fn a_marker_stroke_is_translucent_and_much_fatter_than_the_pen() {
        let mut state = editing(300, 200);
        state.set_tool(ScreenshotTool::Marker);
        assert_eq!(state.stroke_width(), state.line_width * MARKER_WIDTH_FACTOR);

        state.begin_annotation(110.0, 110.0);
        state.update_annotation(150.0, 140.0);
        state.commit_annotation();
        match state.annotations.last().expect("marker stroke") {
            ScreenshotAnnotation::Marker { color, width, .. } => {
                assert_eq!(color[3], MARKER_ALPHA);
                assert_eq!(*width, state.line_width * MARKER_WIDTH_FACTOR);
            }
            other => panic!("expected a marker, got {other:?}"),
        }

        state.set_tool(ScreenshotTool::Pencil);
        assert_eq!(state.stroke_width(), state.line_width);
    }

    #[test]
    fn the_toolbar_offers_every_tool_and_marks_the_current_one() {
        let mut state = editing(640, 480);
        state.set_tool(ScreenshotTool::Arrow);
        let entries = state.toolbar_entries();

        for tool in ScreenshotTool::PALETTE {
            let found = entries
                .iter()
                .find(|entry| entry.command == Some(ToolbarCommand::SelectTool(tool)))
                .unwrap_or_else(|| panic!("{tool:?} has no button"));
            assert_eq!(
                found.button.active,
                tool == ScreenshotTool::Arrow,
                "{tool:?} selected state"
            );
        }

        // Every ending is reachable without a keyboard.
        for command in [
            ToolbarCommand::Undo,
            ToolbarCommand::Redo,
            ToolbarCommand::Copy,
            ToolbarCommand::Save,
            ToolbarCommand::Cancel,
            ToolbarCommand::NextColor,
            ToolbarCommand::Thinner,
            ToolbarCommand::Thicker,
        ] {
            assert!(
                entries.iter().any(|entry| entry.command == Some(command)),
                "{command:?} has no button"
            );
        }
    }

    /// The readout is what tells you the capture is 640×480 before you take
    /// it, and it is the one cell that must never swallow a click.
    #[test]
    fn the_size_readout_shows_the_selection_and_is_not_clickable() {
        let state = editing(640, 480);
        let entries = state.toolbar_entries();
        let label = entries
            .iter()
            .find(|entry| matches!(entry.button.face, ButtonFace::Label(_)))
            .expect("a size readout");
        match &label.button.face {
            ButtonFace::Label(text) => assert_eq!(text, "640×480"),
            other => panic!("expected a label, got {other:?}"),
        }
        assert!(label.command.is_none());
        assert!(!label.button.enabled);
    }

    #[test]
    fn controls_with_nothing_to_do_are_disabled_rather_than_missing() {
        let mut state = editing(300, 200);
        let disabled = |state: &ScreenshotState, command| {
            state
                .toolbar_entries()
                .into_iter()
                .find(|entry| entry.command == Some(command))
                .map(|entry| !entry.button.enabled)
                .expect("button exists")
        };

        assert!(
            disabled(&state, ToolbarCommand::Undo),
            "nothing to undo yet"
        );
        assert!(
            disabled(&state, ToolbarCommand::Redo),
            "nothing to redo yet"
        );
        assert!(!disabled(&state, ToolbarCommand::Thinner));
        assert!(!disabled(&state, ToolbarCommand::Thicker));

        while state.line_width > MIN_LINE_WIDTH {
            state.decrease_line_width();
        }
        assert!(disabled(&state, ToolbarCommand::Thinner), "at the floor");
        while state.line_width < MAX_LINE_WIDTH {
            state.increase_line_width();
        }
        assert!(disabled(&state, ToolbarCommand::Thicker), "at the ceiling");

        state.set_tool(ScreenshotTool::Line);
        state.begin_annotation(110.0, 110.0);
        state.update_annotation(200.0, 180.0);
        state.commit_annotation();
        assert!(!disabled(&state, ToolbarCommand::Undo));

        // …and the row keeps the same number of cells throughout, so it never
        // reflows under the pointer mid-edit.
        let count = state.toolbar_entries().len();
        state.undo_annotation();
        assert_eq!(state.toolbar_entries().len(), count);
    }

    #[test]
    fn the_swatch_walks_the_palette_and_carries_the_current_ink() {
        let mut state = editing(300, 200);
        assert_eq!(state.color, PALETTE[0]);
        state.next_palette_color();
        assert_eq!(state.color, PALETTE[1]);

        let swatch = state
            .toolbar_entries()
            .into_iter()
            .find(|entry| entry.command == Some(ToolbarCommand::NextColor))
            .expect("a swatch");
        assert_eq!(swatch.button.tint, Some(PALETTE[1]));

        for _ in 0..PALETTE.len() {
            state.next_palette_color();
        }
        assert_eq!(state.color, PALETTE[1], "the ring closes");
    }

    #[test]
    fn line_width_stays_inside_its_bounds() {
        let mut state = editing(300, 200);
        for _ in 0..100 {
            state.increase_line_width();
        }
        assert_eq!(state.line_width, MAX_LINE_WIDTH);
        for _ in 0..100 {
            state.decrease_line_width();
        }
        assert_eq!(state.line_width, MIN_LINE_WIDTH);
    }

    /// Every derived metric has to move with the width control, or the one
    /// slider only reaches half the tools.
    #[test]
    fn the_width_control_scales_the_click_placed_tools_too() {
        let mut state = editing(300, 200);
        let (radius, size, block) = (
            state.counter_radius(),
            state.text_size(),
            state.pixelate_block(),
        );
        for _ in 0..8 {
            state.increase_line_width();
        }
        assert!(state.counter_radius() > radius);
        assert!(state.text_size() > size);
        assert!(state.pixelate_block() > block);
    }

    #[test]
    fn redrawing_the_selection_clears_the_editor() {
        let mut state = editing(300, 200);
        state.set_tool(ScreenshotTool::Counter);
        state.begin_annotation(150.0, 150.0);
        state.hovered_button = Some(2);
        assert!(!state.annotations.is_empty());

        state.reset_selection();
        assert!(state.annotations.is_empty());
        assert!(state.undone.is_empty());
        assert!(state.toolbar.is_none());
        assert_eq!(state.hovered_button, None);
        assert_eq!(state.counter_next, 1);
        assert!(!state.is_typing());
    }

    /// Moving a committed selection has always carried its marks with it; the
    /// click-placed ones must not be the exception.
    #[test]
    fn moving_the_selection_carries_labels_and_counters_along() {
        let mut state = editing(60, 60);
        state.set_tool(ScreenshotTool::Counter);
        state.begin_annotation(120.0, 130.0);
        state.set_tool(ScreenshotTool::Text);
        state.begin_annotation(140.0, 150.0);
        state.text_input('x');
        state.commit_text_draft();

        state.move_selection(10.0, -5.0);
        match &state.annotations[0] {
            ScreenshotAnnotation::Counter { at, .. } => assert_eq!(*at, (130.0, 125.0)),
            other => panic!("expected a counter, got {other:?}"),
        }
        match &state.annotations[1] {
            ScreenshotAnnotation::Text { at, .. } => assert_eq!(*at, (150.0, 145.0)),
            other => panic!("expected text, got {other:?}"),
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
