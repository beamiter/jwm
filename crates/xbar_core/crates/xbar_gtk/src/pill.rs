//! One projected control, rendered as a GTK button.
//!
//! A pill is the widget equivalent of the rounded rect plus centered text that
//! `LayoutEngine` pushes into a scene: same text, same state classes, same
//! pointer bindings, and the same thin accent line under a control that
//! reports progress.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::pango::FontDescription;
use gtk4::prelude::*;
use gtk4::{
    Button, DrawingArea, EventControllerScroll, EventControllerScrollFlags, GestureClick, Label,
    Overlay, glib,
};
use xbar_core::controls::ControlSpec;
use xbar_core::presentation::{NodeId, PointerAction, Rgba};
use xbar_core::{ShellRoute, UserAction};

use crate::theme::Metrics;

/// Height of the progress underline's own row, matching the two logical pixels
/// the Cairo renderer leaves between a pill's bottom edge and its line.
const PROGRESS_ROW_HEIGHT: i32 = 3;

/// Sends a user action into the runtime.
pub type Dispatch = Rc<dyn Fn(UserAction)>;

/// Everything a pill needs that does not come from its own control.
#[derive(Clone)]
pub struct PillStyle {
    pub metrics: Metrics,
    pub accent: Rgba,
    /// The font the Cairo bars measure and draw with, already carrying the
    /// configured absolute size. Pill widths are derived from it directly so
    /// this bar reserves the same width for the same glyphs.
    pub font: FontDescription,
}

pub struct Pill {
    id: NodeId,
    button: Button,
    label: Label,
    font: FontDescription,
    progress_area: DrawingArea,
    /// Shared with the draw function, and with the pointer controllers through
    /// `bindings`, so a pill updates in place instead of being rebuilt.
    progress: Rc<Cell<Option<f32>>>,
    bindings: Rc<RefCell<Bindings>>,
}

/// The actions a pill currently answers to. Cleared while the control is
/// disabled, which is how `PillSpec` drops bindings for the Cairo bars.
#[derive(Default)]
struct Bindings {
    primary: Option<UserAction>,
    secondary: Option<UserAction>,
    scroll_up: Option<UserAction>,
    scroll_down: Option<UserAction>,
}

impl Bindings {
    fn from_spec(spec: &ControlSpec) -> Self {
        if !spec.state.enabled {
            return Self::default();
        }
        Self {
            primary: spec.bindings.primary,
            secondary: spec.bindings.secondary,
            scroll_up: spec.bindings.scroll_up,
            scroll_down: spec.bindings.scroll_down,
        }
    }

    const fn action(&self, input: PointerAction) -> Option<UserAction> {
        match input {
            PointerAction::Primary => self.primary,
            PointerAction::Secondary => self.secondary,
            PointerAction::ScrollUp => self.scroll_up,
            PointerAction::ScrollDown => self.scroll_down,
        }
    }
}

