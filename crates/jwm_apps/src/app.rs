#[cfg(feature = "accessibility")]
pub(crate) mod accessibility;

use crate::{
    editor::Editor,
    notes::{Notes, SaveError},
    ui::{self, Icon, Rect},
    viewer::Viewer,
};
use gio::prelude::*;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use winit::{
    event::KeyEvent,
    keyboard::{Key, ModifiersState, NamedKey},
};

pub(crate) enum Document {
    Notes(Box<Notes>),
    Image(Box<Viewer>),
}
#[derive(Clone, PartialEq)]
enum Action {
    New,
    Open,
    Save,
    KeepConflictCopy,
    SaveAs(bool),
    Trash,
    Previous,
    Next,
    ZoomIn,
    ZoomOut,
    Actual,
    Fit,
    Rotate,
    Fullscreen,
    Minimize,
    Maximize,
    Close,
    Search,
    RefreshNotebook,
    ReloadExternal,
    Find,
    FindSelection,
    FindField,
    FindNext(bool),
    FindClose,
    Edit,
    TogglePreview,
    ToggleCodeView,
    Preview,
    Note(PathBuf, u64),
    Cancel,
    Accept,
    Parent,
    Entry(PathBuf),
    Discard,
    SaveClose,
    ConfirmTrash,
    Overwrite(PathBuf, bool),
}
pub(crate) enum Effect {
    OpenNote(PathBuf),
    SaveAs(PathBuf, bool),
    KeepConflictCopy,
    Notebook,
    Close,
    Minimize,
    Maximize,
    Fullscreen,
    LeaveFullscreen,
}
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Preview,
    Editor,
    Search,
    Results,
    Find,
}
enum PickerMode {
    Notes,
    Images,
    Save(bool),
}
struct Picker {
    mode: PickerMode,
    path: Editor,
    entries: Vec<PathBuf>,
    offset: usize,
}
const CONFLICT_TITLE: &str = "原文件已被修改";
const CONFLICT_DESCRIPTION: &str =
    "原文件未覆盖。可将当前修改保存为同目录的恢复副本并继续编辑，或另存到其他位置。";
const CONFLICT_ACTIONS: [(&str, Action); 3] = [
    ("取消", Action::Cancel),
    ("另存为", Action::SaveAs(false)),
    ("保留两份", Action::KeepConflictCopy),
];

