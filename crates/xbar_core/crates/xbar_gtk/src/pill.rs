//! One projected control, rendered as a GTK button.
//!
//! A pill is the widget equivalent of the rounded rect plus centered text that
//! `LayoutEngine` pushes into a scene: same text, same state classes, same
//! pointer bindings, and the same thin accent line under a control that
//! reports progress.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use gtk4::pango::FontDescription;
use gtk4::prelude::*;
use gtk4::{
    Button, DrawingArea, EventControllerMotion, EventControllerScroll, EventControllerScrollFlags,
    GestureClick, Label, Overlay, glib,
};
use xbar_core::controls::ControlSpec;
use xbar_core::presentation::{
    NetworkTextMetrics, NodeId, PercentTextMetrics, PointerAction, Rgba,
};
use xbar_core::{BarEdge, ControlScroll, ScrollDelta, ScrollPhase, ShellRoute, UserAction};

use crate::theme::Metrics;

/// Height of the progress underline's own row, matching the two logical pixels
/// the Cairo renderer leaves between a pill's bottom edge and its line.
const PROGRESS_ROW_HEIGHT: i32 = 3;

/// Optional one-shot result channel for actions whose delivery policy depends
/// on bounded-queue acceptance. Most controls are fire-and-forget; the Dock
/// preview uses this receipt to distinguish a fresh ENTER from a renewal.
pub type DispatchCompletion = Sender<bool>;

/// Sends a user action into the runtime. When a completion sender is present,
/// the frontend reports whether the runtime accepted the action without an
/// issue (notably `QueueFull`).
pub type Dispatch = Rc<dyn Fn(UserAction, Option<DispatchCompletion>)>;

/// Everything a pill needs that does not come from its own control.
#[derive(Clone)]
pub struct PillStyle {
    pub edge: BarEdge,
    /// Shared across pills: a precision gesture cannot migrate with layout.
    pub scroll: Rc<RefCell<ControlScroll>>,
    pub metrics: Metrics,
    pub accent: Rgba,
    /// The font the Cairo bars measure and draw with, already carrying the
    /// configured absolute size. Pill widths are derived from it directly so
    /// this bar reserves the same width for the same glyphs.
    pub font: FontDescription,
    /// Both connection glyphs reserve one stable slot, including while the
    /// provider is starting or reconnecting.
    pub network_icons: [String; 2],
    /// Bounded glyph candidates for every percentage control, including mute,
    /// charging, and unavailable states. Live text never sizes these columns.
    pub metric_icons: [(NodeId, Vec<String>); 5],
}

pub struct Pill {
    id: NodeId,
    button: Button,
    label: Label,
    network_labels: Option<NetworkLabels>,
    percent_labels: Option<PercentLabels>,
    sidebar_metric: Option<SidebarMetric>,
    font: FontDescription,
    vertical: bool,
    minimum_height: i32,
    progress_area: DrawingArea,
    /// A non-measuring overlay retained for empty and occupied tags alike.
    occupancy_marker: Option<DrawingArea>,
    /// Shared with the draw function, and with the pointer controllers through
    /// `bindings`, so a pill updates in place instead of being rebuilt.
    progress: Rc<Cell<Option<f32>>>,
    bindings: Rc<RefCell<Bindings>>,
}

/// Horizontal network pills use fixed text columns. Arrow and icon
/// positions stay still while right-aligned rates change digits or IEC units.
struct NetworkLabels {
    row: gtk4::Box,
    icon: Label,
    rates: [Label; 2],
}

impl NetworkLabels {
    fn new(font: &FontDescription, icons: &[String; 2]) -> Self {
        let probe = Label::new(None);
        let metrics = NetworkTextMetrics::measure(&icons[0], &icons[1], |text| {
            text_width(&probe, font, text) as f32
        });
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        row.set_halign(gtk4::Align::Center);
        row.set_hexpand(false);
        let icon = fixed_text_slot(metrics.icon_width, 0.5);
        if metrics.icon_width > 0.0 {
            icon.set_margin_end((metrics.gap * 2.0).ceil() as i32);
            row.append(&icon);
        }
        let rates = [
            fixed_text_slot(metrics.rate_width, 1.0),
            fixed_text_slot(metrics.rate_width, 1.0),
        ];
        for (index, arrow) in ["\u{2193}", "\u{2191}"].into_iter().enumerate() {
            let direction = fixed_text_slot(metrics.arrow_width, 0.5);
            direction.set_text(arrow);
            row.append(&direction);
            row.append(&rates[index]);
        }
        rates[0].set_margin_end(metrics.gap.ceil() as i32);
        Self { row, icon, rates }
    }

