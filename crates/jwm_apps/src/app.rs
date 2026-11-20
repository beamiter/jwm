#[cfg(feature = "accessibility")]
pub(crate) mod accessibility;

use crate::{
    editor::Editor,
    notes::Notes,
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
    Image(Viewer),
}
#[derive(Clone, PartialEq)]
enum Action {
    New,
    Open,
    Save,
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
    Find,
    FindField,
    FindNext(bool),
    FindClose,
    Edit,
    TogglePreview,
    ToggleCodeView,
    Preview,
    Note(PathBuf),
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
enum Dialog {
    Picker(Box<Picker>),
    Error(String),
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
    focus: Focus,
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
            Document::Image(Viewer::new(paths))
        };
        Ok(Self {
            document,
            effects: vec![],
            dialog: None,
            dialog_focus: None,
            focus: Focus::Editor,
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
        let mut dirty = false;
        match &mut self.document {
            Document::Notes(n) if n.due.is_some_and(|due| Instant::now() >= due) => {
                if let Err(error) = n.save() {
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
        self.size = (width, height);
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
                    for (i, (path, title)) in n.rows.iter().enumerate() {
                        let rect = Rect::new(
                            8.,
                            body.y + 60. + i as f64 * 48. - n.list_scroll,
                            sidebar - 16.,
                            44.,
                        );
                        if rect.y + rect.h <= body.y + 60. || rect.y >= body.y + body.h {
                            continue;
                        }
                        if n.document.as_ref().and_then(|d| d.file.path()).as_ref() == Some(path) {
                            ui::rounded(cr, rect, ui::select());
                        } else if rect.contains(self.pointer.0, self.pointer.1) {
                            ui::rounded(cr, rect, ui::hover());
                        }
                        ui::text(
                            cr,
                            title,
                            Rect::new(rect.x + 12., rect.y, rect.w - 24., rect.h),
                            14.,
                            ui::text_color(),
                        );
                        self.hits.push(Hit {
                            rect: Rect::new(
                                rect.x,
                                rect.y.max(body.y + 60.),
                                rect.w,
                                rect.h.min((body.y + body.h - rect.y).max(0.)),
                            ),
                            action: Action::Note(path.clone()),
                            label: title.clone(),
                        });
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
                    ui::fill(cr, Rect::new(split, body.y, 1., body.h), ui::line());
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
                if let Some(image) = &v.image {
                    let (w, h) = v.dimensions();
                    let scale = v.scale(body.w, body.h);
                    let max_x = ((w * scale - body.w) / 2.).max(0.);
                    let max_y = ((h * scale - body.h) / 2.).max(0.);
                    v.pan.0 = v.pan.0.clamp(-max_x, max_x);
                    v.pan.1 = v.pan.1.clamp(-max_y, max_y);
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
                        "{} / {}  ·  {} × {}  ·  {}  ·  {}",
                        v.index + 1,
                        v.paths.len(),
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
                    self.write_as(&path, close);
                }
            }
        }
    }
    fn write_as(&mut self, path: &Path, close: bool) {
        if let Document::Notes(n) = &mut self.document {
            match n.save_as(path) {
                Ok(()) => {
                    // A notebook export does not discard an unsaved source note.
                    if close
                        && n.managed
                        && let Err(e) = n.save()
                    {
                        self.error(e);
                        return;
                    }
                    self.dialog = None;
                    if close {
                        self.effects.push(Effect::Close);
                    }
                }
                Err(e) => self.error(e),
            }
        }
    }
    fn action(&mut self, action: Action) {
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
            Action::Overwrite(path, close) => self.write_as(&path, close),
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
                        Action::Save => n.save(),
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
                        Action::Edit => {
                            self.focus = Focus::Editor;
                            Ok(())
                        }
                        Action::Note(path) => {
                            if n.document.as_ref().and_then(|d| d.file.path()).as_ref()
                                == Some(&path)
                            {
                                Ok(())
                            } else {
                                n.save().and_then(|()| n.load(path)).map(|()| {
                                    n.refresh();
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
                            Action::ZoomIn => v.zoom_by(1.25, body.w, body.h),
                            Action::ZoomOut => v.zoom_by(0.8, body.w, body.h),
                            Action::Actual => {
                                v.fit = false;
                                v.zoom = 1.;
                                v.pan = (0., 0.);
                            }
                            Action::Fit => {
                                v.fit = true;
                                v.pan = (0., 0.);
                            }
                            Action::Rotate => {
                                v.rotation = (v.rotation + 1) % 4;
                                v.pan = (0., 0.);
                            }
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
    pub fn scroll(&mut self, delta: f64, mods: ModifiersState) {
        if let Some(Dialog::Picker(p)) = &mut self.dialog {
            p.offset = (p.offset as isize + if delta > 0. { -3 } else { 3 })
                .clamp(0, p.entries.len().saturating_sub(1) as isize)
                as usize;
            return;
        }
        if self.dialog.is_some() {
            return;
        }
        let body = self.body();
        let sidebar = self.sidebar();
        match &mut self.document {
            Document::Notes(n) => {
                if self.pointer.0 < sidebar {
                    n.list_scroll = (n.list_scroll - delta * 36.)
                        .clamp(0., (n.rows.len() as f64 * 48. - body.h + 60.).max(0.));
                } else if n.preview.enabled && self.pointer.0 >= sidebar + (body.w - sidebar) / 2. {
                    n.preview.scroll = (n.preview.scroll - delta * 36.).max(0.);
                } else {
                    n.editor.scroll = (n.editor.scroll - delta * 36.).max(0.);
                }
            }
            Document::Image(v) => {
                if mods.control_key() {
                    v.zoom_by(if delta > 0. { 1.25 } else { 0.8 }, body.w, body.h);
                } else {
                    v.pan.1 += delta * 36.;
                }
            }
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
                    n.refresh();
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
            Document::Notes(_) if self.focus == Focus::Preview => None,
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
        if let Some(editor) = self.active_editor() {
            if commit {
                editor.preedit.clear();
                editor.insert(&text);
            } else {
                editor.preedit = text;
            }
        }
        if commit
            && self.focus != Focus::Preview
            && self.dialog.is_none()
            && let Document::Notes(n) = &mut self.document
        {
            if self.focus == Focus::Find {
                n.find.sync(&mut n.editor);
            } else if self.focus == Focus::Search {
                n.refresh();
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
                    if self.focus == Focus::Preview {
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