enum Dialog {
    Picker(Box<Picker>),
    Error(String),
    SaveConflict,
    Unsaved,
    Trash,
    Overwrite(PathBuf, bool),
}
struct Hit {
    rect: Rect,
    action: Action,
    label: String,
}
pub(crate) struct App {
    pub document: Document,
    pub effects: Vec<Effect>,
    dialog: Option<Dialog>,
    dialog_focus: Option<usize>,
    conflict_revision: u64,
    focus: Focus,
    result_path: Option<PathBuf>,
    result_index: Option<usize>,
    result_enter: bool,
    hits: Vec<Hit>,
    pub pointer: (f64, f64),
    pub size: (f64, f64),
    dragging: bool,
    pub focused: bool,
    pub caret: bool,
    next_blink: Instant,
}
impl App {
    pub fn new(notes: bool, paths: Vec<PathBuf>) -> Result<Self, String> {
        let document = if notes {
            Document::Notes(Box::new(Notes::new(paths.into_iter().next())?))
        } else {
            Document::Image(Box::new(Viewer::new(paths)))
        };
        Ok(Self {
            document,
            effects: vec![],
            dialog: None,
            dialog_focus: None,
            conflict_revision: 0,
            focus: Focus::Editor,
            result_path: None,
            result_index: None,
            result_enter: false,
            hits: vec![],
            pointer: (0., 0.),
            size: (960., 680.),
            dragging: false,
            focused: true,
            caret: true,
            next_blink: Instant::now() + Duration::from_millis(500),
        })
    }
    pub fn title(&self) -> String {
        match &self.document {
            Document::Notes(n) => format!("{} — JWM 备忘录", n.title()),
            Document::Image(v) => format!("{} — JWM 图片查看器", v.title()),
        }
    }
    pub fn set_search_wake(&mut self, wake: crate::notes::SearchWake) {
        if let Document::Notes(notes) = &mut self.document {
            notes.set_search_wake(wake);
        }
    }
    pub fn poll_search(&mut self) -> bool {
        match &mut self.document {
            Document::Notes(notes) => notes.poll_search(),
            Document::Image(_) => false,
        }
    }
    pub fn loading(&self) -> bool {
        matches!(&self.document,Document::Image(v) if v.loading())
    }
    pub fn show_error(&mut self, error: String) {
        self.error(error);
    }
    fn error(&mut self, error: String) {
        self.dialog_focus = None;
        self.dialog = Some(Dialog::Error(error));
    }
    pub fn tick(&mut self) -> bool {
        let mut dirty = self.poll_search();
        match &mut self.document {
            Document::Notes(n) if n.due.is_some_and(|due| Instant::now() >= due) => {
                if let Err(error) = n.autosave() {
                    n.status = "保存失败；内容仍在编辑器中，请重试或另存为".into();
                    self.error(error);
                }
                dirty = true;
            }
            Document::Image(v) => dirty = v.poll(),
            _ => (),
        }
        if Instant::now() >= self.next_blink {
            self.caret = !self.caret;
            self.next_blink = Instant::now() + Duration::from_millis(500);
            dirty |= self.focused
                && (matches!(self.document, Document::Notes(_))
                    || matches!(self.dialog, Some(Dialog::Picker(_))));
        }
        dirty
    }
    fn sidebar(&self) -> f64 {
        if matches!(&self.document,Document::Notes(n) if n.managed) {
            236. * ui::density()
        } else {
            0.
        }
    }
    fn body(&self) -> Rect {
        let find_height = if matches!(&self.document, Document::Notes(n) if n.find.enabled) {
            48.
        } else {
            0.
        };
        Rect::new(
            0.,
            ui::header() + ui::tools() + find_height,
            self.size.0,
            (self.size.1 - ui::header() - ui::tools() - ui::status() - find_height).max(1.),
        )
    }
    fn button(
        &mut self,
        cr: &cairo::Context,
        rect: Rect,
        icon: Icon,
        action: Action,
        label: &str,
        enabled: bool,
    ) {
        if rect.contains(self.pointer.0, self.pointer.1) && enabled {
            ui::rounded(cr, rect, ui::hover());
        }
        let _ = cr.save();
        if !enabled {
            cr.push_group();
        }
        ui::icon(
            cr,
            icon,
            rect.x + (rect.w - 19.2) / 2.,
            rect.y + (rect.h - 19.2) / 2.,
        );
        if !enabled {
            let _ = cr.pop_group_to_source();
            let _ = cr.paint_with_alpha(0.3);
        }
        let _ = cr.restore();
        if enabled {
            self.hits.push(Hit {
                rect,
                action,
                label: label.into(),
            });
        }
    }
    fn text_button(
        &mut self,
        cr: &cairo::Context,
        rect: Rect,
        label: &str,
        action: Action,
        accent: bool,
    ) {
        ui::rounded(cr, rect, if accent { ui::select() } else { ui::hover() });
        ui::text(
            cr,
            label,
            Rect::new(rect.x + 12., rect.y, rect.w - 20., rect.h),
            13.,
            ui::text_color(),
        );
        self.hits.push(Hit {
            rect,
            action,
            label: label.into(),
        });
    }
    pub fn draw(&mut self, cr: &cairo::Context, width: f64, height: f64, maximized: bool) {
        let resized = self.size != (width, height);
        self.size = (width, height);
        if self.focus == Focus::Results
            && !matches!(&self.document, Document::Notes(n) if n.painted_refresh())
        {
            let index = self.result_index();
            if resized || index != self.result_index {
                self.reveal_result(index);
            }
        }
        self.hits.clear();
        ui::fill(cr, Rect::new(0., 0., width, height), ui::canvas());
        ui::fill(cr, Rect::new(0., 0., width, ui::header()), ui::panel());
        let notes = matches!(self.document, Document::Notes(_));
        ui::icon(
            cr,
            if notes { Icon::Notes } else { Icon::Image },
            20.,
            (ui::header() - 20.) / 2.,
        );
        ui::text(
            cr,
            if notes {
                "JWM 备忘录"
            } else {
                "JWM 图片查看器"
            },
            Rect::new(50., 0., 170., ui::header()),
            14.,
            ui::text_color(),
        );
        let title = match &self.document {
            Document::Notes(n) => n.title(),
            Document::Image(v) => v.title(),
        };
        ui::text(
            cr,
            &title,
            Rect::new(230., 0., (width - 370.).max(1.), ui::header()),
            13.,
            ui::muted(),
        );
        for (offset, icon, action, label) in [
            (118., Icon::Minimize, Action::Minimize, "最小化"),
            (
                80.,
                if maximized {
                    Icon::Restore
                } else {
                    Icon::Maximize
                },
                Action::Maximize,
                "最大化 / 还原",
            ),
            (42., Icon::Close, Action::Close, "关闭 (Ctrl+W)"),
        ] {
            self.button(
                cr,
                Rect::new(width - offset, (ui::header() - 32.) / 2., 32., 32.),
                icon,
                action,
                label,
                true,
            );
        }
        ui::fill(cr, Rect::new(0., ui::header(), width, 1.), ui::line());
        let mut tools = vec![(Icon::Folder, Action::Open, "打开 (Ctrl+O)", true)];
        match &self.document {
            Document::Notes(n) => {
                tools.extend([
                    (Icon::New, Action::New, "新建备忘录 (Ctrl+N)", true),
                    (
                        Icon::Save,
                        Action::Save,
                        "保存 (Ctrl+S)",
                        n.document.is_some(),
                    ),
                    (
                        Icon::Export,
                        Action::SaveAs(false),
                        "导出 / 另存为 (Ctrl+Shift+S)",
                        true,
                    ),
                ]);
                if n.managed {
                    tools.push((Icon::Trash, Action::Trash, "移至废纸篓", true));
                }
            }
            Document::Image(v) => {
                let loaded = v.image.is_some();
                let multiple = v.paths.len() > 1;
                tools.extend([
                    (Icon::Previous, Action::Previous, "上一张 (←)", multiple),
                    (Icon::Next, Action::Next, "下一张 (→)", multiple),
                    (
                        Icon::ZoomOut,
                        Action::ZoomOut,
                        "缩小 (- / Ctrl+滚轮)",
                        loaded,
                    ),
                    (Icon::ZoomIn, Action::ZoomIn, "放大 (+ / Ctrl+滚轮)", loaded),
                    (Icon::Actual, Action::Actual, "原始大小 (1)", loaded),
                    (Icon::Fit, Action::Fit, "适应窗口 (F)", loaded),
                    (Icon::Rotate, Action::Rotate, "顺时针旋转 (R)", loaded),
                    (Icon::Fullscreen, Action::Fullscreen, "全屏 (F11)", true),
                ]);
            }
        }
        for (i, (icon, action, label, enabled)) in tools.into_iter().enumerate() {
            self.button(
                cr,
                Rect::new(
                    12. + i as f64 * 38.,
                    ui::header() + (ui::tools() - 32.) / 2.,
                    32.,
                    32.,
                ),
                icon,
                action,
                label,
                enabled,
            );
        }
        if let Document::Notes(n) = &self.document {
            let label = if n.preview.enabled {
                "关闭 MD 预览"
            } else {
                "MD 预览"
            };
            self.text_button(
                cr,
                Rect::new(220., ui::header() + 6., 134., ui::tools() - 12.),
                label,
                Action::TogglePreview,
                false,
            );
        }
        if matches!(&self.document, Document::Notes(n) if n.managed) {
            self.text_button(
                cr,
                Rect::new(366., ui::header() + 6., 119., ui::tools() - 12.),
                "刷新列表 F5",
                Action::RefreshNotebook,
                false,
            );
        }
        if matches!(&self.document, Document::Notes(n) if !n.managed) {
            self.button(
                cr,
                Rect::new(164., ui::header() + (ui::tools() - 32.) / 2., 32., 32.),
                Icon::Reload,
                Action::ReloadExternal,
                "从磁盘重新载入 (F5)",
                true,
            );
        }
        if matches!(&self.document, Document::Notes(n) if !n.managed) {
            self.text_button(
                cr,
                Rect::new(366., ui::header() + 6., 110., ui::tools() - 12.),
                "查找 Ctrl+F",
                Action::Find,
                false,
            );
        }
        if let Document::Notes(n) = &self.document
            && !n.managed
        {
            let label = if n.editor.monospace {
                "主题正文"
            } else {
                "等宽代码"
            };
            self.text_button(
                cr,
                Rect::new(488., ui::header() + 6., 110., ui::tools() - 12.),
                label,
                Action::ToggleCodeView,
                false,
            );
        }
        if matches!(&self.document, Document::Notes(n) if n.find.enabled) {
            let y = ui::header() + ui::tools();
            let field_width = (width - 370.).max(80.);
            ui::rounded(cr, Rect::new(12., y + 6., field_width, 36.), ui::canvas());
            if let Document::Notes(n) = &mut self.document {
                n.find.query.draw(
                    cr,
                    Rect::new(20., y + 13., field_width - 16., 22.),
                    self.dialog.is_none() && self.focus == Focus::Find,
                    self.caret,
                    false,
                );
                ui::text(
                    cr,
                    &n.find.label(),
                    Rect::new(field_width + 24., y + 6., 146., 36.),
                    12.,
                    ui::muted(),
                );
            }
            self.hits.push(Hit {
                rect: Rect::new(12., y + 6., field_width, 36.),
                action: Action::FindField,
                label: "文内查找".into(),
            });
            for (offset, icon, action, label) in [
                (
                    132.,
                    Icon::Previous,
                    Action::FindNext(true),
                    "上一处 (Shift+Enter)",
                ),
                (92., Icon::Next, Action::FindNext(false), "下一处 (Enter)"),
                (52., Icon::Close, Action::FindClose, "关闭查找 (Esc)"),
            ] {
                self.button(
                    cr,
                    Rect::new(width - offset, y + 8., 32., 32.),
                    icon,
                    action,
                    label,
                    true,
                );
            }
        }
        let body = self.body();
        let sidebar = self.sidebar();
        match &mut self.document {
            Document::Notes(n) => {
                ui::fill(cr, body, ui::panel());
                if n.managed {
                    ui::fill(cr, Rect::new(sidebar - 1., body.y, 1., body.h), ui::line());
                    ui::rounded(
                        cr,
                        Rect::new(12., body.y + 12., sidebar - 24., 36.),
                        ui::canvas(),
                    );
                    let search = Rect::new(20., body.y + 20., sidebar - 40., 22.);
                    n.search.draw(
                        cr,
                        search,
                        self.dialog.is_none() && self.focus == Focus::Search,
                        self.caret,
                        false,
                    );
                    if n.search.text.is_empty() && self.focus != Focus::Search {
                        ui::text(cr, "搜索备忘录 (Ctrl+F)", search, 13., ui::muted());
                    }
                    self.hits.push(Hit {
                        rect: Rect::new(12., body.y + 12., sidebar - 24., 36.),
                        action: Action::Search,
                        label: String::new(),
                    });
                    let _ = cr.save();
                    cr.rectangle(0., body.y + 60., sidebar, body.h - 60.);
                    cr.clip();
                    let keep_painted = n.painted_refresh();
                    if n.search_pending() && !keep_painted {
                        ui::text(
                            cr,
                            "正在搜索…",
                            Rect::new(20., body.y + 60., sidebar - 40., 40.),
                            13.,
                            ui::muted(),
                        );
                    }
                    let generation = n.current_rows();
                    let rows = if generation.is_some() || keep_painted {
                        n.rows.as_slice()
                    } else {
                        &[]
                    };
                    for (i, (path, title)) in rows.iter().enumerate() {
                        let rect = Rect::new(
                            8.,
                            body.y + 60. + i as f64 * 48. - n.list_scroll,
                            sidebar - 16.,
                            44.,
                        );
                        if rect.y + rect.h <= body.y + 60. || rect.y >= body.y + body.h {
                            continue;
                        }
                        let selected = if self.focus == Focus::Results {
                            self.result_path.as_ref() == Some(path)
                        } else {
                            n.document.as_ref().and_then(|d| d.file.path()).as_ref() == Some(path)
                        };
                        if selected {
                            ui::rounded(cr, rect, ui::select());
                        } else if generation.is_some()
                            && rect.contains(self.pointer.0, self.pointer.1)
                        {
                            ui::rounded(cr, rect, ui::hover());
                        }
                        ui::text(
                            cr,
                            title,
                            Rect::new(rect.x + 12., rect.y, rect.w - 24., rect.h),
                            14.,
                            ui::text_color(),
                        );
                        if let Some(generation) = generation {
                            self.hits.push(Hit {
                                rect: Rect::new(
                                    rect.x,
                                    rect.y.max(body.y + 60.),
                                    rect.w,
                                    rect.h.min((body.y + body.h - rect.y).max(0.)),
                                ),
                                action: Action::Note(path.clone(), generation),
                                label: title.clone(),
                            });
                        }
                    }
                    let _ = cr.restore();
                }
                let split = if n.preview.enabled {
                    sidebar + (body.w - sidebar) / 2.
                } else {
                    body.w
                };
                let edit = Rect::new(
                    sidebar + 24.,
                    body.y + 24.,
                    (split - sidebar - 48.).max(1.),
                    (body.h - 48.).max(1.),
                );
                n.editor.highlight_selection =
                    n.find.enabled && n.find.total > 0 && self.focus == Focus::Find;
                n.editor.draw(
                    cr,
                    edit,
                    self.dialog.is_none() && self.focus == Focus::Editor && self.focused,
                    self.caret,
                    true,
                );
                self.hits.push(Hit {
                    rect: Rect::new(sidebar, body.y, split - sidebar, body.h),
                    action: Action::Edit,
                    label: String::new(),
                });
                if n.preview.enabled {
                    let divider =
                        if self.focus == Focus::Preview && self.focused && self.dialog.is_none() {
                            ui::accent()
                        } else {
                            ui::line()
                        };
                    ui::fill(cr, Rect::new(split, body.y, 1., body.h), divider);
                    ui::text(
                        cr,
                        "Markdown · 只读 / 无网络",
                        Rect::new(split + 20., body.y, (body.w - split - 40.).max(1.), 30.),
                        12.,
                        ui::muted(),
                    );
                    n.preview.draw(
                        cr,
                        Rect::new(
                            split + 24.,
                            body.y + 38.,
                            (body.w - split - 48.).max(1.),
                            (body.h - 54.).max(1.),
                        ),
                        &n.editor.text,
                        n.editor.revision,
                    );
                    self.hits.push(Hit {
                        rect: Rect::new(split, body.y, body.w - split, body.h),
                        action: Action::Preview,
                        label: String::new(),
                    });
                }
            }
            Document::Image(v) => {
                v.clamp_pan(body.w, body.h);
                if let Some(image) = &v.image {
                    let (w, h) = v.dimensions();
                    let scale = v.scale(body.w, body.h);
                    let _ = cr.save();
                    cr.rectangle(body.x, body.y, body.w, body.h);
                    cr.clip();
                    cr.translate(body.w / 2. + v.pan.0, body.y + body.h / 2. + v.pan.1);
                    cr.scale(scale, scale);
                    cr.rotate(v.rotation as f64 * std::f64::consts::FRAC_PI_2);
                    cr.translate(-(image.width() as f64) / 2., -(image.height() as f64) / 2.);
                    let _ = cr.set_source_surface(image, 0., 0.);
                    cr.source().set_filter(cairo::Filter::Bilinear);
                    let _ = cr.paint();
                    let _ = cr.restore();
                    v.status = format!(
                        "{}  ·  {} × {}  ·  {}  ·  {}",
                        v.collection_status(),
                        w as u32,
                        h as u32,
                        if v.fit {
                            "适应窗口".into()
                        } else {
                            format!("{:.0}%", v.zoom * 100.)
                        },
                        v.paths[v.index].display()
                    );
                } else {
                    let layout = ui::layout(cr, &v.message, 14., (width - 100.).max(1.), true);
                    layout.set_alignment(pango::Alignment::Center);
                    ui::color(cr, ui::muted());
                    cr.move_to(50., body.y + (body.h - layout.pixel_size().1 as f64) / 2.);
                    pangocairo::functions::show_layout(cr, &layout);
                }
            }
        }
        ui::fill(
            cr,
            Rect::new(0., height - ui::status(), width, ui::status()),
            ui::panel(),
        );
        ui::fill(
            cr,
            Rect::new(0., height - ui::status(), width, 1.),
            ui::line(),
        );
        let status = match &mut self.document {
            Document::Notes(n) => n.editor_status(),
            Document::Image(v) => v.status.clone(),
        };
        let tooltip = self
            .hits
            .iter()
            .find(|h| h.rect.contains(self.pointer.0, self.pointer.1) && !h.label.is_empty())
            .map(|h| h.label.as_str())
            .unwrap_or(&status);
        ui::text(
            cr,
            tooltip,
            Rect::new(16., height - ui::status(), width - 32., ui::status()),
            12.,
            ui::muted(),
        );
        if self.dialog.is_some() {
            self.draw_dialog(cr);
            if let Some(action) = self
                .dialog_focus
                .and_then(|i| self.dialog_actions().get(i).cloned())
                && let Some(hit) = self.hits.iter().find(|h| h.action == action)
            {
                ui::color(cr, ui::accent());
                cr.set_line_width(2.);
                cr.rectangle(hit.rect.x, hit.rect.y, hit.rect.w, hit.rect.h);
                let _ = cr.stroke();
            }
        }
    }
    fn draw_dialog(&mut self, cr: &cairo::Context) {
        self.hits.clear();
        cr.set_source_rgba(0., 0., 0., 0.65);
        let _ = cr.paint();
        let w = (self.size.0 - 48.).min(640.);
        let h = if matches!(self.dialog, Some(Dialog::Picker(_))) {
            (self.size.1 - 48.).min(480.)
        } else {
            300_f64.min(self.size.1 - 32.)
        };
        let r = Rect::new((self.size.0 - w) / 2., (self.size.1 - h) / 2., w, h);
        ui::rounded(cr, r, ui::panel());
        let dialog = self.dialog.take().unwrap();
        match dialog {
            Dialog::Picker(mut picker) => {
                let title = match picker.mode {
                    PickerMode::Notes => "打开 UTF-8 文本文件",
                    PickerMode::Images => "打开图片",
                    PickerMode::Save(_) => "导出 / 另存为文本文件",
                };
                ui::text(
                    cr,
                    title,
                    Rect::new(r.x + 24., r.y + 12., r.w - 48., 32.),
                    16.,
                    ui::text_color(),
                );
                ui::rounded(
                    cr,
                    Rect::new(r.x + 20., r.y + 56., r.w - 40., 38.),
                    ui::canvas(),
                );
                picker.path.draw(
                    cr,
                    Rect::new(r.x + 30., r.y + 64., r.w - 60., 24.),
                    self.dialog_focus.is_none(),
                    self.caret,
                    false,
                );
                let rows = ((r.h - 170.) / 32.).max(0.) as usize;
                for (i, path) in picker
                    .entries
                    .iter()
                    .skip(picker.offset)
                    .take(rows)
                    .enumerate()
                {
                    let rect = Rect::new(r.x + 24., r.y + 102. + i as f64 * 32., r.w - 48., 30.);
                    if rect.contains(self.pointer.0, self.pointer.1) {
                        ui::rounded(cr, rect, ui::hover());
                    }
                    let label = format!(
                        "{}{}",
                        if path.is_dir() { "▸  " } else { "    " },
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                    ui::text(cr, &label, rect, 13., ui::text_color());
                    self.hits.push(Hit {
                        rect,
                        action: Action::Entry(path.clone()),
                        label,
                    });
                }
                self.text_button(
                    cr,
                    Rect::new(r.x + 24., r.y + r.h - 52., 100., 32.),
                    "上级目录",
                    Action::Parent,
                    false,
                );
                self.text_button(
                    cr,
                    Rect::new(r.x + r.w - 204., r.y + r.h - 52., 80., 32.),
                    "取消",
                    Action::Cancel,
                    false,
                );
                self.text_button(
                    cr,
                    Rect::new(r.x + r.w - 112., r.y + r.h - 52., 88., 32.),
                    if matches!(picker.mode, PickerMode::Save(_)) {
                        "保存"
                    } else {
                        "打开"
                    },
                    Action::Accept,
                    true,
                );
                self.dialog = Some(Dialog::Picker(picker));
            }
            dialog => {
                let (title, message) = match &dialog {
                    Dialog::Error(e) => ("操作未完成", e.as_str()),
                    Dialog::SaveConflict => (CONFLICT_TITLE, CONFLICT_DESCRIPTION),
                    Dialog::Unsaved => (
                        "保存当前修改？",
                        "当前内容尚未保存。可以保存、另存为，或放弃修改。",
                    ),
                    Dialog::Trash => ("移至废纸篓？", "可以从系统废纸篓恢复此备忘录。"),
                    Dialog::Overwrite(_, _) => {
                        ("替换现有文件？", "目标文件已存在，保存将替换其内容。")
                    }
                    _ => unreachable!(),
                };
                ui::text(
                    cr,
                    title,
                    Rect::new(r.x + 24., r.y + 16., r.w - 48., 36.),
                    17.,
                    ui::text_color(),
                );
                let layout = ui::layout(cr, message, 14., r.w - 48., true);
                layout.set_height(((r.h - 124.) * pango::SCALE as f64) as i32);
                layout.set_ellipsize(pango::EllipsizeMode::End);
                ui::color(cr, ui::muted());
                cr.move_to(r.x + 24., r.y + 72.);
                pangocairo::functions::show_layout(cr, &layout);
                let buttons = match &dialog {
                    Dialog::SaveConflict => CONFLICT_ACTIONS.to_vec(),
                    Dialog::Unsaved => vec![
                        ("取消", Action::Cancel),
                        ("放弃修改", Action::Discard),
                        ("保存", Action::SaveClose),
                        ("另存为", Action::SaveAs(true)),
                    ],
                    Dialog::Trash => vec![
                        ("取消", Action::Cancel),
                        ("移至废纸篓", Action::ConfirmTrash),
                    ],
                    Dialog::Overwrite(path, close) => vec![
                        ("取消", Action::Cancel),
                        ("替换", Action::Overwrite(path.clone(), *close)),
                    ],
                    _ => vec![("确定", Action::Cancel)],
                };
                let count = buttons.len();
                for (i, (label, action)) in buttons.into_iter().enumerate() {
                    self.text_button(
                        cr,
                        Rect::new(
                            r.x + r.w - 24. - (count - i) as f64 * 104.,
                            r.y + r.h - 56.,
                            96.,
                            32.,
                        ),
                        label,
                        action,
                        i == count - 1,
                    );
                }
                self.dialog = Some(dialog);
            }
        }
    }
    fn dialog_actions(&self) -> Vec<Action> {
        match &self.dialog {
            Some(Dialog::Picker(_)) => vec![Action::Cancel, Action::Accept, Action::Parent],
            Some(Dialog::Unsaved) => vec![
                Action::Cancel,
                Action::Discard,
                Action::SaveClose,
                Action::SaveAs(true),
            ],
            Some(Dialog::Trash) => vec![Action::Cancel, Action::ConfirmTrash],
            Some(Dialog::Overwrite(path, close)) => {
                vec![Action::Cancel, Action::Overwrite(path.clone(), *close)]
            }
            Some(Dialog::Error(_)) => vec![Action::Cancel],
            Some(Dialog::SaveConflict) => CONFLICT_ACTIONS
                .iter()
                .map(|(_, action)| action.clone())
                .collect(),
            None => vec![],
        }
    }
    fn picker(&mut self, mode: PickerMode) {
        self.dialog_focus = None;
        let path = match (&self.document, &mode) {
            (Document::Notes(n), PickerMode::Save(_)) => gio::glib::home_dir().join(format!(
                "{}.txt",
                crate::note_title(&n.editor.text).replace(['/', '\\'], "_")
            )),
            (Document::Notes(n), _) => n
                .document
                .as_ref()
                .and_then(|d| d.file.path())
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .unwrap_or_else(gio::glib::home_dir),
            (Document::Image(v), _) => v
                .paths
                .get(v.index)
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .unwrap_or_else(gio::glib::home_dir),
        };
        let mut picker = Picker {
            mode,
            path: Editor::new(path.to_string_lossy().into_owned()),
            entries: vec![],
            offset: 0,
        };
        picker.path.select_all();
        picker.refresh();
        self.dialog = Some(Dialog::Picker(Box::new(picker)));
    }
    fn accept_picker(&mut self) {
        let Some(Dialog::Picker(mut picker)) = self.dialog.take() else {
            return;
        };
        let path = picker.resolve();
        if path.is_dir() {
            picker.path = Editor::new(path.to_string_lossy().into_owned());
            picker.refresh();
            self.dialog = Some(Dialog::Picker(picker));
            return;
        }
        match picker.mode {
            PickerMode::Notes => {
                self.effects.push(Effect::OpenNote(path));
            }
            PickerMode::Images => {
                if let Document::Image(v) = &mut self.document {
                    v.open(vec![path]);
                }
            }
            PickerMode::Save(close) => {
                let same = matches!(&self.document,Document::Notes(n) if n.document.as_ref().and_then(|d|d.file.path()).as_ref()==Some(&path));
                if path.exists() && !same {
                    self.dialog = Some(Dialog::Overwrite(path, close));
                } else {
                    self.effects.push(Effect::SaveAs(path, close));
                }
            }
        }
    }
    pub(crate) fn write_as(&mut self, path: &Path, close: bool, same_document: bool) -> bool {
        let Document::Notes(n) = &mut self.document else {
            return false;
        };
        let result = if same_document {
            n.save()
        } else {
            n.save_as(path)
        };
        if let Err(error) = result {
            self.error(error);
            return false;
        }
        // A notebook export does not discard an unsaved source note.
        if close
            && n.managed
            && let Err(error) = n.save()
        {
            self.error(error);
            return false;
        }
        self.dialog = None;
        close
    }
    pub(crate) fn write_conflict_copy(&mut self, reserved: impl Fn(&Path) -> bool) {
        if !matches!(self.dialog, Some(Dialog::SaveConflict)) {
            return;
        }
        if let Document::Notes(n) = &mut self.document {
            match n.keep_conflict_copy(reserved) {
                Ok(_) => {
                    self.dialog = None;
                    self.focus = Focus::Editor;
                    self.dragging = false;
                }
                Err(error) => self.error(error),
            }
        }
    }

    fn action(&mut self, action: Action) {
        if let Action::Note(path, generation) = &action
            && !matches!(&self.document, Document::Notes(n)
                if n.current_rows() == Some(*generation) && n.rows.iter().any(|(p, _)| p == path))
        {
            return;
        }
        self.dialog_focus = None;
        match action {
            Action::Minimize => self.effects.push(Effect::Minimize),
            Action::Maximize => self.effects.push(Effect::Maximize),
            Action::Fullscreen => self.effects.push(Effect::Fullscreen),
            Action::Close => {
                if let Document::Notes(n) = &mut self.document
                    && n.dirty
                    && (!n.managed || n.save().is_err())
                {
                    self.dialog = Some(Dialog::Unsaved);
                    return;
                }
                self.effects.push(Effect::Close);
            }
            Action::Open => self.picker(if matches!(self.document, Document::Notes(_)) {
                PickerMode::Notes
            } else {
                PickerMode::Images
            }),
            Action::Save => {
                if let Document::Notes(n) = &mut self.document {
                    match n.save_explicitly() {
                        Ok(()) => (),
                        Err(SaveError::Conflict) => {
                            self.conflict_revision = self.conflict_revision.wrapping_add(1);
                            self.dialog = Some(Dialog::SaveConflict);
                        }
                        Err(SaveError::Other(error)) => self.error(error),
                    }
                }
            }
            Action::KeepConflictCopy => {
                if matches!(self.dialog, Some(Dialog::SaveConflict)) {
                    self.effects.push(Effect::KeepConflictCopy);
                }
            }
            Action::SaveAs(close) => self.picker(PickerMode::Save(close)),
            Action::Cancel => self.dialog = None,
            Action::Accept => self.accept_picker(),
            Action::Discard => self.effects.push(Effect::Close),
            Action::SaveClose => {
                if let Document::Notes(n) = &mut self.document {
                    match n.save() {
                        Ok(()) => self.effects.push(Effect::Close),
                        Err(e) => self.error(e),
                    }
                }
            }
            Action::Trash => self.dialog = Some(Dialog::Trash),
            Action::Overwrite(path, close) => self.effects.push(Effect::SaveAs(path, close)),
            Action::Parent => {
                if let Some(Dialog::Picker(p)) = &mut self.dialog {
                    let path = p.resolve();
                    let directory = if path.is_dir() {
                        path
                    } else {
                        path.parent().unwrap_or(Path::new("/")).into()
                    };
                    p.path = Editor::new(
                        directory
                            .parent()
                            .unwrap_or(Path::new("/"))
                            .to_string_lossy()
                            .into_owned(),
                    );
                    p.refresh();
                }
            }
            Action::Entry(path) => {
                if let Some(Dialog::Picker(p)) = &mut self.dialog {
                    p.path = Editor::new(path.to_string_lossy().into_owned());
                    if path.is_dir() {
                        p.refresh();
                    }
                }
            }
            action => {
                let body = self.body();
                let result = match &mut self.document {
                    Document::Notes(n) => match action {
                        Action::New => {
                            if n.managed {
                                self.focus = Focus::Editor;
                                n.new_note()
                            } else {
                                self.effects.push(Effect::Notebook);
                                Ok(())
                            }
                        }
                        Action::ToggleCodeView if !n.managed => {
                            n.editor.toggle_monospace();
                            Ok(())
                        }
                        Action::TogglePreview => {
                            n.preview.enabled = !n.preview.enabled;
                            self.focus = Focus::Editor;
                            n.editor.preedit.clear();
                            n.search.preedit.clear();
                            n.find.query.preedit.clear();
                            self.dragging = false;
                            Ok(())
                        }
                        Action::Preview => {
                            self.focus = Focus::Preview;
                            n.editor.preedit.clear();
                            n.search.preedit.clear();
                            n.find.query.preedit.clear();
                            Ok(())
                        }
                        Action::ConfirmTrash => {
                            self.dialog = None;
                            n.trash().map(|()| {
                                self.focus = Focus::Editor;
                                self.dragging = false;
                            })
                        }
                        Action::Find | Action::FindField => {
                            n.find.enabled = true;
                            self.focus = Focus::Find;
                            n.editor.preedit.clear();
                            n.find.sync(&mut n.editor);
                            if action == Action::Find {
                                n.find.query.select_all();
                            }
                            self.dragging = false;
                            Ok(())
                        }
                        Action::FindSelection => {
                            if !n.managed
                                && self.focus == Focus::Editor
                                && n.find.use_selection(&n.editor)
                            {
                                self.focus = Focus::Find;
                                n.editor.preedit.clear();
                                self.dragging = false;
                            }
                            Ok(())
                        }
                        Action::FindNext(backwards) => {
                            n.find.navigate(&mut n.editor, backwards);
                            Ok(())
                        }
                        Action::FindClose => {
                            n.find.enabled = false;
                            n.find.query.preedit.clear();
                            self.focus = Focus::Editor;
                            self.dragging = false;
                            Ok(())
                        }
                        Action::Search => {
                            self.focus = Focus::Search;
                            Ok(())
                        }
                        Action::RefreshNotebook => {
                            n.refresh_notebook();
                            Ok(())
                        }
                        Action::ReloadExternal => n.reload_external().map(|changed| {
                            if changed {
                                self.focus = Focus::Editor;
                                self.dragging = false;
                            }
                        }),
                        Action::Edit => {
                            self.focus = Focus::Editor;
                            Ok(())
                        }
                        Action::Note(path, _) => {
                            if n.document.as_ref().and_then(|d| d.file.path()).as_ref()
                                == Some(&path)
                            {
                                if self.focus == Focus::Results {
                                    self.focus = Focus::Editor;
                                }
                                Ok(())
                            } else {
                                n.open_result(path).map(|()| {
                                    self.focus = Focus::Editor;
                                })
                            }
                        }
                        _ => Ok(()),
                    },
                    Document::Image(v) => {
                        match action {
                            Action::Previous => v.navigate(-1),
                            Action::Next => v.navigate(1),
                            Action::ZoomIn => v.zoom_at(1.25, body.w, body.h, (0., 0.)),
                            Action::ZoomOut => v.zoom_at(0.8, body.w, body.h, (0., 0.)),
                            Action::Actual => {
                                v.fit = false;
                                v.zoom = 1.;
                                v.pan = (0., 0.);
                            }
                            Action::Fit => {
                                v.fit = true;
                                v.pan = (0., 0.);
                            }
                            Action::Rotate => v.rotate_clockwise(body.w, body.h),
                            _ => (),
                        }
                        Ok(())
                    }
                };
                if let Err(e) = result {
                    self.error(e);
                }
            }
        }
    }
    pub fn close(&mut self) {
        self.action(Action::Close);
    }
    pub fn press(&mut self, mods: ModifiersState) -> bool {
        let (x, y) = self.pointer;
        if let Some(action) = self
            .hits
            .iter()
            .rev()
            .find(|h| h.rect.contains(x, y))
            .map(|h| h.action.clone())
        {
            let edit = matches!(action, Action::Edit | Action::Search | Action::FindField);
            self.action(action);
            if edit {
                self.editor_click(x, y, mods.shift_key());
                self.dragging = true;
            }
            return false;
        }
        if self.dialog.is_some() {
            if let Some(Dialog::Picker(p)) = &mut self.dialog {
                p.path.click(x, y, mods.shift_key());
            }
            return false;
        }
        if self.body().contains(x, y) && matches!(self.document, Document::Image(_)) {
            self.dragging = true;
            return false;
        }
        y < ui::header()
    }
    fn editor_click(&mut self, x: f64, y: f64, extend: bool) {
        if self.focus == Focus::Preview {
            return;
        }
        if let Document::Notes(n) = &mut self.document {
            if self.focus == Focus::Find {
                n.find.query.click(x, y, extend);
            } else if self.focus == Focus::Search {
                n.search.click(x, y, extend);
            } else {
                n.editor.click(x, y, extend);
            }
        }
    }
    pub fn release(&mut self) {
        self.dragging = false;
    }
    pub fn motion(&mut self, x: f64, y: f64) {
        let previous = self.pointer;
        self.pointer = (x, y);
        if self.dragging {
            match &mut self.document {
                Document::Notes(_) => self.editor_click(x, y, true),
                Document::Image(v) => {
                    v.pan.0 += x - previous.0;
                    v.pan.1 += y - previous.1;
                }
            }
        }
    }
    pub fn scroll(&mut self, (dx, dy): (f64, f64), mods: ModifiersState) {
        if let Some(Dialog::Picker(p)) = &mut self.dialog {
            if dy != 0. {
                p.offset = (p.offset as isize + if dy > 0. { -3 } else { 3 })
                    .clamp(0, p.entries.len().saturating_sub(1) as isize)
                    as usize;
            }
            return;
        }
        if self.dialog.is_some() {
            return;
        }
        let body = self.body();
        let sidebar = self.sidebar();
        match &mut self.document {
            Document::Notes(n) => {
                if !dy.is_finite() || dy == 0. || !body.contains(self.pointer.0, self.pointer.1) {
                    return;
                }
                if self.pointer.0 < sidebar {
                    n.list_scroll = (n.list_scroll - dy * 36.)
                        .clamp(0., (n.rows.len() as f64 * 48. - body.h + 60.).max(0.));
                } else if n.preview.enabled && self.pointer.0 >= sidebar + (body.w - sidebar) / 2. {
                    n.preview.scroll_by(-dy * 36.);
                } else {
                    n.editor.scroll = (n.editor.scroll - dy * 36.).max(0.);
                }
            }
            Document::Image(v) => {
                if mods.control_key() {
                    if dy == 0. {
                        return;
                    }
                    let (x, y) = self.pointer;
                    let factor = if dy > 0. { 1.25 } else { 0.8 };
                    let anchor = if body.contains(x, y) {
                        (x - (body.x + body.w / 2.), y - (body.y + body.h / 2.))
                    } else {
                        (0., 0.)
                    };
                    v.zoom_at(factor, body.w, body.h, anchor);
                } else {
                    v.pan.0 += dx * 36.;
                    v.pan.1 += dy * 36.;
                }
            }
        }
    }
    fn result_index(&self) -> Option<usize> {
        let Document::Notes(notes) = &self.document else {
            return None;
        };
        notes.current_rows()?;
        let path = self.result_path.as_ref()?;
        notes.rows.iter().position(|(row, _)| row == path)
    }

    fn reveal_result(&mut self, index: Option<usize>) {
        self.result_index = index;
        let height = (self.body().h - 60.).max(1.);
        if let Document::Notes(notes) = &mut self.document {
            if let Some(index) = index {
                let top = index as f64 * 48.;
                notes.list_scroll = notes
                    .list_scroll
                    .clamp((top + 44. - height).max(0.).min(top), top);
            } else {
                self.result_path = None;
            }
        }
    }

    fn refresh_key(&mut self, key: &Key, mods: ModifiersState, repeat: bool) -> bool {
        if *key != Key::Named(NamedKey::F5) || !mods.is_empty() || self.dialog.is_some() {
            return false;
        }
        let Document::Notes(notes) = &self.document else {
            return false;
        };
        if !repeat {
            self.action(if notes.managed {
                Action::RefreshNotebook
            } else {
                Action::ReloadExternal
            });
        }
        true
    }

    fn notebook_results_key(&mut self, key: &Key, mods: ModifiersState, repeat: bool) -> bool {
        // A held result Enter must not insert newlines into the opened document
        // or dismiss a modal save/load error on subsequent repeat events.
        if *key == Key::Named(NamedKey::Enter) {
            if repeat && self.result_enter {
                return true;
            }
            if !repeat {
                self.result_enter = false;
            }
        }
        if self.dialog.is_some()
            || !mods.is_empty()
            || !matches!(self.focus, Focus::Search | Focus::Results)
        {
            return false;
        }
        let Document::Notes(notes) = &self.document else {
            return false;
        };
        if !notes.managed {
            return false;
        }

        match key {
            Key::Named(NamedKey::ArrowDown | NamedKey::ArrowUp) => {
                if notes.current_rows().is_none()
                    || !notes.search.preedit.is_empty()
                    || notes.rows.is_empty()
                {
                    return true;
                }
                let last = notes.rows.len() - 1;
                let up = *key == Key::Named(NamedKey::ArrowUp);
                let current = if self.focus == Focus::Results {
                    self.result_index()
                } else {
                    None
                };
                let index = match (current, up) {
                    (Some(index), true) => index.saturating_sub(1),
                    (Some(index), false) => (index + 1).min(last),
                    (None, true) => last,
                    (None, false) => 0,
                };
                self.result_path = Some(notes.rows[index].0.clone());
                self.focus = Focus::Results;
                self.dragging = false;
                self.reveal_result(Some(index));
                true
            }
            Key::Named(NamedKey::Enter) if self.focus == Focus::Results => {
                if !repeat {
                    self.result_enter = true;
                    // A redraw need not have happened since the list was changed.
                    if let (Some(index), Some(generation)) =
                        (self.result_index(), notes.current_rows())
                    {
                        let top = index as f64 * 48. - notes.list_scroll;
                        if top + 44. > 0. && top < self.body().h - 60. {
                            self.action(Action::Note(notes.rows[index].0.clone(), generation));
                        } else {
                            // The user may have scrolled the highlight away.
                            // Reveal it without opening an invisible note.
                            self.reveal_result(Some(index));
                        }
                    } else {
                        self.reveal_result(None);
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn find_selection_key(&mut self, key: &Key, mods: ModifiersState, repeat: bool) -> bool {
        if self.dialog.is_some()
            || self.focus != Focus::Editor
            || mods != ModifiersState::CONTROL
            || !matches!(key, Key::Character(key) if key.eq_ignore_ascii_case("e"))
            || !matches!(&self.document, Document::Notes(n) if !n.managed)
        {
            return false;
        }
        if !repeat {
            self.action(Action::FindSelection);
        }
        true
    }

    fn preview_focus_key(&mut self, key: &Key, mods: ModifiersState, repeat: bool) -> bool {
        if *key != Key::Named(NamedKey::F6)
            || !mods.is_empty()
            || self.dialog.is_some()
            || !matches!(self.focus, Focus::Editor | Focus::Preview)
            || !matches!(&self.document, Document::Notes(n) if n.preview.enabled)
        {
            return false;
        }
        if !repeat {
            self.dragging = false;
            self.action(if self.focus == Focus::Preview {
                Action::Edit
            } else {
                Action::Preview
            });
        }
        true
    }

    fn preview_scroll_key(&mut self, key: &Key, mods: ModifiersState) {
        if !mods.is_empty() || self.focus != Focus::Preview {
            return;
        }
        let Key::Named(key) = key else {
            return;
        };
        if let Document::Notes(n) = &mut self.document
            && n.preview.enabled
        {
            n.preview.scroll_key(*key);
        }
    }

    pub fn key(
        &mut self,
        event: &KeyEvent,
        mods: ModifiersState,
        clipboard: &mut Option<arboard::Clipboard>,
    ) {
        self.caret = true;
        self.next_blink = Instant::now() + Duration::from_millis(500);
        if self.refresh_key(&event.logical_key, mods, event.repeat)
            || self.notebook_results_key(&event.logical_key, mods, event.repeat)
            || self.find_selection_key(&event.logical_key, mods, event.repeat)
            || self.preview_focus_key(&event.logical_key, mods, event.repeat)
        {
            return;
        }
        if self.dialog.is_some() {
            if event.logical_key == Key::Named(NamedKey::Tab) {
                let count = self.dialog_actions().len();
                self.dialog_focus = match self.dialog_focus {
                    None => Some(if mods.shift_key() { count - 1 } else { 0 }),
                    Some(i) if mods.shift_key() => i.checked_sub(1),
                    Some(i) => (i + 1 < count).then_some(i + 1),
                };
                return;
            }
            if event.logical_key == Key::Named(NamedKey::Escape) {
                self.dialog = None;
                return;
            }
            if event.logical_key == Key::Named(NamedKey::Enter) {
                if let Some(action) = self
                    .dialog_focus
                    .and_then(|i| self.dialog_actions().get(i).cloned())
                {
                    self.action(action);
                    return;
                }
                match self.dialog {
                    Some(Dialog::Picker(_)) => self.action(Action::Accept),
                    Some(Dialog::Error(_)) => self.dialog = None,
                    _ => (),
                }
                return;
            }
            if self.dialog_focus.is_some() {
                return;
            }
            if let Some(Dialog::Picker(p)) = &mut self.dialog {
                let result = p.path.key(event, mods, clipboard, false);
                if let Err(e) = result {
                    self.error(e);
                }
            }
            return;
        }
        let character = if let Key::Character(key) = &event.logical_key {
            key.to_lowercase()
        } else {
            String::new()
        };
        let action = if mods.control_key() {
            match character.as_str() {
                "p" if mods.shift_key() && matches!(self.document, Document::Notes(_)) => {
                    Some(Action::TogglePreview)
                }
                "w" => Some(Action::Close),
                "o" => Some(Action::Open),
                "n" if matches!(self.document, Document::Notes(_)) => Some(Action::New),
                "s" if matches!(self.document, Document::Notes(_)) => Some(if mods.shift_key() {
                    Action::SaveAs(false)
                } else {
                    Action::Save
                }),
                "f" if matches!(self.document, Document::Notes(_)) => Some(
                    if matches!(&self.document,Document::Notes(n) if n.managed) {
                        Action::Search
                    } else {
                        Action::Find
                    },
                ),
                _ => None,
            }
        } else {
            None
        };
        if let Some(action) = action {
            self.action(action);
            return;
        }
        if matches!(&self.document, Document::Notes(n) if n.find.enabled) {
            if event.logical_key == Key::Named(NamedKey::Escape) {
                self.action(Action::FindClose);
                return;
            }
            if event.logical_key == Key::Named(NamedKey::F3)
                || (self.focus == Focus::Find && event.logical_key == Key::Named(NamedKey::Enter))
            {
                self.action(Action::FindNext(mods.shift_key()));
                return;
            }
        }
        if let Document::Notes(n) = &mut self.document {
            if event.logical_key == Key::Named(NamedKey::Escape) {
                self.focus = Focus::Editor;
                return;
            }
            if self.focus == Focus::Preview {
                self.preview_scroll_key(&event.logical_key, mods);
                return;
            }
            if self.focus == Focus::Results {
                return;
            }
            let find = self.focus == Focus::Find;
            let search = self.focus == Focus::Search;
            let editor = if find {
                &mut n.find.query
            } else if search {
                &mut n.search
            } else {
                &mut n.editor
            };
            let revision = editor.revision;
            let result = editor.key(event, mods, clipboard, !search && !find);
            if editor.revision != revision {
                if find {
                    n.find.sync(&mut n.editor);
                } else if search {
                    n.update_query();
                    n.list_scroll = 0.;
                } else {
                    n.edited();
                }
            }
            if let Err(e) = result {
                self.error(e);
            }
            return;
        }
        let action = match &event.logical_key {
            Key::Named(NamedKey::ArrowLeft | NamedKey::PageUp) => Some(Action::Previous),
            Key::Named(NamedKey::ArrowRight | NamedKey::PageDown | NamedKey::Space) => {
                Some(Action::Next)
            }
            Key::Named(NamedKey::F11) => Some(Action::Fullscreen),
            Key::Named(NamedKey::Escape) => {
                self.effects.push(Effect::LeaveFullscreen);
                None
            }
            _ => match character.as_str() {
                "+" | "=" => Some(Action::ZoomIn),
                "-" => Some(Action::ZoomOut),
                "1" => Some(Action::Actual),
                "f" | "0" => Some(Action::Fit),
                "r" => Some(Action::Rotate),
                _ => None,
            },
        };
        if let Some(action) = action {
            self.action(action);
        }
    }
    fn active_editor(&mut self) -> Option<&mut Editor> {
        if self.dialog.is_some() && self.dialog_focus.is_some() {
            return None;
        }
        match &mut self.dialog {
            Some(Dialog::Picker(p)) => return Some(&mut p.path),
            Some(_) => return None,
            None => (),
        }
        match &mut self.document {
            Document::Notes(_) if matches!(self.focus, Focus::Preview | Focus::Results) => None,
            Document::Notes(n) => Some(if self.focus == Focus::Find {
                &mut n.find.query
            } else if self.focus == Focus::Search {
                &mut n.search
            } else {
                &mut n.editor
            }),
            _ => None,
        }
    }
    pub fn ime(&mut self, text: String, commit: bool) {
        let text = if self.focus == Focus::Find && self.dialog.is_none() {
            text.replace(['\r', '\n'], "")
        } else {
            text
        };
        let changed = if let Some(editor) = self.active_editor() {
            let revision = editor.revision;
            if commit {
                editor.preedit.clear();
                editor.insert(&text);
            } else {
                editor.preedit = text;
            }
            editor.revision != revision
        } else {
            false
        };
        if commit
            && !matches!(self.focus, Focus::Preview | Focus::Results)
            && self.dialog.is_none()
            && let Document::Notes(n) = &mut self.document
        {
            if self.focus == Focus::Find {
                n.find.sync(&mut n.editor);
            } else if self.focus == Focus::Search {
                if changed {
                    n.update_query();
                    n.list_scroll = 0.;
                }
            } else {
                n.edited();
            }
        }
    }
    pub fn ime_position(&mut self) -> (f64, f64) {
        self.active_editor()
            .map(|e| e.caret_position())
            .unwrap_or((0., 0.))
    }
    pub fn ime_target(&self) -> u8 {
        if self.dialog_focus.is_some() && self.dialog.is_some() {
            return 0;
        }
        match &self.dialog {
            Some(Dialog::Picker(_)) => 3,
            Some(_) => 0,
            None => match &self.document {
                Document::Notes(_) => {
                    if matches!(self.focus, Focus::Preview | Focus::Results) {
                        0
                    } else if self.focus == Focus::Find {
                        4
                    } else if self.focus == Focus::Editor {
                        1
                    } else {
                        2
                    }
                }
                _ => 0,
            },
        }
    }
    pub fn clear_preedit(&mut self) {
        if let Document::Notes(n) = &mut self.document {
            n.editor.preedit.clear();
            n.search.preedit.clear();
            n.find.query.preedit.clear();
        }
        if let Some(Dialog::Picker(p)) = &mut self.dialog {
            p.path.preedit.clear();
        }
    }
    pub fn dropped(&mut self, path: PathBuf) {
        if self.dialog.is_some() {
            return;
        }
        match &mut self.document {
            Document::Notes(_) => self.effects.push(Effect::OpenNote(path)),
            Document::Image(v) => v.open(vec![path]),
        }
    }
}
impl Picker {
    fn resolve(&self) -> PathBuf {
        let path = self.path.text.trim();
        if path == "~" {
            gio::glib::home_dir()
        } else if let Some(path) = path.strip_prefix("~/") {
            gio::glib::home_dir().join(path)
        } else {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| gio::glib::home_dir())
                    .join(path)
            }
        }
    }
    fn refresh(&mut self) {
        let path = self.resolve();
        let directory = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(Path::new("/")).into()
        };
        self.entries = std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir() || !matches!(self.mode, PickerMode::Images) || crate::image_path(p)
            })
            .collect();
        self.entries
            .sort_by(|a, b| (!a.is_dir(), a.file_name()).cmp(&(!b.is_dir(), b.file_name())));
        self.offset = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_zoom_steps_preserve_the_center_detail_after_panning() {
        let mut app = App::new(false, Vec::new()).unwrap();
        let Document::Image(viewer) = &mut app.document else {
            unreachable!()
        };
        viewer.image = Some(cairo::ImageSurface::create(cairo::Format::ARgb32, 800, 600).unwrap());
        viewer.fit = false;
        viewer.zoom = 2.;
        viewer.pan = (120., -60.);

        app.action(Action::ZoomIn);
        let Document::Image(viewer) = &app.document else {
            unreachable!()
        };
        assert_eq!(viewer.zoom, 2.5);
        assert_eq!(viewer.pan, (150., -75.));

        app.action(Action::ZoomOut);
        let Document::Image(viewer) = &app.document else {
            unreachable!()
        };
        assert_eq!(viewer.zoom, 2.);
        assert_eq!(viewer.pan, (120., -60.));
    }

    #[test]
    fn image_pan_uses_both_axes_but_picker_and_zoom_ignore_horizontal_scroll() {
        let mut app = App::new(false, Vec::new()).unwrap();
        let Document::Image(viewer) = &mut app.document else {
            unreachable!()
        };
        viewer.image = Some(cairo::ImageSurface::create(cairo::Format::ARgb32, 800, 600).unwrap());
        viewer.fit = false;
        viewer.zoom = 2.;
        app.scroll((2., -1.), ModifiersState::empty());
        app.scroll((-1., 0.), ModifiersState::empty());
        app.scroll((3., 0.), ModifiersState::CONTROL);
        let Document::Image(viewer) = &app.document else {
            unreachable!()
        };
        assert_eq!(viewer.pan, (36., -36.));
        assert_eq!(viewer.zoom, 2.);

        app.dialog = Some(Dialog::Picker(Box::new(Picker {
            mode: PickerMode::Images,
            path: Editor::new(String::new()),
            entries: vec![PathBuf::new(); 10],
            offset: 2,
        })));
        app.scroll((-3., 0.), ModifiersState::empty());
        assert!(matches!(&app.dialog, Some(Dialog::Picker(p)) if p.offset == 2));
        app.scroll((-3., -1.), ModifiersState::empty());
        assert!(matches!(&app.dialog, Some(Dialog::Picker(p)) if p.offset == 5));
    }

    #[test]
    fn notes_wheel_scroll_stays_inside_content_and_reverses_at_preview_edge() {
        let dir = std::env::temp_dir().join(format!("jwm-notes-wheel-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("preview.md");
        std::fs::write(&path, "# Heading\n\nA paragraph.\n\n".repeat(100)).unwrap();
        let mut app = App::new(true, vec![path]).unwrap();
        app.action(Action::TogglePreview);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 900, 410).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 900., 410., false);

        let offsets = |app: &App| match &app.document {
            Document::Notes(n) => (n.editor.scroll, n.preview.scroll),
            _ => unreachable!(),
        };
        let plain = ModifiersState::empty();
        app.pointer = (200., 2.);
        app.scroll((0., -2.), plain);
        app.pointer = (700., 2.);
        app.scroll((0., -2.), plain);
        assert_eq!(offsets(&app), (0., 0.), "chrome cannot scroll either pane");

        app.pointer = (700., 180.);
        app.scroll((0., -10000.), plain);
        let (editor, bottom) = offsets(&app);
        assert_eq!(editor, 0.);
        assert!(bottom > 36. && bottom < 360000.);
        app.scroll((0., 0.5), plain);
        assert_eq!(offsets(&app), (0., bottom - 18.));
        app.scroll((3., 0.), plain);
        assert_eq!(offsets(&app), (0., bottom - 18.));

        app.pointer = (200., 180.);
        app.scroll((0., -1.), plain);
        assert_eq!(offsets(&app), (36., bottom - 18.));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preview_focus_shortcut_respects_visibility_modifiers_and_repeat() {
        let dir = std::env::temp_dir().join(format!("jwm-preview-focus-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("preview.md");
        std::fs::write(&path, "source text").unwrap();
        let mut app = App::new(true, vec![path]).unwrap();
        let f6 = Key::Named(NamedKey::F6);
        let plain = ModifiersState::empty();
        assert!(!app.preview_focus_key(&f6, plain, false));

        app.action(Action::TogglePreview);
        assert!(!app.preview_focus_key(&f6, ModifiersState::SHIFT, false));
        let Document::Notes(n) = &mut app.document else {
            unreachable!()
        };
        n.editor.select(0..6);
        app.ime("pending".into(), false);
        assert!(app.preview_focus_key(&f6, plain, false));
        assert_eq!(app.ime_target(), 0);
        assert!(app.preview_focus_key(&f6, plain, true));
        assert_eq!(app.ime_target(), 0);
        app.ime("ignored".into(), true);
        assert!(app.preview_focus_key(&f6, plain, false));
        assert_eq!(app.ime_target(), 1);
        let Document::Notes(n) = &app.document else {
            unreachable!()
        };
        assert_eq!(n.editor.text, "source text");
        assert!(n.editor.preedit.is_empty());

        app.ime("new".into(), true);
        let Document::Notes(n) = &app.document else {
            unreachable!()
        };
        assert_eq!(n.editor.text, "new text", "source selection was preserved");
        app.action(Action::Find);
        assert!(!app.preview_focus_key(&f6, plain, false));
        assert_eq!(app.ime_target(), 4);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn focused_markdown_preview_scrolls_by_key_without_editing_source() {
        let dir = std::env::temp_dir().join(format!("jwm-preview-keys-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("preview.md");
        let source = "# Heading\n\nThis is a Markdown paragraph.\n\n".repeat(100);
        std::fs::write(&path, &source).unwrap();

        let mut app = App::new(true, vec![path]).unwrap();
        app.action(Action::TogglePreview);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 900, 410).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 900., 410., false);
        let scroll = |app: &App| match &app.document {
            Document::Notes(n) => n.preview.scroll,
            _ => unreachable!(),
        };
        let key =
            |app: &mut App, k| app.preview_scroll_key(&Key::Named(k), ModifiersState::empty());
        key(&mut app, NamedKey::PageDown);
        assert_eq!(scroll(&app), 0., "editor focus must not scroll the preview");
        app.action(Action::Preview);
        assert_eq!(app.ime_target(), 0);
        key(&mut app, NamedKey::ArrowDown);
        assert_eq!(scroll(&app), 36.);
        key(&mut app, NamedKey::PageDown);
        let page = scroll(&app);
        assert!(page > 72.);
        key(&mut app, NamedKey::End);
        let end = scroll(&app);
        assert!(end > page);
        key(&mut app, NamedKey::PageDown);
        assert_eq!(scroll(&app), end, "scroll must clamp before the next paint");
        key(&mut app, NamedKey::PageUp);
        assert!(scroll(&app) < end);
        key(&mut app, NamedKey::Home);
        key(&mut app, NamedKey::ArrowUp);
        assert_eq!(scroll(&app), 0.);
        let Document::Notes(n) = &app.document else {
            unreachable!()
        };
        assert_eq!(n.editor.text, source);
        assert_eq!(n.editor.revision, 0);
        assert!(!n.dirty);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn external_reload_preserves_dirty_input_and_reuses_current_disk_format() {
        let dir = std::env::temp_dir().join(format!("jwm-external-reload-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("external.md");
        let replacement = dir.join("replacement");
        std::fs::write(&path, "# Old\none\n").unwrap();
        let mut app = App::new(true, vec![path.clone()]).unwrap();
        let f5 = Key::Named(NamedKey::F5);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 540, 360).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 540., 360., false);
        let hit = app
            .hits
            .iter()
            .find(|hit| hit.action == Action::ReloadExternal)
            .unwrap();
        let click = (hit.rect.x + 8., hit.rect.y + 8.);
        assert!(!app.hits.iter().any(|h| h.action == Action::RefreshNotebook));
        #[cfg(feature = "accessibility")]
        {
            let mut semantics = accessibility::Semantics::default();
            let tree = semantics.update(&app, 1.);
            assert!(tree.nodes.iter().any(|(_, node)| {
                node.label() == Some("从磁盘重新载入 (F5)")
                    && node.supports_action(accesskit::Action::Click)
            }));
        }
        app.action(Action::ToggleCodeView);
        app.action(Action::TogglePreview);
        let Document::Notes(n) = &mut app.document else {
            unreachable!()
        };
        n.editor.select(2..5);
        app.action(Action::Find);
        let Document::Notes(n) = &mut app.document else {
            unreachable!()
        };
        n.find.query.text = "Old".into();
        n.find.query.preedit = "待提交".into();
        std::fs::write(&replacement, b"\xef\xbb\xbf# New\r\nupdated\r\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(app.refresh_key(&f5, ModifiersState::empty(), false));
        let Document::Notes(n) = &mut app.document else {
            unreachable!()
        };
        assert_eq!(n.editor.text, "# Old\none\n");
        assert_eq!(n.find.query.preedit, "待提交");
        n.find.query.preedit.clear();
        assert!(app.refresh_key(&f5, ModifiersState::empty(), true));
        app.pointer = click;
        app.press(ModifiersState::empty());
        let Document::Notes(n) = &mut app.document else {
            unreachable!()
        };
        assert_eq!(n.editor.text, "# New\nupdated\n");
        assert!(n.editor.monospace && n.preview.enabled);
        assert!(!n.dirty && !n.find.enabled);
        assert_eq!(n.editor.selection(), 5..5);
        assert_eq!(n.editor.position(), (1, 6));
        assert!(app.focus == Focus::Editor);

        // Same decoded text with a new inode preserves the current selection,
        // yet refreshes the etag so the next edit can be saved safely.
        n.editor.select(2..5);
        std::fs::write(&replacement, b"\xef\xbb\xbf# New\r\nupdated\r\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(app.refresh_key(&f5, ModifiersState::empty(), false));
        let Document::Notes(n) = &mut app.document else {
            unreachable!()
        };
        assert_eq!(n.editor.selection(), 2..5);
        n.editor.insert("Local");
        n.edited();
        assert!(app.refresh_key(&f5, ModifiersState::empty(), false));
        assert!(matches!(app.dialog, Some(Dialog::Error(_))));
        app.action(Action::Cancel);
        let Document::Notes(n) = &mut app.document else {
            unreachable!()
        };
        assert!(n.dirty);
        assert_eq!(n.editor.text, "# Local\nupdated\n");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xef\xbb\xbf# New\r\nupdated\r\n"
        );
        n.save().unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xef\xbb\xbf# Local\r\nupdated\r\n"
        );

        std::fs::write(&path, [0xff]).unwrap();
        assert!(app.refresh_key(&f5, ModifiersState::empty(), false));
        assert!(matches!(app.dialog, Some(Dialog::Error(_))));
        let Document::Notes(n) = &app.document else {
            unreachable!()
        };
        assert!(!n.dirty);
        assert_eq!(n.editor.text, "# Local\nupdated\n");
        assert_eq!(std::fs::read(&path).unwrap(), [0xff]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refresh_existing_notebook_filter_observes_external_writes_without_saving_editor() {
        let dir = std::env::temp_dir().join(format!("jwm-external-refresh-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let current = dir.join("current.txt");
        let rename = dir.join("old-result.txt");
        let inplace = dir.join("hidden.txt");
        let new = dir.join("new.txt");
        std::fs::write(&current, "Needle current").unwrap();
        std::fs::write(&rename, "Needle rename").unwrap();
        std::fs::write(&inplace, "previously hidden").unwrap();
        let mut app = App::new(true, vec![current.clone()]).unwrap();
        let (send, ready) = std::sync::mpsc::channel();
        let due = if let Document::Notes(notes) = &mut app.document {
            notes.managed = true;
            notes.directory = dir.clone();
            notes.search.text = "needle".into();
            notes.refresh();
            assert_eq!(notes.rows.len(), 2);
            notes.editor = Editor::new("unsaved private buffer".into());
            notes.edited();
            notes.due
        } else {
            panic!("expected native Notes");
        };
        app.set_search_wake(std::sync::Arc::new(move || {
            let _ = send.send(());
        }));
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 960, 680).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 960., 680., false);
        assert!(app.hits.iter().any(|h| h.action == Action::RefreshNotebook));
        #[cfg(feature = "accessibility")]
        let (semantics, refresh) = {
            let mut semantics = accessibility::Semantics::default();
            let tree = semantics.update(&app, 1.);
            let refresh = tree
                .nodes
                .iter()
                .find_map(|(id, node)| {
                    (node.label() == Some("刷新列表 F5")
                        && node.supports_action(accesskit::Action::Click))
                    .then_some(*id)
                })
                .unwrap();
            (semantics, refresh)
        };
        std::fs::write(&inplace, "Needle edited in place").unwrap();
        let temp = dir.join("vim-write.tmp");
        std::fs::write(&temp, "Needle atomically added").unwrap();
        std::fs::rename(&temp, &new).unwrap();
        let renamed = dir.join("renamed.txt");
        std::fs::rename(&rename, &renamed).unwrap();

        assert!(app.refresh_key(&Key::Named(NamedKey::F5), ModifiersState::empty(), false));
        assert!(app.refresh_key(&Key::Named(NamedKey::F5), ModifiersState::empty(), true));
        assert!(app.focus == Focus::Editor);
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(app.poll_search());
        let Document::Notes(notes) = &app.document else {
            panic!("expected notebook");
        };
        assert_eq!(notes.search.text, "needle");
        assert_eq!(notes.rows.len(), 4);
        assert!(
            notes
                .rows
                .iter()
                .any(|(p, title)| p == &new && title == "Needle atomically added")
        );
        assert!(
            notes
                .rows
                .iter()
                .any(|(p, title)| p == &inplace && title == "Needle edited in place")
        );
        assert!(
            notes
                .rows
                .iter()
                .any(|(p, title)| p == &renamed && title == "Needle rename")
        );
        assert!(!notes.rows.iter().any(|(p, _)| p == &rename));
        assert_eq!(notes.editor.text, "unsaved private buffer");
        assert!(notes.dirty);
        assert_eq!(notes.due, due);
        assert_eq!(std::fs::read_to_string(current).unwrap(), "Needle current");
        #[cfg(feature = "accessibility")]
        {
            assert!(semantics.action(
                &mut app,
                accesskit::ActionRequest {
                    action: accesskit::Action::Click,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: refresh,
                    data: None,
                }
            ));
            ready.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(app.poll_search());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn autosave_tick_keeps_existing_sidebar_pixels_until_new_results_arrive() {
        let dir = std::env::temp_dir().join(format!("jwm-autosave-frame-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("current.txt");
        std::fs::write(&path, "Current title").unwrap();
        let mut app = App::new(true, vec![path.clone()]).unwrap();
        if let Document::Notes(notes) = &mut app.document {
            notes.managed = true;
            notes.directory = dir.clone();
            notes.refresh();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        app.set_search_wake(std::sync::Arc::new(move || {
            let _ = tx.send(());
        }));
        let capture = |app: &mut App| {
            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 960, 680).unwrap();
            {
                let cr = cairo::Context::new(&surface).unwrap();
                app.draw(&cr, 960., 680., false);
            }
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            // Exclude the title and search controls; examine the painted rows.
            (120..300)
                .flat_map(|y| data[y * stride..y * stride + 180 * 4].iter().copied())
                .collect::<Vec<_>>()
        };
        let before = capture(&mut app);
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.action, Action::Note(..)))
        );
        if let Document::Notes(notes) = &mut app.document {
            notes.editor = crate::editor::Editor::new("Renamed title".into());
            notes.edited();
            notes.due = Some(Instant::now());
        }
        assert!(app.tick());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Renamed title");
        assert_eq!(capture(&mut app), before);
        assert!(
            !app.hits
                .iter()
                .any(|hit| matches!(hit.action, Action::Note(..)))
        );
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(app.poll_search());
        assert_ne!(capture(&mut app), before);
        assert!(app.hits.iter().any(|hit| hit.label == "Renamed title"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn asynchronous_query_coalesces_and_rejects_old_hits_before_a_redraw() {
        let directory =
            std::env::temp_dir().join(format!("jwm-async-search-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let original = directory.join("current.txt");
        let result = directory.join("result.txt");
        std::fs::write(&original, "original").unwrap();
        std::fs::write(&result, "other note\n你好 ΑΘΉΝΑ").unwrap();
        let mut app = App::new(true, vec![original.clone()]).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        if let Document::Notes(notes) = &mut app.document {
            notes.managed = true;
            notes.directory = directory.clone();
            notes.refresh();
            notes.editor.insert("unsaved ");
            notes.edited();
        }
        app.set_search_wake(std::sync::Arc::new(move || {
            let _ = tx.send(());
        }));
        app.action(Action::Search);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 960, 680).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 960., 680., false);
        let old_rect = app
            .hits
            .iter()
            .find_map(|h| {
                matches!(&h.action, Action::Note(path, _) if path == &result).then_some(h.rect)
            })
            .unwrap();
        #[cfg(feature = "accessibility")]
        let (mut semantics, old_button) = {
            use accesskit::{Action, Role};
            let mut semantics = accessibility::Semantics::default();
            let tree = semantics.update(&app, 1.);
            let old_button = tree
                .nodes
                .iter()
                .find_map(|(id, node)| {
                    (node.role() == Role::Button
                        && node.label() == Some("other note")
                        && node.supports_action(Action::Click))
                    .then_some(*id)
                })
                .unwrap();
            (semantics, old_button)
        };
        #[cfg(feature = "accessibility")]
        let click = |target_node| accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target_tree: accesskit::TreeId::ROOT,
            target_node,
            data: None,
        };

        app.ime("你好".into(), false);
        assert!(matches!(&app.document, Document::Notes(n) if !n.search_pending()));
        app.ime("missing ".into(), true);
        if let Document::Notes(notes) = &mut app.document {
            // Both changes precede the next event-loop tick. Even if the first
            // worker has finished, it must not publish its now superseded rows.
            notes.search.select_all();
        }
        app.ime("αθήνα".into(), true);
        app.pointer = (old_rect.x + 5., old_rect.y + 5.);
        assert!(!app.press(ModifiersState::empty()));
        assert!(app.notebook_results_key(
            &Key::Named(NamedKey::ArrowDown),
            ModifiersState::empty(),
            false
        ));
        assert!(app.focus == Focus::Search);
        assert!(app.result_path.is_none());
        #[cfg(feature = "accessibility")]
        assert!(!semantics.action(&mut app, click(old_button)));
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "original");

        let mut ready = false;
        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(3)).unwrap();
            if app.poll_search() {
                ready = true;
                break;
            }
        }
        assert!(ready);
        assert!(
            matches!(&app.document, Document::Notes(n) if n.rows.len() == 1 && n.rows[0].0 == result)
        );
        // The old coordinate and old accessibility ID still must not save
        // anything or open a note, even though the same path matches again.
        assert!(!app.press(ModifiersState::empty()));
        #[cfg(feature = "accessibility")]
        assert!(!semantics.action(&mut app, click(old_button)));
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "original");
        app.draw(&cr, 960., 680., false);
        #[cfg(feature = "accessibility")]
        {
            semantics.update(&app, 1.);
            assert!(!semantics.action(&mut app, click(old_button)));
        }
        assert!(app.notebook_results_key(
            &Key::Named(NamedKey::ArrowDown),
            ModifiersState::empty(),
            false
        ));
        assert!(app.notebook_results_key(
            &Key::Named(NamedKey::Enter),
            ModifiersState::empty(),
            false
        ));
        assert!(app.focus == Focus::Editor);
        assert!(
            matches!(&app.document, Document::Notes(n) if n.document.as_ref().unwrap().file.path() == Some(result))
        );
        assert_eq!(
            std::fs::read_to_string(&original).unwrap(),
            "unsaved original"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn notebook_keyboard_open_preserves_identity_and_guards_save() {
        let directory =
            std::env::temp_dir().join(format!("jwm-result-keys-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let original = directory.join("current.txt");
        std::fs::write(&original, "original").unwrap();
        for i in 0..24 {
            std::fs::write(directory.join(format!("{i:02}.txt")), "same result title").unwrap();
        }
        let mut app = App::new(true, vec![original.clone()]).unwrap();
        let Document::Notes(n) = &mut app.document else {
            panic!("expected note")
        };
        n.managed = true;
        n.directory = directory.clone();
        n.refresh();
        app.action(Action::Search);
        app.ime("same result".to_owned(), true);
        app.ime("预编辑".to_owned(), false);
        let key = |app: &mut App, key, repeat| {
            app.notebook_results_key(&Key::Named(key), ModifiersState::empty(), repeat)
        };
        assert!(key(&mut app, NamedKey::ArrowDown, false));
        assert!(app.focus == Focus::Search);
        assert!(app.result_path.is_none());
        app.clear_preedit();
        assert!(key(&mut app, NamedKey::ArrowUp, false));
        assert!(app.focus == Focus::Results);
        assert_eq!(app.ime_target(), 0);
        app.ime("stale composition".to_owned(), true);
        let last = app.result_path.clone().unwrap();
        let Document::Notes(n) = &mut app.document else {
            panic!("expected note")
        };
        assert!(n.list_scroll > 0.);
        assert_eq!(n.editor.text, "original");
        assert_eq!(n.search.text, "same result");
        assert!(!n.dirty);
        n.rows.reverse();

        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 960, 680).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 960., 680., false);
        assert_eq!(app.result_path.as_ref(), Some(&last));
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(&hit.action, Action::Note(path, _) if path == &last))
        );
        #[cfg(feature = "accessibility")]
        {
            let mut semantics = accessibility::Semantics::default();
            let tree = semantics.update(&app, 1.);
            assert!(tree.nodes.iter().any(|(id, node)| {
                *id == tree.focus
                    && node.label() == Some("same result title")
                    && node.supports_action(accesskit::Action::Click)
            }));
        }
        // Manual scrolling may hide the selected row. Enter must reveal it
        // without opening (or writing) until a fresh Enter confirms it.
        app.pointer = (10., app.body().y + 80.);
        app.scroll((0., -100.), ModifiersState::empty());
        app.draw(&cr, 960., 680., false);
        assert!(
            !app.hits
                .iter()
                .any(|hit| matches!(&hit.action, Action::Note(path, _) if path == &last))
        );
        #[cfg(feature = "accessibility")]
        {
            let mut semantics = accessibility::Semantics::default();
            let tree = semantics.update(&app, 1.);
            assert!(
                tree.nodes.iter().any(|(id, node)| {
                    *id == tree.focus && node.role() == accesskit::Role::Window
                })
            );
        }
        assert!(key(&mut app, NamedKey::Enter, false));
        assert!(key(&mut app, NamedKey::Enter, true));
        assert!(app.focus == Focus::Results);
        let Document::Notes(n) = &app.document else {
            panic!("expected note")
        };
        assert_eq!(
            n.document.as_ref().unwrap().file.path().as_ref(),
            Some(&original)
        );
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "original");
        app.draw(&cr, 960., 680., false);
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(&hit.action, Action::Note(path, _) if path == &last))
        );

        assert!(key(&mut app, NamedKey::ArrowDown, true));
        let target = app.result_path.clone().unwrap();
        assert_ne!(target, last);
        let Document::Notes(n) = &mut app.document else {
            panic!("expected note")
        };
        n.rows.retain(|(path, _)| path != &target);
        assert!(key(&mut app, NamedKey::Enter, false));
        assert!(app.result_path.is_none());
        assert!(app.focus == Focus::Results);
        assert!(key(&mut app, NamedKey::Enter, true));
        assert!(key(&mut app, NamedKey::ArrowDown, false));
        assert_eq!(app.result_path.as_ref(), Some(&last));

        let Document::Notes(n) = &mut app.document else {
            panic!("expected note")
        };
        n.editor.insert("pending ");
        n.edited();
        std::fs::write(&original, "external replacement with different length").unwrap();
        assert!(key(&mut app, NamedKey::Enter, false));
        assert!(matches!(app.dialog, Some(Dialog::Error(_))));
        assert!(key(&mut app, NamedKey::Enter, true));
        assert!(matches!(app.dialog, Some(Dialog::Error(_))));
        let Document::Notes(n) = &app.document else {
            panic!("expected note")
        };
        assert!(n.dirty);
        assert_eq!(n.editor.text, "pending original");
        assert_eq!(
            n.document.as_ref().unwrap().file.path().as_ref(),
            Some(&original)
        );

        app.action(Action::Cancel);
        let Document::Notes(n) = &mut app.document else {
            panic!("expected note")
        };
        n.load(original.clone()).unwrap();
        n.editor.insert("save me ");
        n.edited();
        assert!(key(&mut app, NamedKey::Enter, false));
        assert_eq!(app.ime_target(), 1);
        assert!(app.focus == Focus::Editor);
        assert!(key(&mut app, NamedKey::Enter, true));
        let Document::Notes(n) = &app.document else {
            panic!("expected note")
        };
        assert_eq!(n.editor.text, "same result title");
        assert_eq!(
            n.document.as_ref().unwrap().file.path().as_ref(),
            Some(&last)
        );
        assert_eq!(
            std::fs::read_to_string(original).unwrap(),
            "save me external replacement with different length"
        );
        assert!(!key(&mut app, NamedKey::Enter, false));
        assert!(app.effects.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ime_search_reveals_first_result_without_resetting_preedit_or_noop() {
        let directory = std::env::temp_dir().join(format!("jwm-ime-search-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let source = "正文保持不变";
        let current = directory.join("current.txt");
        std::fs::write(&current, source).unwrap();
        for i in 0..20 {
            std::fs::write(
                directory.join(format!("{i:02}.txt")),
                format!("项目 {i:02}"),
            )
            .unwrap();
        }
        let mut app = App::new(true, vec![current.clone()]).unwrap();
        if let Document::Notes(n) = &mut app.document {
            n.managed = true;
            n.directory = directory.clone();
            n.refresh();
            n.list_scroll = 480.;
        }
        app.action(Action::Search);
        app.ime("项".into(), false);
        if let Document::Notes(n) = &app.document {
            assert_eq!(n.list_scroll, 480.);
            assert_eq!(n.search.preedit, "项");
            assert_eq!(n.search.text, "");
        }
        app.ime("项目".into(), true);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 960, 680).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 960., 680., false);
        let visible = app
            .hits
            .iter()
            .filter(|hit| matches!(hit.action, Action::Note(..)))
            .count();
        if let Document::Notes(n) = &app.document {
            let first = n.rows[0].0.clone();
            let first_visible = app
                .hits
                .iter()
                .any(|hit| matches!(&hit.action, Action::Note(path, _) if path == &first));
            eprintln!(
                "IME_SEARCH scroll={} rows={} visible={} first_visible={}",
                n.list_scroll,
                n.rows.len(),
                visible,
                first_visible
            );
            assert!(first_visible, "committed query must reveal first result");
            assert_eq!(n.list_scroll, 0.);
            assert_eq!(n.search.cursor, "项目".len());
            assert!(n.search.preedit.is_empty());
        }
        assert!(app.focus == Focus::Search);
        // Empty commit and an uninterrupted preedit must not jump a scrolled result list.
        if let Document::Notes(n) = &mut app.document {
            n.list_scroll = 240.;
        }
        app.ime(String::new(), true);
        app.ime("不存在".into(), false);
        if let Document::Notes(n) = &app.document {
            assert_eq!(n.list_scroll, 240.);
            assert_eq!(n.rows.len(), 20);
        }
        if let Document::Notes(n) = &mut app.document {
            n.search.select_all();
        }
        app.ime("不存在".into(), true);
        if let Document::Notes(n) = &app.document {
            assert!(n.rows.is_empty());
            assert_eq!(n.list_scroll, 0.);
        }
        // Committed empty replacement is a real edit, distinct from an empty no-op.
        if let Document::Notes(n) = &mut app.document {
            n.search.select_all();
        }
        app.ime(String::new(), true);
        if let Document::Notes(n) = &app.document {
            assert_eq!(n.rows.len(), 21);
            assert_eq!(n.list_scroll, 0.);
            assert_eq!(n.search.cursor, 0);
            assert_eq!(n.editor.text, source);
            assert_eq!(n.editor.revision, 0);
            assert!(!n.dirty);
        }
        assert!(app.focus == Focus::Search);
        assert!(app.effects.is_empty());
        assert_eq!(std::fs::read_to_string(&current).unwrap(), source);
        // Exercise the IME-to-keyboard transition through the result action.
        app.ime("项".into(), false);
        let result_key = |app: &mut App, key| {
            app.notebook_results_key(&Key::Named(key), ModifiersState::empty(), false)
        };
        assert!(result_key(&mut app, NamedKey::ArrowDown));
        assert!(app.focus == Focus::Search);
        app.ime("项目".into(), true);
        assert!(result_key(&mut app, NamedKey::ArrowDown));
        assert!(app.focus == Focus::Results);
        assert_eq!(app.ime_target(), 0);
        let target = app.result_path.clone().unwrap();
        let Document::Notes(n) = &mut app.document else {
            panic!("expected note")
        };
        assert_eq!(target, n.rows[0].0);
        n.editor.insert("待保存 ");
        n.edited();
        std::fs::write(&current, "externally replaced original contents").unwrap();
        assert!(result_key(&mut app, NamedKey::Enter));
        assert!(matches!(app.dialog, Some(Dialog::Error(_))));
        let Document::Notes(n) = &app.document else {
            panic!("expected note")
        };
        assert!(n.dirty);
        assert_eq!(n.editor.text, "待保存 正文保持不变");
        assert_eq!(
            n.document.as_ref().unwrap().file.path().as_ref(),
            Some(&current)
        );
        assert_eq!(
            std::fs::read_to_string(&current).unwrap(),
            "externally replaced original contents"
        );

        app.action(Action::Cancel);
        let Document::Notes(n) = &mut app.document else {
            panic!("expected note")
        };
        n.load(current.clone()).unwrap();
        n.editor.insert("已保存 ");
        n.edited();
        assert!(result_key(&mut app, NamedKey::Enter));
        assert!(app.focus == Focus::Editor);
        assert_eq!(app.ime_target(), 1);
        let Document::Notes(n) = &app.document else {
            panic!("expected note")
        };
        assert_eq!(n.document.as_ref().unwrap().file.path(), Some(target));
        assert_eq!(
            std::fs::read_to_string(&current).unwrap(),
            "已保存 externally replaced original contents"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn code_view_status_preserves_external_file_bytes() {
        let path = std::env::temp_dir().join(format!("jwm-code-{}.tsx", std::process::id()));
        let source = "\u{feff}const value = <div>中</div>;\r\n";
        std::fs::write(&path, source).unwrap();
        let mut app = App::new(true, vec![path.clone()]).unwrap();
        app.action(Action::ToggleCodeView);
        if let Document::Notes(n) = &mut app.document {
            assert!(n.editor.monospace);
            n.editor.cursor = n.editor.text.len();
            n.editor.anchor = n.editor.cursor;
            let status = n.editor_status();
            assert!(status.contains("行 2 · 字素列 1"));
            assert!(status.contains("UTF-8 BOM · CRLF"));
            assert!(!n.dirty);
            assert_eq!(n.editor.revision, 0);
            // Report the bytes the existing CRLF save mode would produce,
            // including unusual separators inserted by IME.
            let saved = n.editor.text.clone();
            for (text, expected) in [
                ("a\rb", "CR"),
                ("a\r\nb", "混合换行"),
                ("a\nb", "CRLF"),
                ("a", "无换行"),
            ] {
                n.editor.select_all();
                n.editor.insert(text);
                assert!(n.editor_status().contains(&format!("BOM · {expected} ·")));
            }
            n.editor.select_all();
            n.editor.insert(&saved);
        }
        app.action(Action::Save);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        app.action(Action::ToggleCodeView);
        if let Document::Notes(n) = &mut app.document {
            assert!(!n.editor.monospace);
            n.managed = true;
        }
        app.action(Action::ToggleCodeView);
        if let Document::Notes(n) = &mut app.document {
            assert!(!n.editor.monospace);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn document_find_selection_shortcut_keeps_the_source_clean() {
        let term = "编译🙂Error";
        let source = format!("{term}\n编译🙂error\n{term}");
        let filename = format!("jwm-find-selection-{}.txt", std::process::id());
        let path = std::env::temp_dir().join(filename);
        std::fs::write(&path, &source).unwrap();
        let mut app = App::new(true, vec![path.clone()]).unwrap();
        let start = source.rfind(term).unwrap();
        let Document::Notes(n) = &mut app.document else {
            unreachable!()
        };
        n.editor.anchor = source.len();
        n.editor.cursor = start;
        let key = Key::Character("e".into());
        let ctrl = ModifiersState::CONTROL;
        assert!(!app.find_selection_key(&key, ctrl | ModifiersState::SHIFT, false));
        assert!(app.find_selection_key(&key, ctrl, true));
        assert!(app.focus == Focus::Editor);
        assert!(app.find_selection_key(&key, ctrl, false));
        assert_eq!(app.ime_target(), 4);
        let Document::Notes(n) = &app.document else {
            unreachable!()
        };
        assert_eq!(n.find.query.text, term);
        assert_eq!((n.find.current, n.find.total), (1, 2));
        assert_eq!((n.editor.cursor, n.editor.anchor), (start, source.len()));

        app.action(Action::FindNext(false));
        app.action(Action::FindClose);
        assert_eq!(app.ime_target(), 1);
        let Document::Notes(n) = &mut app.document else {
            unreachable!()
        };
        assert_eq!(n.editor.selection(), 0..term.len());
        assert_eq!(n.editor.revision, 0);
        assert!(!n.dirty);
        n.managed = true;
        assert!(!app.find_selection_key(&key, ctrl, false));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn document_find_keeps_source_clean_and_routes_ime_and_dialogs() {
        let path = std::env::temp_dir().join(format!("jwm-find-{}.txt", std::process::id()));
        std::fs::write(&path, "前猫 后猫").unwrap();
        let mut app = App::new(true, vec![path.clone()]).unwrap();
        app.action(Action::Find);
        assert!(app.focus == Focus::Find);
        assert_eq!(app.ime_target(), 4);
        app.ime("猫".into(), false);
        app.ime("猫\n".into(), true);
        if let Document::Notes(n) = &app.document {
            assert_eq!(n.find.total, 2);
            assert_eq!(n.find.query.text, "猫");
            assert_eq!(n.editor.selection(), 3..6);
            assert!(!n.dirty);
            assert_eq!(n.editor.revision, 0);
        }
        let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 960, 680).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 960., 680., false);
        drop(cr);
        assert!(app.hits.iter().any(|hit| hit.action == Action::FindField));
        if let Ok(output) = std::env::var("JWM_FIND_TEST_PNG") {
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            let mut image = image::RgbImage::new(960, 680);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                let i = y as usize * stride + x as usize * 4;
                let native = u32::from_ne_bytes(data[i..i + 4].try_into().unwrap());
                *pixel = image::Rgb([(native >> 16) as u8, (native >> 8) as u8, native as u8]);
            }
            image.save(output).unwrap();
        }
        app.action(Action::FindNext(false));
        app.action(Action::SaveAs(false));
        assert_eq!(app.ime_target(), 3);
        app.ime("/tmp/find-export".into(), true);
        app.action(Action::Cancel);
        assert_eq!(app.ime_target(), 4);
        app.action(Action::Save);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "前猫 后猫");
        app.action(Action::FindClose);
        assert_eq!(app.ime_target(), 1);
        app.ime("替换".into(), true);
        if let Document::Notes(n) = &app.document {
            assert_eq!(n.editor.text, "前猫 后替换");
            assert!(n.dirty);
            assert!(!n.find.enabled);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn trash_success_restores_editor_focus_when_preview_is_reset() {
        // Exercise both replacement paths without touching the user's trash.
        let dir = std::env::temp_dir().join(format!("jwm-preview-trash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.txt");
        std::fs::write(&path, "next note").unwrap();
        let mut app = App::new(true, vec![path.clone()]).unwrap();
        if let Document::Notes(n) = &mut app.document {
            n.managed = true;
            n.directory = dir.clone();
            n.document = None;
        }
        for existing in [true, false] {
            if !existing {
                std::fs::remove_file(&path).unwrap();
                if let Document::Notes(n) = &mut app.document {
                    n.document = None;
                }
            }
            app.action(Action::TogglePreview);
            app.action(Action::Preview);
            app.action(Action::ConfirmTrash);
            assert!(app.dialog.is_none());
            assert!(app.focus == Focus::Editor);
            assert_eq!(app.ime_target(), 1);
            let Document::Notes(n) = &app.document else {
                panic!()
            };
            assert!(!n.preview.enabled);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preview_is_read_only_and_preserves_source_save_and_focus() {
        let path = std::env::temp_dir().join(format!("jwm-preview-{}.md", std::process::id()));
        let source = "# JWM 文本编辑器\n\n**实时 Markdown** · 原生预览\n\n- [x] 独立滚动\n- [ ] TSX 沙箱\n\n```rust\nfn main() { println!(\"Hello\"); }\n```";
        std::fs::write(&path, source).unwrap();
        let mut app = App::new(true, vec![path.clone()]).unwrap();
        app.action(Action::TogglePreview);
        let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 960, 680).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 960., 680., false);
        drop(cr);
        if let Ok(output) = std::env::var("JWM_PREVIEW_TEST_PNG") {
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            let mut image = image::RgbImage::new(960, 680);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                let i = y as usize * stride + x as usize * 4;
                let native = u32::from_ne_bytes(data[i..i + 4].try_into().unwrap());
                *pixel = image::Rgb([(native >> 16) as u8, (native >> 8) as u8, native as u8]);
            }
            image.save(output).unwrap();
        }
        app.pointer = (750., 200.);
        app.press(ModifiersState::empty());
        assert!(app.focus == Focus::Preview);
        assert_eq!(app.ime_target(), 0);
        app.ime("must not insert".into(), true);
        let Document::Notes(n) = &app.document else {
            panic!()
        };
        assert_eq!(n.editor.text, source);
        assert!(!n.dirty);
        assert_eq!(n.editor.revision, 0);
        app.action(Action::SaveAs(false));
        assert_eq!(app.ime_target(), 3);
        app.ime("-path".into(), true);
        let Some(Dialog::Picker(picker)) = &app.dialog else {
            panic!()
        };
        assert!(picker.path.text.contains("-path"));
        app.action(Action::Cancel);
        assert_eq!(app.ime_target(), 0);
        let Document::Notes(n) = &app.document else {
            panic!()
        };
        assert_eq!(n.editor.text, source);
        assert!(!n.dirty);
        app.action(Action::TogglePreview);
        assert!(app.focus == Focus::Editor);
        assert_eq!(app.ime_target(), 1);
        app.ime("edited ".into(), true);
        app.action(Action::Save);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("edited {source}")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn explicit_save_conflict_offers_a_copy_without_automatic_writes() {
        let directory =
            std::env::temp_dir().join(format!("jwm-app-recovery-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("source.md");
        std::fs::write(&path, "source").unwrap();
        let mut app = App::new(true, vec![path.clone()]).unwrap();
        app.ime("draft ".into(), true);
        let external = directory.join("external");
        std::fs::write(&external, "outside edit").unwrap();
        std::fs::rename(external, &path).unwrap();

        app.action(Action::Save);
        assert!(matches!(app.dialog, Some(Dialog::SaveConflict)));
        assert!(app.dialog_actions().contains(&Action::KeepConflictCopy));
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        app.action(Action::Cancel);
        assert!(app.dialog.is_none());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);

        app.action(Action::Save);
        app.action(Action::KeepConflictCopy);
        assert!(matches!(app.effects.as_slice(), [Effect::KeepConflictCopy]));
        app.write_conflict_copy(|_| false);
        assert!(app.dialog.is_none());
        assert!(app.focus == Focus::Editor);
        let Document::Notes(notes) = &app.document else {
            panic!("expected notes editor");
        };
        let copy = notes.document.as_ref().unwrap().file.path().unwrap();
        assert_ne!(copy, path);
        assert_eq!(std::fs::read_to_string(copy).unwrap(), "draft source");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "outside edit");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn composition_and_failed_save_keep_unsaved_text_in_the_window() {
        let path =
            std::env::temp_dir().join(format!("jwm-app-composition-{}.txt", std::process::id()));
        std::fs::write(&path, "original").unwrap();
        let mut app = App::new(true, vec![path.clone()]).unwrap();
        app.ime("备忘".into(), false);
        let Document::Notes(notes) = &app.document else {
            panic!()
        };
        assert_eq!(notes.editor.text, "original");
        assert!(!notes.dirty);
        app.ime("备忘".into(), true);
        app.close();
        assert!(matches!(app.dialog, Some(Dialog::Unsaved)));
        assert!(app.effects.is_empty());
        app.action(Action::Cancel);
        // A different inode guarantees an etag change even on coarse filesystems.
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, "changed elsewhere").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        app.action(Action::SaveClose);
        assert!(matches!(app.dialog, Some(Dialog::Error(_))));
        assert!(app.effects.is_empty());
        let Document::Notes(notes) = &app.document else {
            panic!()
        };
        assert_eq!(notes.editor.text, "备忘original");
        assert!(notes.dirty);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed elsewhere");
        std::fs::remove_file(path).unwrap();
    }
}