impl Pill {
    pub fn new(spec: &ControlSpec, style: &PillStyle, dispatch: Dispatch) -> Self {
        let PillStyle {
            metrics,
            accent,
            font,
        } = style.clone();

        // No ellipsizing: a pill is sized by its own text, and the client name
        // is the one cell that gives up width. That is also how `LayoutEngine`
        // divides a bar — status cells keep their natural width and are
        // dropped whole rather than squeezed.
        let label = Label::new(None);
        label.set_single_line_mode(true);

        let progress = Rc::new(Cell::new(None::<f32>));
        let progress_area = DrawingArea::new();
        progress_area.set_can_target(false);
        progress_area.set_valign(gtk4::Align::End);
        progress_area.set_content_height(PROGRESS_ROW_HEIGHT);
        progress_area.set_draw_func({
            let progress = progress.clone();
            move |area, context, _width, _height| {
                let Some(fraction) = progress.get() else {
                    return;
                };
                // Same recipe as `LayoutEngine::push_pill`: a one-pixel line
                // inset two pixels from each side of the pill.
                let width = f64::from(area.width());
                let inset = 2.0_f64.min(width * 0.5);
                let span = (width - inset * 2.0).max(0.0) * f64::from(fraction);
                if span <= 0.0 {
                    return;
                }
                let y = f64::from(PROGRESS_ROW_HEIGHT) - 1.5;
                context.set_source_rgba(
                    f64::from(accent.red),
                    f64::from(accent.green),
                    f64::from(accent.blue),
                    f64::from(accent.alpha),
                );
                context.set_line_width(1.0);
                context.move_to(inset, y);
                context.line_to(inset + span, y);
                let _ = context.stroke();
            }
        });

        let overlay = Overlay::new();
        overlay.set_child(Some(&label));
        overlay.add_overlay(&progress_area);

        let button = Button::new();
        button.set_child(Some(&overlay));
        button.add_css_class("pill");
        button.add_css_class(node_class(spec.id));
        button.set_size_request(-1, metrics.pill_height.round() as i32);
        // Focus would draw a ring the Cairo bars have no equivalent for, and a
        // bar is not part of any keyboard navigation order.
        button.set_can_focus(false);
        button.set_focus_on_click(false);

        let bindings = Rc::new(RefCell::new(Bindings::from_spec(spec)));

        button.connect_clicked({
            let bindings = bindings.clone();
            let dispatch = dispatch.clone();
            // Read the binding out before dispatching: dispatching re-enters
            // `update`, and a borrow still held here would abort the process.
            move |_| {
                let action = bindings.borrow().action(PointerAction::Primary);
                if let Some(action) = action {
                    dispatch(action);
                }
            }
        });

        let secondary = GestureClick::new();
        secondary.set_button(gtk4::gdk::BUTTON_SECONDARY);
        secondary.connect_pressed({
            let bindings = bindings.clone();
            let dispatch = dispatch.clone();
            move |_, _, _, _| {
                let action = bindings.borrow().action(PointerAction::Secondary);
                if let Some(action) = action {
                    dispatch(action);
                }
            }
        });
        button.add_controller(secondary);

        let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll({
            let bindings = bindings.clone();
            move |_, _, delta_y| {
                // The core owns the direction convention; a GTK wheel delta is
                // positive downwards, which is the opposite sign.
                let Some(input) = PointerAction::from_vertical_delta(-delta_y) else {
                    return glib::Propagation::Proceed;
                };
                let action = bindings.borrow().action(input);
                match action {
                    Some(action) => {
                        dispatch(action);
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                }
            }
        });
        button.add_controller(scroll);

        let pill = Self {
            id: spec.id,
            button,
            label,
            font,
            progress_area,
            progress,
            bindings,
        };
        pill.update(spec);
        pill
    }

    #[must_use]
    pub const fn id(&self) -> NodeId {
        self.id
    }

    #[must_use]
    pub const fn widget(&self) -> &Button {
        &self.button
    }

    /// Apply a fresh projection of the same control.
    pub fn update(&self, spec: &ControlSpec) {
        debug_assert_eq!(self.id, spec.id, "a pill only ever shows its own control");

        let text = spec.text();
        if self.label.text() != text {
            self.label.set_text(&text);
            // GTK would size the label by its logical extents alone, which
            // under-reports Nerd Font icons whose ink overflows their advance
            // — the pill would then be narrower here than in the Cairo bars.
            self.label.set_size_request(self.text_width(&text), -1);
        }

        set_class(&self.button, "selected", spec.state.selected);
        set_class(&self.button, "urgent", spec.state.urgent);
        // `LayoutEngine` folds both transport signals into one visual state.
        set_class(
            &self.button,
            "occupied",
            spec.state.occupied || spec.state.filled,
        );
        set_class(&self.button, "unavailable", !spec.available);

        *self.bindings.borrow_mut() = Bindings::from_spec(spec);

        let progress = spec
            .progress
            .map(|percent| (percent.as_f32() / 100.0).clamp(0.0, 1.0))
            .filter(|fraction| *fraction > 0.0);
        if self.progress.get() != progress {
            self.progress.set(progress);
            self.progress_area.queue_draw();
        }
    }
}

impl Pill {
    /// The union of a text's ink and logical extents, which is what
    /// `xbar_core`'s Pango measurer reports and what its layout engine turns
    /// into a pill width.
    fn text_width(&self, text: &str) -> i32 {
        if text.is_empty() {
            return -1;
        }
        let layout = self.label.create_pango_layout(Some(text));
        layout.set_single_paragraph_mode(true);
        layout.set_font_description(Some(&self.font));
        let (ink, logical) = layout.pixel_extents();
        if ink.width() <= 0 || ink.height() <= 0 {
            return logical.width().max(0);
        }
        let left = ink.x().min(logical.x());
        let right = (ink.x() + ink.width()).max(logical.x() + logical.width());
        (right - left).max(0)
    }
}

fn set_class(button: &Button, class: &str, present: bool) {
    if present {
        button.add_css_class(class);
    } else {
        button.remove_css_class(class);
    }
}

/// A stable per-control class, so a user stylesheet can reach one cell without
/// this bar having to invent an id scheme of its own.
fn node_class(id: NodeId) -> &'static str {
    match id {
        NodeId::Background => "background",
        NodeId::Tag(_) => "tag",
        NodeId::LayoutButton => "layout-button",
        NodeId::LayoutOption(_) => "layout-option",
        NodeId::Client => "client",
        NodeId::Monitor => "monitor",
        NodeId::Cpu => "cpu",
        NodeId::Memory => "memory",
        NodeId::Brightness => "brightness",
        NodeId::Battery => "battery",
        NodeId::Audio => "audio",
        NodeId::Network => "network",
        NodeId::Media => "media",
        NodeId::Theme => "theme",
        NodeId::Screenshot => "screenshot",
        NodeId::Clock => "clock",
        NodeId::ShellHub(route) => shell_class(route),
    }
}