    fn update(&self, spec: &ControlSpec) {
        if self.icon.text() != spec.icon {
            self.icon.set_text(&spec.icon);
        }
        if let Some(rates) = spec.network_rates() {
            for (label, rate) in self.rates.iter().zip(rates) {
                if label.text() != rate {
                    label.set_text(rate);
                }
            }
        }
    }
}

/// Percentage glyphs and numbers each get an independent stable column. A
/// centered combined string would still move the icon when 9% becomes 100%.
struct PercentLabels {
    row: gtk4::Box,
    icon: Label,
    value: Label,
}

impl PercentLabels {
    fn new(font: &FontDescription, icons: &[String]) -> Self {
        let probe = Label::new(None);
        let candidates: Vec<_> = icons.iter().map(String::as_str).collect();
        let metrics =
            PercentTextMetrics::measure(&candidates, |text| text_width(&probe, font, text) as f32);
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        row.set_halign(gtk4::Align::Center);
        row.set_hexpand(false);
        let icon = fixed_text_slot(metrics.icon_width, 0.5);
        if metrics.icon_width > 0.0 {
            icon.set_margin_end(metrics.gap.ceil() as i32);
            row.append(&icon);
        }
        let value = fixed_text_slot(metrics.value_width, 1.0);
        row.append(&value);
        Self { row, icon, value }
    }

    fn update(&self, spec: &ControlSpec) {
        if self.icon.text() != spec.icon {
            self.icon.set_text(&spec.icon);
        }
        if self.value.text() != spec.value {
            self.value.set_text(&spec.value);
        }
    }
}

fn fixed_text_slot(width: f32, align: f32) -> Label {
    let label = Label::new(None);
    label.set_single_line_mode(true);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    // A size request is only a minimum in GTK. Cap the label's natural
    // request too, so later text never enlarges this measured slot.
    label.set_max_width_chars(1);
    label.set_size_request(width.ceil() as i32, -1);
    label.set_hexpand(false);
    label.set_xalign(align);
    label
}

/// Sidebars keep their compact upright lines, reserving the icon row if any
/// configured state uses it. An empty mute/charging icon must not move all
/// controls below it by one line.
struct SidebarMetric {
    icon_row: bool,
    height: i32,
}

impl SidebarMetric {
    fn new(font: &FontDescription, icons: &[String], minimum_height: i32) -> Self {
        let icon_row = icons.iter().any(|icon| !icon.is_empty());
        let line_height =
            (f64::from(font.size()) / f64::from(gtk4::pango::SCALE)).ceil() as i32 + 3;
        let mut height = line_height * if icon_row { 2 } else { 1 };
        // Reserve tall fallback glyphs too; setting a minimum based only on
        // the font size still allows an emoji state to grow GTK's request.
        let probe = Label::new(None);
        for icon in icons {
            let text = if icon_row {
                format!("{icon}\n100%")
            } else {
                "100%".to_owned()
            };
            let layout = probe.create_pango_layout(Some(&text));
            layout.set_font_description(Some(font));
            let (ink, logical) = layout.pixel_extents();
            let top = ink.y().min(logical.y());
            let bottom = (ink.y() + ink.height()).max(logical.y() + logical.height());
            height = height.max(bottom - top);
        }
        Self {
            icon_row,
            height: (height + 6).max(minimum_height),
        }
    }

    fn text(&self, spec: &ControlSpec) -> String {
        if self.icon_row {
            format!("{}\n{}", spec.icon, spec.value)
        } else {
            spec.value.clone()
        }
    }
}

/// The actions a pill currently answers to. Cleared while the control is
/// disabled, which is how `PillSpec` drops bindings for the Cairo bars.
#[derive(Clone, Copy, Default)]
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
            edge,
            metrics,
            accent,
            font,
            network_icons,
            metric_icons,
            scroll: scroll_state,
        } = style;
        let accent = *accent;
        let minimum_height = metrics.pill_height.round().max(1.0) as i32;
        let icons = metric_icons
            .iter()
            .find_map(|(id, icons)| (*id == spec.id).then_some(icons.as_slice()));

        // Ordinary horizontal pills keep their natural text width; telemetry
        // uses fixed slots so changing readings cannot move adjacent controls.
        let label = Label::new(None);
        label.set_single_line_mode(!edge.is_vertical());
        if edge.is_vertical() {
            label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            label.set_max_width_chars(6);
            label.set_justify(gtk4::Justification::Center);
        }

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
        let network_labels = (spec.id == NodeId::Network && !edge.is_vertical())
            .then(|| NetworkLabels::new(font, network_icons));
        let percent_labels = icons
            .filter(|_| !edge.is_vertical())
            .map(|icons| PercentLabels::new(font, icons));
        let sidebar_metric = icons
            .filter(|_| edge.is_vertical())
            .map(|icons| SidebarMetric::new(font, icons, minimum_height));
        if let Some(network) = &network_labels {
            overlay.set_child(Some(&network.row));
        } else if let Some(percent) = &percent_labels {
            overlay.set_child(Some(&percent.row));
        } else {
            overlay.set_child(Some(&label));
        }
        overlay.add_overlay(&progress_area);
        let occupancy_marker = matches!(spec.id, NodeId::Tag(_)).then(|| {
            // Do not alter the label's request or placement. Paint only when
            // the actual allocated glyph ink leaves a clear bottom slot.
            let marker = DrawingArea::new();
            marker.add_css_class("occupancy-marker");
            marker.set_can_target(false);
            marker.set_draw_func({
                let label = label.clone();
                move |area, context, width, height| {
                    let (ink, _) = label.layout().pixel_extents();
                    let (_, layout_y) = label.layout_offsets();
                    let bottom = layout_y as f32 + ink.y() as f32 + ink.height() as f32;
                    let Some(bottom) =
                        label.compute_point(area, &gtk4::graphene::Point::new(0.0, bottom))
                    else {
                        return;
                    };
                    let Some((x, y)) =
                        occupancy_marker_origin(width, height, f64::from(bottom.y()))
                    else {
                        return;
                    };
                    let color = area.color();
                    context.set_source_rgba(
                        f64::from(color.red()),
                        f64::from(color.green()),
                        f64::from(color.blue()),
                        f64::from(color.alpha()),
                    );
                    // A 4px segment plus round 1px caps is a 6x2 dash.
                    context.set_line_width(2.0);
                    context.set_line_cap(gtk4::cairo::LineCap::Round);
                    context.move_to(x + 1.0, y + 1.0);
                    context.line_to(x + 5.0, y + 1.0);
                    let _ = context.stroke();
                }
            });
            overlay.add_overlay(&marker);
            overlay.set_measure_overlay(&marker, false);
            marker
        });

        let button = Button::new();
        button.set_child(Some(&overlay));
        button.add_css_class("pill");
        button.add_css_class(node_class(spec.id));
        button.set_size_request(-1, metrics.pill_height.round() as i32);
        // Mouse use preserves client focus; explicit keyboard navigation can
        // still reach every control and reveals the theme's focus outline.
        button.set_can_focus(true);
        button.set_focus_on_click(false);

        let bindings = Rc::new(RefCell::new(Bindings::from_spec(spec)));

        button.connect_clicked({
            let bindings = bindings.clone();
            let dispatch = dispatch.clone();
            let scroll_state = scroll_state.clone();
            // Read the binding out before dispatching: dispatching re-enters
            // `update`, and a borrow still held here would abort the process.
            move |_| {
                scroll_state.borrow_mut().reset();
                let action = bindings.borrow().action(PointerAction::Primary);
                if let Some(action) = action {
                    dispatch(action, None);
                }
            }
        });

        let secondary = GestureClick::new();
        secondary.set_button(gtk4::gdk::BUTTON_SECONDARY);
        secondary.connect_pressed({
            let bindings = bindings.clone();
            let dispatch = dispatch.clone();
            let scroll_state = scroll_state.clone();
            move |_, _, _, _| {
                scroll_state.borrow_mut().reset();
                let action = bindings.borrow().action(PointerAction::Secondary);
                if let Some(action) = action {
                    dispatch(action, None);
                }
            }
        });
        button.add_controller(secondary);

        let id = spec.id;
        let edge = *edge;
        let motion = EventControllerMotion::new();
        motion.connect_enter({
            let state = scroll_state.clone();
            move |_, _, _| state.borrow_mut().hover(Some(id), edge, Instant::now())
        });
        motion.connect_leave({
            let state = scroll_state.clone();
            move |_| state.borrow_mut().hover(None, edge, Instant::now())
        });
        button.add_controller(motion);
        button.connect_scale_factor_notify({
            let state = scroll_state.clone();
            move |_| state.borrow_mut().reset()
        });
        let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
        let pending_start = Rc::new(Cell::new(false));
        scroll.connect_scroll_begin({
            let state = scroll_state.clone();
            let pending_start = pending_start.clone();
            move |_| {
                state.borrow_mut().reset();
                pending_start.set(true);
            }
        });
        scroll.connect_scroll_end({
            let state = scroll_state.clone();
            let pending_start = pending_start.clone();
            move |_| {
                pending_start.set(false);
                state.borrow_mut().cancel(Instant::now());
            }
        });
        scroll.connect_scroll({
            let bindings = bindings.clone();
            let state = scroll_state.clone();
            move |controller, delta_x, delta_y| {
                let bindings = *bindings.borrow();
                if bindings.scroll_up.is_none() && bindings.scroll_down.is_none() {
                    return glib::Propagation::Proceed;
                }
                // GDK surface deltas are logical pixels; wheel deltas are
                // lines. GDK's positive-down/right convention is inverted once.
                let delta = match controller.unit() {
                    gtk4::gdk::ScrollUnit::Surface => ScrollDelta::Pixels {
                        x: -delta_x,
                        y: -delta_y,
                    },
                    _ => ScrollDelta::Lines {
                        x: -delta_x,
                        y: -delta_y,
                    },
                };
                let phase = if pending_start.replace(false) {
                    ScrollPhase::Started
                } else {
                    ScrollPhase::Moved
                };
                let action = state
                    .borrow_mut()
                    .step(id, edge, delta, phase, Instant::now())
                    .and_then(|input| bindings.action(input));
                if let Some(action) = action {
                    dispatch(action, None);
                }
                // Own the entire gesture, including subthreshold packets,
                // instead of also scrolling a parent container underneath it.
                glib::Propagation::Stop
            }
        });
        button.add_controller(scroll);

        let pill = Self {
            id: spec.id,
            button,
            label,
            network_labels,
            percent_labels,
            sidebar_metric,
            font: font.clone(),
            vertical: edge.is_vertical(),
            minimum_height,
            progress_area,
            occupancy_marker,
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

        if let Some(network) = &self.network_labels {
            network.update(spec);
        } else if let Some(percent) = &self.percent_labels {
            percent.update(spec);
        } else {
            let text = if let Some(metric) = &self.sidebar_metric {
                metric.text(spec)
            } else if self.vertical {
                spec.sidebar_lines().join("\n")
            } else {
                spec.text()
            };
            if self.label.text() != text {
                self.label.set_text(&text);
                // Reserve the union of icon ink and logical extents, as the
                // Cairo bars do, rather than GTK's logical extents alone.
                if self.vertical {
                    self.label.set_size_request(0, -1);
                    let line_height = (f64::from(self.font.size()) / f64::from(gtk4::pango::SCALE))
                        .ceil() as i32
                        + 3;
                    let height = self.sidebar_metric.as_ref().map_or_else(
                        || {
                            (text.lines().count().max(1) as i32 * line_height + 6)
                                .max(self.minimum_height)
                        },
                        |metric| metric.height,
                    );
                    self.button.set_size_request(self.minimum_height, height);
                } else {
                    let width = if text.is_empty() {
                        -1
                    } else {
                        text_width(&self.label, &self.font, &text)
                    };
                    self.label.set_size_request(width, -1);
                }
            }
        }
        let accessible = spec.accessible_label();
        self.button.set_tooltip_text(Some(&spec.tooltip_text()));
        self.button
            .update_property(&[gtk4::accessible::Property::Label(&accessible)]);
        self.button.set_can_focus(spec.state.enabled);

        set_class(
            &self.button,
            "selected",
            spec.state.selected || (matches!(spec.id, NodeId::Tag(_)) && spec.state.filled),
        );
        set_class(&self.button, "urgent", spec.state.urgent);
        // `filled` means filter/selection, not the presence of clients.
        let occupied =
            spec.state.occupied || (!matches!(spec.id, NodeId::Tag(_)) && spec.state.filled);
        set_class(&self.button, "occupied", occupied);
        if let Some(marker) = &self.occupancy_marker {
            // Opacity leaves the same overlay allocated in both states.
            marker.set_opacity(if spec.state.occupied { 1.0 } else { 0.0 });
            // Text changes can keep the same request but change ink extents.
            marker.queue_draw();
        }
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

/// The union of a text's ink and logical extents, matching the shared Pango
/// measurer rather than GTK's logical-only label request.
fn text_width(label: &Label, font: &FontDescription, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let layout = label.create_pango_layout(Some(text));
    layout.set_single_paragraph_mode(true);
    layout.set_font_description(Some(font));
    let (ink, logical) = layout.pixel_extents();
    if ink.width() <= 0 || ink.height() <= 0 {
        return logical.width().max(0);
    }
    let left = ink.x().min(logical.x());
    let right = (ink.x() + ink.width()).max(logical.x() + logical.width());
    (right - left).max(0)
}

/// Marker coordinates inside the button's one-pixel layout border. Keeping a
/// two-pixel inner bottom inset gives the shared three-pixel outer offset.
/// Two pixels of air above the marker protect tall or shifted custom glyphs.
fn occupancy_marker_origin(width: i32, height: i32, ink_bottom: f64) -> Option<(f64, f64)> {
    let y = f64::from(height) - 4.0;
    if width < 8 || height < 6 || !ink_bottom.is_finite() || ink_bottom + 2.0 > y {
        return None;
    }
    Some(((f64::from(width) - 6.0) * 0.5, y))
}

#[cfg(test)]
mod occupancy_tests {
    use super::occupancy_marker_origin;

    #[test]
    fn allocated_ink_must_leave_clearance_without_changing_label_geometry() {
        assert_eq!(occupancy_marker_origin(36, 30, 24.0), Some((15.0, 26.0)));
        assert_eq!(occupancy_marker_origin(36, 30, 24.1), None);
        assert_eq!(occupancy_marker_origin(36, 20, 16.0), None);
        assert_eq!(occupancy_marker_origin(7, 30, 10.0), None);
        assert_eq!(occupancy_marker_origin(36, 5, 0.0), None);
        assert_eq!(occupancy_marker_origin(36, 30, f64::NAN), None);
        assert_eq!(occupancy_marker_origin(36, 30, f64::INFINITY), None);
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
        NodeId::WorkspaceOverflow => "workspace-overflow",
        NodeId::StatusOverflow => "status-overflow",
        NodeId::LayoutButton => "layout-button",
        NodeId::LayoutOption(_) => "layout-option",
        NodeId::Client => "client",
        NodeId::ClientIcon => "client-icon",
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
        NodeId::DockShelf => "dock-shelf",
        NodeId::MinimizedWindow(_) => "minimized-window",
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
        ShellRoute::Theme => "shell-theme",
    }
}

/// An axis-aware run of pills that reconciles itself against a projection.
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
        let orientation = if style.edge.is_vertical() {
            gtk4::Orientation::Vertical
        } else {
            gtk4::Orientation::Horizontal
        };
        let container = gtk4::Box::new(orientation, style.metrics.item_gap);
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

    pub(crate) fn focused_control(&self) -> Option<NodeId> {
        self.pills
            .borrow()
            .iter()
            .find(|pill| pill.button.is_focus())
            .map(Pill::id)
    }

    pub(crate) fn focus_control(&self, id: NodeId) -> bool {
        self.pills
            .borrow()
            .iter()
            .find(|pill| pill.id() == id)
            .is_some_and(|pill| pill.button.grab_focus())
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