const fn shell_class(route: ShellRoute) -> &'static str {
    match route {
        ShellRoute::Hub => "shell-hub",
        ShellRoute::Applications => "shell-applications",
        ShellRoute::Notifications => "shell-notifications",
        ShellRoute::Clipboard => "shell-clipboard",
        ShellRoute::Calendar => "shell-calendar",
        ShellRoute::Wallpaper => "shell-wallpaper",
    }
}

/// A horizontal run of pills that reconciles itself against a projection.
///
/// Rebuilding only when the control identities change keeps the common case —
/// a clock tick, a changed percentage — down to a label and a class update,
/// which is what the Cairo bars' damage tracking achieves by other means.
pub struct PillRow {
    container: gtk4::Box,
    pills: RefCell<Vec<Pill>>,
    style: PillStyle,
    dispatch: Dispatch,
}

impl PillRow {
    pub fn new(style: PillStyle, dispatch: Dispatch) -> Self {
        let container = gtk4::Box::new(gtk4::Orientation::Horizontal, style.metrics.item_gap);
        Self {
            container,
            pills: RefCell::new(Vec::new()),
            style,
            dispatch,
        }
    }

    #[must_use]
    pub const fn widget(&self) -> &gtk4::Box {
        &self.container
    }

    pub fn sync(&self, specs: &[ControlSpec]) {
        let mut pills = self.pills.borrow_mut();
        let reusable = pills.len() == specs.len()
            && pills
                .iter()
                .zip(specs)
                .all(|(pill, spec)| pill.id() == spec.id);

        if reusable {
            for (pill, spec) in pills.iter().zip(specs) {
                pill.update(spec);
            }
            return;
        }

        for pill in pills.drain(..) {
            self.container.remove(pill.widget());
        }
        for spec in specs {
            let pill = Pill::new(spec, &self.style, self.dispatch.clone());
            self.container.append(pill.widget());
            pills.push(pill);
        }
    }
}
