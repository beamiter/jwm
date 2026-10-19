//! Renderer-neutral layout, scene, and interaction primitives.
//!
//! All geometry in this module is expressed in logical `f32` units. A
//! frontend applies its output scale only while translating a [`Scene`] to
//! device pixels. This keeps layout and hit testing identical across Cairo,
//! wgpu, toolkit, and web renderers.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::ThemeMode;
use crate::controls::{
    BarPresentation, ControlSpec, PresentationProjector, bounded_borrowed_control_text,
    bounded_cow_control_text,
};
use crate::display::{BatteryThresholds, IconSet, UsageThresholds, VolumeThresholds};
use crate::model::{
    BarView, LayoutId, MAX_MODEL_TAGS, Percent, ShellRoute, TagId, UserAction, WindowToken,
};
use crate::tag_contrast::{TagContrastMode, TagPalette};

/// A point in logical (DPI-independent) coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A size in logical (DPI-independent) coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    #[must_use]
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            width: finite_non_negative(self.width),
            height: finite_non_negative(self.height),
        }
    }
}

/// A logical rectangle. Right and bottom edges are exclusive for hit tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    #[must_use]
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub const fn from_size(size: Size) -> Self {
        Self::new(0.0, 0.0, size.width, size.height)
    }

    #[must_use]
    pub fn right(self) -> f32 {
        self.x + self.width
    }

    #[must_use]
    pub fn bottom(self) -> f32 {
        self.y + self.height
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.x.is_finite()
            || !self.y.is_finite()
            || !self.width.is_finite()
            || !self.height.is_finite()
            || self.width <= 0.0
            || self.height <= 0.0
    }

    #[must_use]
    pub fn contains(self, point: Point) -> bool {
        !self.is_empty()
            && point.x >= self.x
            && point.y >= self.y
            && point.x < self.right()
            && point.y < self.bottom()
    }

    #[must_use]
    pub fn intersection(self, other: Self) -> Option<Self> {
        if self.is_empty() || other.is_empty() {
            return None;
        }
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right > left && bottom > top {
            Some(Self::new(left, top, right - left, bottom - top))
        } else {
            None
        }
    }

    #[must_use]
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self::new(left, top, right - left, bottom - top)
    }

    #[must_use]
    fn touches(self, other: Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }

    #[must_use]
    pub fn inset(self, x: f32, y: f32) -> Self {
        let x = finite_non_negative(x).min(self.width * 0.5);
        let y = finite_non_negative(y).min(self.height * 0.5);
        Self::new(
            self.x + x,
            self.y + y,
            (self.width - 2.0 * x).max(0.0),
            (self.height - 2.0 * y).max(0.0),
        )
    }
}

/// Renderer-neutral premultiplied-independent RGBA color.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rgba {
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub alpha: f32,
}

impl Rgba {
    #[must_use]
    pub const fn new(red: f32, green: f32, blue: f32, alpha: f32) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    #[must_use]
    pub const fn rgb8(red: u8, green: u8, blue: u8) -> Self {
        Self::new(
            red as f32 / 255.0,
            green as f32 / 255.0,
            blue as f32 / 255.0,
            1.0,
        )
    }
}

/// A stable semantic identifier. IDs do not depend on frame order or bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeId {
    Background,
    Tag(TagId),
    /// Compact workspace navigation when the full tag strip does not fit.
    WorkspaceOverflow,
    /// Entry to the shell quick settings when status controls are condensed.
    StatusOverflow,
    LayoutButton,
    LayoutOption(LayoutId),
    Client,
    /// Desktop icon of the application owning the focused window.
    ClientIcon,
    Monitor,
    Cpu,
    Memory,
    Brightness,
    Battery,
    Audio,
    Network,
    Media,
    Theme,
    Screenshot,
    Clock,
    DockShelf,
    MinimizedWindow(WindowToken),
    /// One entry point into the window manager's own shell surface. Carrying
    /// the route makes each page a distinct node, so hover, damage tracking
    /// and hit testing keep working when a bar shows several of them.
    ShellHub(ShellRoute),
}

/// State carried by scene nodes so every renderer applies interaction styling
/// consistently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualState {
    pub hovered: bool,
    pub selected: bool,
    pub urgent: bool,
    pub occupied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextAlign {
    Start,
    Center,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub color: Rgba,
    pub width: f32,
}

/// A raster image a renderer is asked to draw.
///
/// The scene names a file rather than carrying pixels: decoding and texture
/// caching belong to the renderer, which is the only layer that knows its own
/// pixel format and how long a texture should live. `key` is stable for a given
/// file, so a renderer can cache on it without hashing paths every frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageSource {
    pub key: u64,
    pub path: std::path::PathBuf,
}

impl From<&crate::app_icon::AppIcon> for ImageSource {
    fn from(icon: &crate::app_icon::AppIcon) -> Self {
        Self {
            key: icon.key,
            path: icon.path.clone(),
        }
    }
}

/// Minimal display list understood by both retained and immediate renderers.
/// Multiple primitives may share a semantic [`NodeId`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SceneNode {
    Background {
        id: NodeId,
        bounds: Rect,
        fill: Rgba,
    },
    RoundedRect {
        id: NodeId,
        bounds: Rect,
        radius: f32,
        fill: Rgba,
        stroke: Option<Stroke>,
        state: VisualState,
    },
    Text {
        id: NodeId,
        bounds: Rect,
        text: String,
        size: f32,
        color: Rgba,
        align: TextAlign,
        state: VisualState,
    },
    Polyline {
        id: NodeId,
        bounds: Rect,
        points: Vec<Point>,
        color: Rgba,
        width: f32,
        state: VisualState,
    },
    /// A raster image scaled to fit `bounds` while keeping its aspect ratio.
    /// A renderer that cannot decode the file draws nothing, which is the same
    /// outcome as an unresolved icon.
    Image {
        id: NodeId,
        bounds: Rect,
        source: ImageSource,
        state: VisualState,
    },
}

impl SceneNode {
    #[must_use]
    pub const fn id(&self) -> NodeId {
        match self {
            Self::Background { id, .. }
            | Self::RoundedRect { id, .. }
            | Self::Text { id, .. }
            | Self::Polyline { id, .. }
            | Self::Image { id, .. } => *id,
        }
    }

    #[must_use]
    pub const fn bounds(&self) -> Rect {
        match self {
            Self::Background { bounds, .. }
            | Self::RoundedRect { bounds, .. }
            | Self::Text { bounds, .. }
            | Self::Polyline { bounds, .. }
            | Self::Image { bounds, .. } => *bounds,
        }
    }

    #[must_use]
    pub const fn visual_state(&self) -> VisualState {
        match self {
            Self::Background { .. } => VisualState {
                hovered: false,
                selected: false,
                urgent: false,
                occupied: false,
            },
            Self::RoundedRect { state, .. }
            | Self::Text { state, .. }
            | Self::Polyline { state, .. }
            | Self::Image { state, .. } => *state,
        }
    }
}

/// Native pointer input translated into presentation semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerAction {
    Primary,
    Secondary,
    ScrollUp,
    ScrollDown,
}

impl PointerAction {
    /// Translate the conventional X11 core pointer button numbers used by
    /// XCB and x11rb frontends.
    #[must_use]
    pub const fn from_x11_button(button: u8) -> Option<Self> {
        match button {
            1 => Some(Self::Primary),
            3 => Some(Self::Secondary),
            4 => Some(Self::ScrollUp),
            5 => Some(Self::ScrollDown),
            _ => None,
        }
    }

    /// Translate a vertical wheel/trackpad delta. Positive deltas scroll up;
    /// zero and non-finite deltas do not produce an action.
    #[must_use]
    pub fn from_vertical_delta(delta: f64) -> Option<Self> {
        if !delta.is_finite() || delta == 0.0 {
            None
        } else if delta > 0.0 {
            Some(Self::ScrollUp)
        } else {
            Some(Self::ScrollDown)
        }
    }
}

/// One semantic interaction target in logical coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitRegion {
    pub id: NodeId,
    pub bounds: Rect,
    pub primary: Option<UserAction>,
    pub secondary: Option<UserAction>,
    pub scroll_up: Option<UserAction>,
    pub scroll_down: Option<UserAction>,
}

impl HitRegion {
    #[must_use]
    pub const fn action(&self, input: PointerAction) -> Option<UserAction> {
        match input {
            PointerAction::Primary => self.primary,
            PointerAction::Secondary => self.secondary,
            PointerAction::ScrollUp => self.scroll_up,
            PointerAction::ScrollDown => self.scroll_down,
        }
    }
}

/// A complete clipped frame plus its semantic hit map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub viewport: Size,
    pub clip: Rect,
    pub nodes: Vec<SceneNode>,
    pub hits: Vec<HitRegion>,
}

/// Coalesced logical damage produced by comparing two retained scenes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Damage {
    regions: Vec<Rect>,
}

impl Damage {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    #[must_use]
    pub fn regions(&self) -> &[Rect] {
        &self.regions
    }

    fn add(&mut self, region: Rect) {
        if region.is_empty() {
            return;
        }

        let mut merged = region;
        let mut index = 0;
        while index < self.regions.len() {
            if merged.touches(self.regions[index]) {
                merged = merged.union(self.regions.swap_remove(index));
                index = 0;
            } else {
                index += 1;
            }
        }
        self.regions.push(merged);
    }
}

impl Scene {
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<&HitRegion> {
        let visible = self
            .clip
            .intersection(Rect::from_size(self.viewport.normalized()))?;
        if !visible.contains(point) {
            return None;
        }
        self.hits
            .iter()
            .rev()
            .find(|region| region.bounds.contains(point))
    }

    #[must_use]
    pub fn action_at(&self, point: Point, input: PointerAction) -> Option<UserAction> {
        self.hit_test(point).and_then(|region| region.action(input))
    }

    pub fn nodes_for(&self, id: NodeId) -> impl Iterator<Item = &SceneNode> {
        self.nodes.iter().filter(move |node| node.id() == id)
    }

    /// Union of all primitives carrying one semantic identity.
    #[must_use]
    pub fn bounds_for(&self, id: NodeId) -> Option<Rect> {
        let nodes: Vec<_> = self.nodes_for(id).collect();
        component_bounds(&nodes)
    }

    /// Compute paint damage from stable semantic node identities.
    ///
    /// When a component changes, both its previous and current bounds are
    /// invalidated so moving or shrinking nodes cannot leave stale pixels.
    #[must_use]
    pub fn damage_from(&self, previous: &Self) -> Damage {
        let mut damage = Damage::default();
        if self.viewport != previous.viewport || self.clip != previous.clip {
            damage.add(previous.clip);
            damage.add(self.clip);
            return damage;
        }

        let mut seen = HashSet::new();
        let ids = previous
            .nodes
            .iter()
            .chain(&self.nodes)
            .map(SceneNode::id)
            .filter(|id| seen.insert(*id));

        for id in ids {
            let old_nodes: Vec<_> = previous.nodes_for(id).collect();
            let new_nodes: Vec<_> = self.nodes_for(id).collect();
            if old_nodes == new_nodes {
                continue;
            }
            if let Some(bounds) = component_bounds(&old_nodes) {
                damage.add(bounds);
            }
            if let Some(bounds) = component_bounds(&new_nodes) {
                damage.add(bounds);
            }
        }

        // Per-component equality cannot observe display-list reordering.
        // Reordering may coincide with additions or removals, so comparing ID
        // counts is insufficient: unchanged overlapping nodes can still
        // produce different pixels. Conservatively repaint every old and new
        // bound whenever semantic display-list order changes.
        let previous_order: Vec<_> = previous.nodes.iter().map(SceneNode::id).collect();
        let current_order: Vec<_> = self.nodes.iter().map(SceneNode::id).collect();
        if previous_order != current_order {
            for node in previous.nodes.iter().chain(&self.nodes) {
                damage.add(node.bounds());
            }
        }
        damage
    }
}

fn component_bounds(nodes: &[&SceneNode]) -> Option<Rect> {
    nodes
        .iter()
        .map(|node| node.bounds())
        .filter(|bounds| !bounds.is_empty())
        .reduce(Rect::union)
}

/// Pointer state retained by a frontend between scene builds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct InteractionState {
    hovered: Option<NodeId>,
    pressed: Option<NodeId>,
    #[serde(default)]
    pressed_bindings: Option<crate::controls::InputBindings>,
    #[serde(default)]
    pointer: Option<Point>,
}

/// Detailed hover result used by Dock presenters to deduplicate enter/leave
/// commands while still redrawing proximity magnification within one item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HoverTransition {
    pub previous: Option<NodeId>,
    pub current: Option<NodeId>,
    pub pointer_changed: bool,
}

impl HoverTransition {
    #[must_use]
    pub fn target_changed(self) -> bool {
        self.previous != self.current
    }

    #[must_use]
    pub fn needs_redraw(self) -> bool {
        self.target_changed()
            || (self.pointer_changed
                && matches!(
                    (self.previous, self.current),
                    (
                        Some(NodeId::MinimizedWindow(_)),
                        Some(NodeId::MinimizedWindow(_))
                    )
                ))
    }
}

impl InteractionState {
    #[must_use]
    pub const fn hovered(&self) -> Option<NodeId> {
        self.hovered
    }

    #[must_use]
    pub const fn pressed(&self) -> Option<NodeId> {
        self.pressed
    }

    #[must_use]
    pub const fn pointer(&self) -> Option<Point> {
        self.pointer
    }

    /// Update exact pointer position and semantic target in one hit test.
    pub fn update_hover_transition(&mut self, scene: &Scene, point: Point) -> HoverTransition {
        let previous = self.hovered;
        let current = scene.hit_test(point).map(|region| region.id);
        let pointer_changed = self.pointer != Some(point);
        self.pointer = Some(point);
        self.hovered = current;
        HoverTransition {
            previous,
            current,
            pointer_changed,
        }
    }

    /// Updates hover state and returns whether a redraw is needed.
    pub fn update_hover(&mut self, scene: &Scene, point: Point) -> bool {
        self.update_hover_transition(scene, point).needs_redraw()
    }

    pub fn clear_hover(&mut self) -> bool {
        let hover_changed = self.hovered.take().is_some();
        let press_changed = self.pressed.take().is_some();
        self.pressed_bindings = None;
        let pointer_changed = self.pointer.take().is_some();
        hover_changed || press_changed || pointer_changed
    }

    /// Cancel a pending activation without changing hover state.
    pub fn cancel_press(&mut self) -> bool {
        self.pressed_bindings = None;
        self.pressed.take().is_some()
    }

    pub fn press(&mut self, scene: &Scene, point: Point) -> bool {
        self.pointer = Some(point);
        let hit = scene.hit_test(point);
        let next = hit.map(|region| region.id);
        let bindings = hit.map(|region| crate::controls::InputBindings {
            primary: region.primary,
            secondary: region.secondary,
            scroll_up: region.scroll_up,
            scroll_down: region.scroll_down,
        });
        let changed = self.pressed != next || self.pressed_bindings != bindings;
        self.pressed = next;
        self.pressed_bindings = bindings;
        changed
    }

    /// Activates only when press and release resolve to the same stable node
    /// and action. A model refresh may change an overflow binding or reuse a
    /// numeric window token; the held gesture must not acquire that new intent.
    pub fn release(
        &mut self,
        scene: &Scene,
        point: Point,
        input: PointerAction,
    ) -> Option<UserAction> {
        let released = scene.hit_test(point);
        let action = released
            .filter(|region| Some(region.id) == self.pressed)
            .and_then(|region| region.action(input))
            .filter(|action| {
                self.pressed_bindings.is_some_and(|bindings| {
                    let pressed = match input {
                        PointerAction::Primary => bindings.primary,
                        PointerAction::Secondary => bindings.secondary,
                        PointerAction::ScrollUp => bindings.scroll_up,
                        PointerAction::ScrollDown => bindings.scroll_down,
                    };
                    pressed == Some(*action)
                })
            });
        self.pressed = None;
        self.pressed_bindings = None;
        action
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutChoice {
    pub id: LayoutId,
    pub label: String,
}

/// `serde(default)` so a config written before a label existed keeps loading:
/// adding an icon must never turn an existing bar into a startup failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PresentationLabels {
    pub clock: String,
    pub screenshot: String,
    pub theme_dark: String,
    pub theme_light: String,
    pub monitor: String,
    pub cpu: String,
    pub memory: String,
    pub audio: String,
    pub muted: String,
    pub brightness: String,
    pub battery: String,
    pub charging: String,
    pub network: String,
    pub network_offline: String,
    pub media_playing: String,
    pub media_paused: String,
    /// Icon per shell route, in [`ShellRoute::ALL`] order.
    pub shell_routes: [String; 7],
}

impl Default for PresentationLabels {
    fn default() -> Self {
        Self {
            clock: "🕐".to_owned(),
            screenshot: "📸".to_owned(),
            theme_dark: "🌙".to_owned(),
            theme_light: "☀️".to_owned(),
            monitor: "🖥".to_owned(),
            cpu: "🧠".to_owned(),
            memory: "💾".to_owned(),
            audio: "🔊".to_owned(),
            muted: "🔇".to_owned(),
            brightness: "🔆".to_owned(),
            battery: "🔋".to_owned(),
            charging: "⚡".to_owned(),
            network: "📶".to_owned(),
            network_offline: "📵".to_owned(),
            media_playing: "▶".to_owned(),
            media_paused: "⏸".to_owned(),
            shell_routes: PresentationLabels::DEFAULT_SHELL_ROUTE_ICONS.map(str::to_owned),
        }
    }
}

impl PresentationLabels {
    /// Emoji rather than Nerd Font glyphs, matching every other default here:
    /// a bar with no patched font still shows something recognizable.
    pub const DEFAULT_SHELL_ROUTE_ICONS: [&'static str; 7] =
        ["◈", "🚀", "🔔", "📋", "📅", "🖼", "🎨"];

    /// Icon for one route, falling back to the built-in default when a host
    /// supplies a blank override.
    #[must_use]
    pub fn shell_route(&self, route: ShellRoute) -> &str {
        let index = route.code() as usize;
        self.shell_routes
            .get(index)
            .map(String::as_str)
            .filter(|icon| !icon.trim().is_empty())
            .unwrap_or(Self::DEFAULT_SHELL_ROUTE_ICONS[index])
    }

    fn from_icon_set_with(icons: &IconSet, own: impl Fn(&str) -> String) -> Self {
        Self {
            clock: own(&icons.clock),
            screenshot: own(&icons.screenshot),
            theme_dark: own(&icons.theme_dark),
            theme_light: own(&icons.theme_light),
            monitor: own(&icons.monitor),
            cpu: own(&icons.cpu),
            memory: own(&icons.memory),
            audio: own(&icons.volume_high),
            muted: own(&icons.volume_muted),
            brightness: own(&icons.brightness),
            battery: own(&icons.battery),
            charging: own(&icons.battery_charging),
            network: "\u{f05a9}".to_owned(),
            network_offline: "\u{f05aa}".to_owned(),
            media_playing: "\u{f040a}".to_owned(),
            media_paused: "\u{f03e4}".to_owned(),
            // Deliberately the same glyphs JWM's own shell rows use, so the
            // bar entry and the page it opens read as one surface.
            shell_routes: PresentationLabels::NERD_FONT_SHELL_ROUTE_ICONS.map(str::to_owned),
        }
    }

    fn bounded_from_icon_set(icons: &IconSet) -> Self {
        Self::from_icon_set_with(icons, bounded_borrowed_control_text)
    }
}

impl From<&IconSet> for PresentationLabels {
    fn from(icons: &IconSet) -> Self {
        Self::from_icon_set_with(icons, str::to_owned)
    }
}

impl PresentationLabels {
    /// Labels matching the shared toolkit Nerd Font preset.
    #[must_use]
    pub fn nerd_font() -> Self {
        Self::from(&IconSet::nerd_font())
    }

    pub const NERD_FONT_SHELL_ROUTE_ICONS: [&'static str; 7] = [
        "\u{f009}", // grid — hub home
        "\u{f135}", // rocket — applications
        "\u{f0f3}", // bell — notifications
        "\u{f0ea}", // clipboard
        "\u{f073}", // calendar
        "\u{f03e}", // image — wallpaper
        "\u{f1fc}", // paint-brush — theme
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PresentationVisibility {
    pub client_name: bool,
    /// Desktop icon of the focused window's application, drawn beside its
    /// title. Follows `client_name`: without the title there is nothing for
    /// the icon to label.
    pub client_icon: bool,
    /// macOS-style shelf containing windows minimized by the WM.
    pub minimized_windows: bool,
    pub monitor: bool,
    pub system: bool,
    pub audio: bool,
    pub brightness: bool,
    pub battery: bool,
    pub network: bool,
    pub media: bool,
    pub theme: bool,
    pub screenshot: bool,
    pub clock: bool,
    /// Entry points into the window manager's shell surface. On by default:
    /// a window manager that does not answer the shell command also does not
    /// hold the transport open, and `wm_available` already grays the entry out
    /// in that case, so there is no dead-button risk to guard against.
    pub shell_hub: bool,
}

impl Default for PresentationVisibility {
    fn default() -> Self {
        Self {
            client_name: true,
            client_icon: true,
            minimized_windows: true,
            monitor: false,
            system: true,
            audio: true,
            brightness: true,
            battery: true,
            network: true,
            media: true,
            theme: true,
            screenshot: true,
            clock: true,
            shell_hub: true,
        }
    }
}

/// Convert an authoritative physical bar height into logical presentation units.
///
/// Window-system scale factors should be positive and finite, but validating
/// the boundary keeps a malformed platform value out of the long-lived
/// presentation configuration. Results that cannot remain positive and finite
/// after conversion to the configuration's `f32` representation are rejected.
#[must_use]
pub fn logical_bar_height(physical_height: u32, scale_factor: f64) -> Option<f32> {
    if physical_height == 0 || !scale_factor.is_finite() || scale_factor <= 0.0 {
        return None;
    }

    let logical_height = f64::from(physical_height) / scale_factor;
    if !logical_height.is_finite() || logical_height <= 0.0 || logical_height > f64::from(f32::MAX)
    {
        return None;
    }

    let logical_height = logical_height as f32;
    (logical_height.is_finite() && logical_height > 0.0).then_some(logical_height)
}

/// Owned visual configuration. Tag labels and layouts are deliberately
/// dynamic; missing tag labels fall back to one-based numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PresentationConfig {
    /// Output edge. Sidebars use upright, compact cells along the vertical axis.
    pub edge: crate::placement::BarEdge,
    /// Logical thickness: height on top/bottom, width on left/right.
    pub bar_height: f32,
    /// Optional authoritative physical thickness supplied by the WM. This is
    /// converted exactly once by native frontends when the output scale changes.
    pub physical_thickness: Option<u32>,
    pub horizontal_padding: f32,
    pub vertical_padding: f32,
    pub item_gap: f32,
    pub pill_horizontal_padding: f32,
    pub corner_radius: f32,
    pub font_size: f32,
    pub minimum_visible_width: f32,
    /// Resting height for minimized-window thumbnail cards.
    pub dock_item_size: f32,
    /// Window-like thumbnail width divided by height.
    pub dock_item_aspect_ratio: f32,
    /// Gap between fixed Dock slots. Slot size reserves peak magnification so
    /// client/status content never jumps while the pointer moves.
    pub dock_item_gap: f32,
    pub dock_shelf_padding: f32,
    pub dock_corner_radius: f32,
    /// Peak scale of the card directly under the pointer.
    pub dock_hover_scale: f32,
    /// Logical distance along the bar's main axis over which neighbouring cards magnify.
    pub dock_influence_radius: f32,
    pub dock_separator_width: f32,
    /// Preferred leading fraction reserved for workspaces/layout before status items.
    pub left_fraction: f32,
    /// Labels are looked up only for the model-bounded visible tag indices;
    /// unused entries and source-vector spare capacity are never projected.
    pub tag_labels: Vec<String>,
    /// Tag-only contrast policy; does not alter the bar background.
    pub tag_contrast: TagContrastMode,
    /// Ordered layout catalog. Projection inspects at most
    /// [`crate::MAX_PROJECTED_CONFIG_ITEMS`] entries and keeps the first entry
    /// for each repeated layout identifier.
    pub layouts: Vec<LayoutChoice>,
    pub labels: PresentationLabels,
    /// Font family that backs private-use icon glyphs, when the host wants a
    /// specific one. `None` lets the renderer pick an installed patched font
    /// itself — see [`crate::icon_font`] for why leaving it to the generic
    /// font fallback is not an option.
    pub icon_font: Option<String>,
    /// Optional band-aware/dynamic icon preset for widget and scene
    /// projections. `labels` remains the fallback and customization surface.
    pub icon_set: Option<IconSet>,
    /// Renderer-independent CPU/memory severity policy.
    pub usage_thresholds: UsageThresholds,
    /// Renderer-independent remaining-battery severity policy.
    pub battery_thresholds: BatteryThresholds,
    /// Renderer-independent audio icon band policy.
    pub volume_thresholds: VolumeThresholds,
    /// Shell entry points to project, in left-to-right order, when
    /// [`PresentationVisibility::shell_hub`] is set. Repeated routes project
    /// once at their first configured position so every control keeps a unique
    /// stable [`NodeId`]. Projection inspects at most
    /// [`crate::MAX_PROJECTED_CONFIG_ITEMS`] entries.
    pub shell_routes: Vec<ShellRoute>,
    pub visibility: PresentationVisibility,
}

impl Default for PresentationConfig {
    fn default() -> Self {
        Self {
            edge: crate::placement::BarEdge::Top,
            bar_height: 42.0,
            physical_thickness: None,
            horizontal_padding: 10.0,
            vertical_padding: 6.0,
            item_gap: 6.0,
            pill_horizontal_padding: 10.0,
            corner_radius: 12.0,
            font_size: 13.0,
            minimum_visible_width: 8.0,
            dock_item_size: 18.0,
            dock_item_aspect_ratio: 1.5,
            dock_item_gap: 4.0,
            dock_shelf_padding: 2.0,
            dock_corner_radius: 6.0,
            dock_hover_scale: 1.55,
            dock_influence_radius: 52.0,
            dock_separator_width: 1.0,
            left_fraction: 0.42,
            tag_contrast: TagContrastMode::default(),
            tag_labels: ["🖥", "🌐", "📁", "💬", "📝", "🎵", "⚙", "📊", "🏠"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            layouts: crate::display::CANONICAL_LAYOUTS
                .into_iter()
                .map(|layout| LayoutChoice {
                    id: layout.id,
                    label: layout.symbol.to_owned(),
                })
                .collect(),
            labels: PresentationLabels::default(),
            icon_font: None,
            icon_set: None,
            usage_thresholds: UsageThresholds::default(),
            battery_thresholds: BatteryThresholds::default(),
            volume_thresholds: VolumeThresholds::default(),
            // One cell by default. It opens the hub home, and its scroll
            // bindings reach every other page, so the common case costs a
            // single pill of bar width.
            shell_routes: vec![ShellRoute::Hub],
            visibility: PresentationVisibility::default(),
        }
    }
}

impl PresentationConfig {
    /// Every icon a percentage control can display, independently of its
    /// current reading. Renderers reserve the widest candidate so mute,
    /// charging and unavailable transitions cannot resize a numeric cell.
    #[must_use]
    pub fn metric_icon_candidates(&self, id: NodeId) -> Option<Vec<&str>> {
        let labels = &self.labels;
        Some(match id {
            NodeId::Cpu => vec![labels.cpu.as_str()],
            NodeId::Memory => vec![labels.memory.as_str()],
            NodeId::Brightness => vec![labels.brightness.as_str()],
            NodeId::Battery => self.icon_set.as_ref().map_or_else(
                || vec![labels.battery.as_str(), labels.charging.as_str()],
                |icons| {
                    vec![
                        icons.battery.as_str(),
                        icons.battery_charging.as_str(),
                        icons.unavailable.as_str(),
                    ]
                },
            ),
            NodeId::Audio => self.icon_set.as_ref().map_or_else(
                || vec![labels.audio.as_str(), labels.muted.as_str()],
                |icons| {
                    vec![
                        icons.volume_unavailable.as_str(),
                        icons.volume_muted.as_str(),
                        icons.volume_low.as_str(),
                        icons.volume_medium.as_str(),
                        icons.volume_high.as_str(),
                    ]
                },
            ),
            _ => return None,
        })
    }

    /// The native surface's thickness in logical units. WM pixels are already
    /// physical; scaling them twice would reserve and paint different strips.
    #[must_use]
    pub fn logical_thickness(&self, scale_factor: f64) -> f64 {
        let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        self.physical_thickness
            .filter(|pixels| *pixels > 0)
            .map_or(f64::from(self.bar_height), |pixels| {
                f64::from(pixels) / scale
            })
    }

    /// Replace semantic labels and dynamic tag labels from one shared icon
    /// preset while preserving geometry, visibility, and threshold policy.
    /// Derived visual strings are bounded before they fan out into these
    /// collections; `icon_set` retains an exact clone for dynamic lookups and
    /// public configuration inspection.
    pub fn apply_icon_set(&mut self, icons: &IconSet) {
        self.tag_labels = (0..MAX_MODEL_TAGS)
            .map(|index| bounded_cow_control_text(icons.tag_icon(index)))
            .collect();
        self.labels = PresentationLabels::bounded_from_icon_set(icons);
        self.icon_set = Some(icons.clone());
    }

    #[must_use]
    pub fn with_icon_set(mut self, icons: &IconSet) -> Self {
        self.apply_icon_set(icons);
        self
    }

    /// Trade the stock emoji preset for the Nerd Font one, tag glyphs included.
    ///
    /// Monochrome glyphs tinted by the text colour read like macOS template
    /// icons, which is what every bar wants; the emoji defaults exist so a host
    /// with no patched font still shows something recognizable. Bars used to
    /// swap [`PresentationLabels`] alone, which left `tag_labels` emoji and put
    /// two icon vocabularies on the same bar — Nerd Font on the right, emoji in
    /// the tag pills.
    ///
    /// The two halves are upgraded independently. A config that customizes
    /// tag labels still gets Nerd Font status icons, while a config that
    /// customizes status labels still gets Nerd Font tags. This matters for
    /// older configs, which commonly supplied only `tag_labels`: treating the
    /// preset as all-or-nothing would leave the untouched status half as
    /// colorful emoji. Returns whether either stock half was replaced.
    pub fn apply_nerd_font_icons_if_stock(&mut self) -> bool {
        let stock = Self::default();
        let replace_labels = self.labels == stock.labels && self.icon_set.is_none();
        let replace_tags = self.tag_labels == stock.tag_labels;
        if !replace_labels && !replace_tags {
            return false;
        }

        let icons = IconSet::nerd_font();
        if replace_labels {
            self.labels = PresentationLabels::bounded_from_icon_set(&icons);
            // Dynamic battery and volume bands are part of the status preset,
            // so activate them only when that half was still stock.
            self.icon_set = Some(icons.clone());
        }
        if replace_tags {
            self.tag_labels = (0..MAX_MODEL_TAGS)
                .map(|index| bounded_cow_control_text(icons.tag_icon(index)))
                .collect();
        }
        true
    }
}

/// Text metrics supplied by the active renderer/font stack.
pub trait TextMeasurer {
    fn measure(&self, text: &str, size: f32) -> Size;
}

/// Deterministic fallback useful for headless operation and tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ApproximateTextMeasurer {
    pub width_factor: f32,
}

impl Default for ApproximateTextMeasurer {
    fn default() -> Self {
        Self { width_factor: 0.58 }
    }
}

impl TextMeasurer for ApproximateTextMeasurer {
    fn measure(&self, text: &str, size: f32) -> Size {
        let count = text.chars().count() as f32;
        Size::new(
            count * finite_non_negative(size) * self.width_factor.max(0.0),
            finite_non_negative(size),
        )
    }
}

/// Pure layout engine producing the same scene and hit map for every backend.
#[derive(Debug, Clone)]
pub struct LayoutEngine<M> {
    config: PresentationConfig,
    measurer: M,
}

struct LayoutPass<'a, M> {
    config: &'a PresentationConfig,
    measurer: &'a M,
    network_metrics: std::cell::OnceCell<NetworkTextMetrics>,
    percent_metrics: [std::cell::OnceCell<PercentTextMetrics>; 5],
}

/// Stable icon and right-aligned value slots for the 0..=100% controls.
/// The dimensions follow the actual font; no live reading or assumed
/// monospace advance participates in allocation.
#[derive(Debug, Clone, Copy)]
pub struct PercentTextMetrics {
    pub icon_width: f32,
    pub value_width: f32,
    pub gap: f32,
}

impl PercentTextMetrics {
    #[must_use]
    pub fn measure(icons: &[&str], mut measure: impl FnMut(&str) -> f32) -> Self {
        let mut width = |text: &str| finite_non_negative(measure(text));
        let icon_width = icons
            .iter()
            .map(|icon| width(&bounded_borrowed_control_text(icon)))
            .fold(0.0_f32, f32::max);
        let digit_width = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]
            .into_iter()
            .map(&mut width)
            .fold(0.0_f32, f32::max);
        let value_width = (digit_width * 3.0 + width("%"))
            .max(width("100%"))
            .max(width(crate::controls::UNKNOWN_VALUE));
        let gap = if icon_width > 0.0 { width("  ") } else { 0.0 };
        Self {
            icon_width,
            value_width,
            gap,
        }
    }

    #[must_use]
    pub fn width(self) -> f32 {
        self.icon_width + self.gap + self.value_width
    }
}

fn percent_metric_index(id: NodeId) -> Option<usize> {
    match id {
        NodeId::Cpu => Some(0),
        NodeId::Memory => Some(1),
        NodeId::Brightness => Some(2),
        NodeId::Battery => Some(3),
        NodeId::Audio => Some(4),
        _ => None,
    }
}

/// Font-derived, stable horizontal network columns shared by scene and
/// widget renderers. Live rates never participate in width calculation.
#[derive(Debug, Clone, Copy)]
pub struct NetworkTextMetrics {
    pub icon_width: f32,
    pub arrow_width: f32,
    pub rate_width: f32,
    pub gap: f32,
}

impl NetworkTextMetrics {
    #[must_use]
    pub fn measure(
        online_icon: &str,
        offline_icon: &str,
        mut measure: impl FnMut(&str) -> f32,
    ) -> Self {
        let mut width = |text: &str| finite_non_negative(measure(text));
        let icon_width = width(&bounded_borrowed_control_text(online_icon))
            .max(width(&bounded_borrowed_control_text(offline_icon)));
        let arrow_width = width("\u{2193}").max(width("\u{2191}"));
        let gap = width(" ");
        let digit_width = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]
            .into_iter()
            .map(&mut width)
            .fold(0.0_f32, f32::max);
        let unit_width = ["B/s", "KiB/s", "MiB/s", "GiB/s", "TiB/s", "PiB/s", "EiB/s"]
            .into_iter()
            .map(&mut width)
            .fold(0.0_f32, f32::max);
        // format_transfer_rate can round a value just below a unit boundary
        // to 1024.0, so reserve five digits plus the decimal point. Measure
        // all digits/units instead of assuming a monospace font or that 8/M
        // are the widest glyphs. Unknown readings use these same columns.
        let rate_width = (digit_width * 5.0 + width(".") + gap + unit_width)
            .max(width(crate::controls::UNKNOWN_VALUE));
        Self {
            icon_width,
            arrow_width,
            rate_width,
            gap,
        }
    }

    #[must_use]
    pub fn width(self) -> f32 {
        self.icon_width
            + if self.icon_width > 0.0 {
                self.gap * 2.0
            } else {
                0.0
            }
            + 2.0 * (self.arrow_width + self.rate_width)
            + self.gap
    }
}

/// One frame's geometry and paint context shared by the two control groups.
/// Each group selects only its own main-axis interval and content priority.
#[derive(Clone, Copy)]
struct ControlGroupContext<'a> {
    axis: BarAxis,
    cross: f32,
    cross_size: f32,
    gap: f32,
    interaction: &'a InteractionState,
    palette: Palette,
    active_tag: Option<TagId>,
}

/// Coordinate vocabulary shared by layout and Dock reporting. Only rectangles
/// and pointer distances change axes: glyphs and image pixels stay upright.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BarAxis {
    pub(crate) vertical: bool,
    pub(crate) bounds: Rect,
}

impl BarAxis {
    pub(crate) fn new(edge: crate::placement::BarEdge, viewport: Size, thickness: f32) -> Self {
        use crate::placement::BarEdge;
        let thickness = finite_non_negative(thickness);
        let bounds = match edge {
            BarEdge::Top => Rect::new(0.0, 0.0, viewport.width, thickness.min(viewport.height)),
            BarEdge::Bottom => {
                let height = thickness.min(viewport.height);
                Rect::new(0.0, viewport.height - height, viewport.width, height)
            }
            BarEdge::Left => Rect::new(0.0, 0.0, thickness.min(viewport.width), viewport.height),
            BarEdge::Right => {
                let width = thickness.min(viewport.width);
                Rect::new(viewport.width - width, 0.0, width, viewport.height)
            }
        };
        Self {
            vertical: edge.is_vertical(),
            bounds,
        }
    }

    pub(crate) const fn from_rect(edge: crate::placement::BarEdge, bounds: Rect) -> Self {
        Self {
            vertical: edge.is_vertical(),
            bounds,
        }
    }

    pub(crate) fn main_start(self) -> f32 {
        if self.vertical {
            self.bounds.y
        } else {
            self.bounds.x
        }
    }
    pub(crate) fn cross_start(self) -> f32 {
        if self.vertical {
            self.bounds.x
        } else {
            self.bounds.y
        }
    }
    pub(crate) fn main_len(self) -> f32 {
        if self.vertical {
            self.bounds.height
        } else {
            self.bounds.width
        }
    }
    pub(crate) fn cross_len(self) -> f32 {
        if self.vertical {
            self.bounds.width
        } else {
            self.bounds.height
        }
    }
    pub(crate) fn main_end(self) -> f32 {
        self.main_start() + self.main_len()
    }
    pub(crate) fn main(self, point: Point) -> f32 {
        if self.vertical { point.y } else { point.x }
    }
    pub(crate) fn rect(self, main: f32, cross: f32, extent: f32, thickness: f32) -> Rect {
        if self.vertical {
            Rect::new(cross, main, thickness, extent)
        } else {
            Rect::new(main, cross, extent, thickness)
        }
    }
    pub(crate) fn point(self, main: f32, cross: f32) -> Point {
        if self.vertical {
            Point::new(cross, main)
        } else {
            Point::new(main, cross)
        }
    }
}

/// Shared resting-card/slot metrics. A vertical shelf keeps landscape cards;
/// its capacity and magnification operate on their *height*, never a rotated
/// horizontal strip. Reporting and drawing use this exact calculation.
#[derive(Debug, Clone, Copy)]
pub struct DockMetrics {
    pub padding: f32,
    pub gap: f32,
    pub separator_width: f32,
    pub item_height: f32,
    pub item_width: f32,
    pub peak_scale: f32,
    pub slot_extent: f32,
    pub slot_pitch: f32,
    fixed_extent: f32,
    vertical: bool,
}

impl DockMetrics {
    pub fn new(config: &PresentationConfig, available_cross: f32) -> Option<Self> {
        let padding = finite_non_negative(config.dock_shelf_padding);
        let gap = finite_non_negative(config.dock_item_gap);
        let separator_width = finite_non_negative(config.dock_separator_width);
        let usable_cross = (finite_non_negative(available_cross) - padding * 2.0).max(0.0);
        let aspect_ratio = if config.dock_item_aspect_ratio.is_finite() {
            config.dock_item_aspect_ratio.clamp(1.0, 4.0)
        } else {
            1.0
        };
        let vertical = config.edge.is_vertical();
        let item_height = finite_non_negative(config.dock_item_size).min(if vertical {
            usable_cross / aspect_ratio
        } else {
            usable_cross
        });
        if item_height < 1.0 {
            return None;
        }
        let item_width = item_height * aspect_ratio;
        let configured_scale = if config.dock_hover_scale.is_finite() {
            config.dock_hover_scale.clamp(1.0, 3.0)
        } else {
            1.0
        };
        let item_cross = if vertical { item_width } else { item_height };
        let item_main = if vertical { item_height } else { item_width };
        let peak_scale = configured_scale.min((usable_cross / item_cross).max(1.0));
        // Magnification changes pixels, never the acquisition target. A full
        // 32px slot also makes small resting thumbnails easy to restore.
        let slot_extent = (item_main * peak_scale).max(32.0);
        let slot_pitch = slot_extent + gap;
        Some(Self {
            padding,
            gap,
            separator_width,
            item_height,
            item_width,
            peak_scale,
            slot_extent,
            slot_pitch,
            fixed_extent: padding * 3.0 + separator_width,
            vertical,
        })
    }

    /// Main-axis extent for the separator and one fixed acquisition slot.
    #[must_use]
    pub fn minimum_extent(self) -> f32 {
        self.minimum_width()
    }

    /// Main-axis extent for exactly `count` fixed slots, including chrome.
    #[must_use]
    pub fn extent_for_slots(self, count: usize) -> f32 {
        self.width_for_slots(count)
    }

    pub(crate) fn minimum_width(self) -> f32 {
        self.fixed_extent + self.slot_extent
    }
    pub fn slot_capacity(self, available: f32) -> usize {
        // Opposite-end subtraction can lose a few ULPs on long outputs;
        // an exactly reserved shelf must not unexpectedly drop its last slot.
        let tolerance = 0.001;
        if available + tolerance < self.minimum_width() {
            return 0;
        }
        1_usize.saturating_add(
            ((available - self.minimum_width() + tolerance) / self.slot_pitch)
                .floor()
                .max(0.0) as usize,
        )
    }
    pub(crate) fn width_for_slots(self, count: usize) -> f32 {
        if count == 0 {
            return 0.0;
        }
        self.fixed_extent
            + count as f32 * self.slot_extent
            + count.saturating_sub(1) as f32 * self.gap
    }
    pub(crate) fn slot_start(self, axis: BarAxis) -> f32 {
        axis.main_start() + self.padding * 2.0 + self.separator_width
    }
    pub(crate) fn card_rect(self, axis: BarAxis, slot: usize, scale: f32) -> Rect {
        let center = self.slot_start(axis) + slot as f32 * self.slot_pitch + self.slot_extent * 0.5;
        let width = self.item_width * scale;
        let height = self.item_height * scale;
        let (main, cross) = if self.vertical {
            (height, width)
        } else {
            (width, height)
        };
        axis.rect(
            center - main * 0.5,
            axis.cross_start() + (axis.cross_len() - cross) * 0.5,
            main,
            cross,
        )
    }
    pub(crate) fn slot_rect(self, axis: BarAxis, slot: usize) -> Rect {
        axis.rect(
            self.slot_start(axis) + slot as f32 * self.slot_pitch,
            axis.cross_start(),
            self.slot_extent,
            axis.cross_len(),
        )
    }
}

impl<M> LayoutEngine<M> {
    #[must_use]
    pub const fn new(config: PresentationConfig, measurer: M) -> Self {
        Self { config, measurer }
    }

    #[must_use]
    pub const fn config(&self) -> &PresentationConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut PresentationConfig {
        &mut self.config
    }

    #[must_use]
    pub fn into_parts(self) -> (PresentationConfig, M) {
        (self.config, self.measurer)
    }
}

impl<M: TextMeasurer> LayoutEngine<M> {
    #[must_use]
    pub fn build(
        &self,
        view: BarView<'_>,
        viewport: Size,
        interaction: &InteractionState,
    ) -> Scene {
        build_scene(&self.config, &self.measurer, view, viewport, interaction)
    }
}

pub(crate) fn build_scene<M: TextMeasurer>(
    config: &PresentationConfig,
    measurer: &M,
    view: BarView<'_>,
    viewport: Size,
    interaction: &InteractionState,
) -> Scene {
    LayoutPass {
        config,
        measurer,
        network_metrics: std::cell::OnceCell::new(),
        percent_metrics: std::array::from_fn(|_| std::cell::OnceCell::new()),
    }
    .build(view, viewport, interaction)
}

impl<M: TextMeasurer> LayoutPass<'_, M> {
    fn build(&self, view: BarView<'_>, viewport: Size, interaction: &InteractionState) -> Scene {
        let BarPresentation {
            theme,
            tags,
            layout_button,
            layout_choices,
            client_name,
            client_icon,
            minimized_windows,
            minimized_overflow,
            status,
        } = PresentationProjector::project(view, self.config);
        let viewport = viewport.normalized();
        let axis = BarAxis::new(self.config.edge, viewport, self.config.bar_height);
        let bar = axis.bounds;
        let palette = Palette::for_theme(theme);
        let tag_palette =
            TagPalette::for_wallpaper(theme, view.wallpaper_luminance, self.config.tag_contrast);
        let mut scene = Scene {
            viewport,
            clip: Rect::from_size(viewport),
            nodes: Vec::new(),
            hits: Vec::new(),
        };
        if bar.is_empty() {
            return scene;
        }
        scene.nodes.push(SceneNode::Background {
            id: NodeId::Background,
            bounds: bar,
            fill: palette.background,
        });

        let target = self.minimum_target_extent();
        // Decorative insets yield before the final usable workspace target.
        // A short output may have exactly one target's worth of room.
        let main_padding = finite_non_negative(self.config.horizontal_padding)
            .min((axis.main_len() - target).max(0.0) * 0.5);
        // Managed thickness is physical: 42 px at 2x leaves only 21 logical
        // pixels. Fixed outer padding plus pill padding used to consume the
        // entire sidebar label. Preserve room for an icon/two clock digits
        // before allocating decorative cross-axis spacing.
        let cross_padding_limit = if axis.vertical {
            (axis.cross_len() - self.font_size() * 2.0).max(0.0) * 0.5
        } else {
            (axis.cross_len() - self.font_size()).max(0.0) * 0.5
        };
        let cross_padding =
            finite_non_negative(self.config.vertical_padding).min(cross_padding_limit);
        let cross_size = (axis.cross_len() - cross_padding * 2.0).max(0.0);
        if cross_size <= 0.0 {
            return scene;
        }
        let cross = axis.cross_start() + cross_padding;
        let gap = finite_non_negative(self.config.item_gap);
        let start = axis.main_start() + main_padding;
        let end = (axis.main_end() - main_padding).max(start);
        let total = end - start;
        if total < target {
            return scene;
        }
        let left_fraction = if self.config.left_fraction.is_finite() {
            self.config.left_fraction.clamp(0.2, 0.8)
        } else {
            0.42
        };
        let layout_extent = self.control_extent(&layout_button, true);
        let tag_ideal: f32 = tags
            .iter()
            .map(|control| self.control_extent(control, false) + gap)
            .sum();
        let menu_ideal: f32 = layout_choices
            .iter()
            .map(|control| self.control_extent(control, true) + gap)
            .sum();
        let leading_ideal = (tag_ideal + layout_extent + menu_ideal).max(target);
        let leading_minimum = if tags.is_empty() {
            layout_extent
        } else {
            target * 2.0 + gap
        };
        let mut leading_budget = leading_ideal.min((total * left_fraction).max(leading_minimum));
        // Keep the clock and a workspace usable on a very short output. A
        // missing layout-menu entry remains available by wheel on its button.
        if !status.is_empty() {
            leading_budget = leading_budget.min((total - target - gap).max(target));
        }
        leading_budget = leading_budget.min(total);
        let trailing_budget = (total - leading_budget - gap).max(0.0);
        let dock_enabled = self.config.visibility.minimized_windows && view.wm_available;
        let metrics = dock_enabled
            .then(|| DockMetrics::new(self.config, axis.cross_len()))
            .flatten();
        let critical_status = status.len().min(3) as f32 * (target + gap);
        let dock_budget = metrics.map_or(0.0, |metrics| {
            let allowance = if status.is_empty() {
                trailing_budget
            } else {
                (trailing_budget - critical_status)
                    .max(0.0)
                    .min(trailing_budget * 0.38)
            };
            if metrics.slot_capacity(allowance) == 0 {
                return 0.0;
            }
            let count = minimized_windows.len()
                + usize::from(minimized_overflow || minimized_windows.is_empty());
            metrics.width_for_slots(count).min(allowance)
        });
        let status_budget =
            (trailing_budget - dock_budget - if dock_budget > 0.0 { gap } else { 0.0 }).max(0.0);
        let group_context = ControlGroupContext {
            axis,
            cross,
            cross_size,
            gap,
            interaction,
            palette,
            active_tag: view.active_tag,
        };
        let mut trailing = self.push_status_group(
            &mut scene,
            status,
            status_budget,
            end,
            group_context,
            view.wm_available,
        );

        let dock_bounds = if dock_budget > 0.0 {
            let available = axis.rect(
                (trailing - dock_budget).max(start + leading_budget),
                axis.cross_start(),
                dock_budget.min((trailing - start - leading_budget).max(0.0)),
                axis.cross_len(),
            );
            let result = self.push_dock(
                &mut scene,
                available,
                minimized_windows,
                minimized_overflow,
                interaction,
                palette,
            );
            if let Some(bounds) = result {
                trailing = BarAxis::from_rect(self.config.edge, bounds).main_start() - gap;
            }
            result
        } else {
            None
        };

        let leading_limit = (start + leading_budget).min(trailing).max(start);
        let room = leading_limit - start;
        let layout_reserved = if tags.is_empty() || room >= target + gap + layout_extent {
            layout_extent
        } else {
            0.0
        };
        let tag_budget = (room
            - layout_reserved
            - if layout_reserved > 0.0 && !tags.is_empty() {
                gap
            } else {
                0.0
            })
        .max(0.0);
        let mut leading = self.push_workspace_group(
            &mut scene,
            tags,
            tag_budget,
            start,
            group_context,
            tag_palette,
        );
        if layout_reserved > 0.0 {
            let bounds = axis.rect(
                leading,
                cross,
                layout_extent.min((leading_limit - leading).max(0.0)),
                cross_size,
            );
            self.push_pill(
                &mut scene,
                bounds,
                self.pill_spec(layout_button, true),
                interaction,
                palette,
                Some(bar),
            );
            leading += layout_extent + gap;
        }

        // A wide output can expand the layout chooser into the spare centre.
        // A sidebar keeps these labels upright; wheel navigation on the main
        // button still reaches every offered layout if the centre is crowded.
        for control in layout_choices {
            let extent = self.control_extent(&control, true);
            if trailing - leading < extent {
                break;
            }
            self.push_pill(
                &mut scene,
                axis.rect(leading, cross, extent, cross_size),
                self.pill_spec(control, true),
                interaction,
                palette,
                Some(bar),
            );
            leading += extent + gap;
        }
        let dock_caption_visible = dock_bounds.is_some()
            && matches!(interaction.hovered(), Some(NodeId::MinimizedWindow(_)));
        // Hovering a minimized application temporarily owns the title slot.
        // Painting both labels in that same centre made the two names run
        // together and partially covered the focused application's title.
        if let Some(client_name) = client_name.filter(|_| !dock_caption_visible) {
            let available = trailing - leading;
            if axis.vertical {
                // The desktop icon is useful on a narrow sidebar; a vertical
                // ribbon of truncated title characters is not. The complete
                // title remains available to the frontend's native tooltip.
                if let Some(icon) = client_icon.as_ref()
                    && available >= target
                {
                    let size = cross_size.min(target);
                    scene.nodes.push(SceneNode::Image {
                        id: NodeId::ClientIcon,
                        bounds: Rect::new(
                            cross + (cross_size - size) * 0.5,
                            leading + (available - size) * 0.5,
                            size,
                            size,
                        ),
                        source: ImageSource::from(icon),
                        state: VisualState::default(),
                    });
                }
            } else if available >= self.minimum_visible_width() {
                self.push_client_title(
                    &mut scene,
                    axis.rect(leading, cross, available, cross_size),
                    &client_name.value,
                    client_icon.as_ref(),
                    palette,
                );
            }
        }
        if let Some(bounds) = dock_bounds {
            self.push_dock_hover_title(&mut scene, bounds, view, interaction, leading, gap);
        }
        scene
    }

    fn minimum_target_extent(&self) -> f32 {
        32.0_f32.max(self.font_size() * 1.8)
    }

    fn network_text_metrics(&self) -> NetworkTextMetrics {
        *self.network_metrics.get_or_init(|| {
            NetworkTextMetrics::measure(
                &self.config.labels.network,
                &self.config.labels.network_offline,
                |text| self.measurer.measure(text, self.font_size()).width,
            )
        })
    }

    fn percent_text_metrics(&self, id: NodeId) -> Option<PercentTextMetrics> {
        let index = percent_metric_index(id)?;
        Some(*self.percent_metrics[index].get_or_init(|| {
            let icons = self.config.metric_icon_candidates(id).unwrap_or_default();
            PercentTextMetrics::measure(&icons, |text| {
                self.measurer.measure(text, self.font_size()).width
            })
        }))
    }

    fn control_extent(&self, control: &ControlSpec, compact: bool) -> f32 {
        if self.config.edge.is_vertical() {
            let lines = self.percent_text_metrics(control.id).map_or_else(
                || control.sidebar_lines().len().max(1) as f32,
                |metrics| if metrics.icon_width > 0.0 { 2.0 } else { 1.0 },
            );
            (lines * (self.font_size() + 2.0) + 8.0).max(self.minimum_target_extent())
        } else {
            if let Some(metrics) = self.percent_text_metrics(control.id) {
                let (width, padding) = if compact {
                    let icon_can_be_empty = self
                        .config
                        .metric_icon_candidates(control.id)
                        .is_some_and(|icons| icons.iter().any(|icon| icon.is_empty()));
                    (
                        if icon_can_be_empty {
                            metrics.icon_width.max(metrics.value_width)
                        } else {
                            metrics.icon_width
                        },
                        self.horizontal_text_padding().min(4.0),
                    )
                } else {
                    (metrics.width(), self.horizontal_text_padding())
                };
                // A full reading either fits in its reserved slot or remains
                // compact. Never let the generic text cap clip 100% at large
                // font sizes or change the overflow decision with each sample.
                return (width + padding * 2.0).max(self.minimum_target_extent());
            }
            if control.id == NodeId::Network {
                let metrics = self.network_text_metrics();
                if !compact {
                    // Full readings either fit as a whole or stay compact.
                    // Do not cap at the generic 240px limit: larger fonts
                    // still need room for both complete rate columns.
                    return (metrics.width() + self.horizontal_text_padding() * 2.0)
                        .max(self.minimum_target_extent());
                }
                let mut width = metrics.icon_width;
                if self.config.labels.network.is_empty()
                    || self.config.labels.network_offline.is_empty()
                {
                    width = width.max(
                        self.measurer
                            .measure("\u{2193}\u{2191}", self.font_size())
                            .width,
                    );
                }
                return (width + self.horizontal_text_padding().min(4.0) * 2.0)
                    .max(self.minimum_target_extent())
                    .min(240.0);
            }
            let text = if compact {
                control.compact_value()
            } else {
                control.text()
            };
            let padding = if compact {
                self.horizontal_text_padding().min(4.0)
            } else {
                self.horizontal_text_padding()
            };
            (self.measurer.measure(&text, self.font_size()).width + padding * 2.0)
                .max(self.minimum_target_extent())
                .min(240.0)
        }
    }

    fn pill_spec(&self, control: ControlSpec, compact: bool) -> PillSpec {
        let lines = if self.config.edge.is_vertical() {
            if let Some(metrics) = self.percent_text_metrics(control.id) {
                if metrics.icon_width > 0.0 {
                    // Keep the row even when one icon candidate is empty.
                    vec![control.icon.clone(), control.value.clone()]
                } else {
                    vec![control.value.clone()]
                }
            } else {
                control.sidebar_lines()
            }
        } else {
            Vec::new()
        };
        let text = if compact && !self.config.edge.is_vertical() {
            control.compact_value()
        } else {
            control.text()
        };
        PillSpec {
            text,
            lines,
            compact,
            ..PillSpec::from(control)
        }
    }

    fn push_status_group(
        &self,
        scene: &mut Scene,
        controls: Vec<ControlSpec>,
        budget: f32,
        end: f32,
        context: ControlGroupContext<'_>,
        wm_available: bool,
    ) -> f32 {
        let ControlGroupContext {
            axis,
            cross,
            cross_size,
            gap,
            interaction,
            palette,
            active_tag: _,
        } = context;
        if controls.is_empty() || budget < self.minimum_target_extent() {
            return end;
        }
        let target = self.minimum_target_extent();
        let compact_total: f32 = controls
            .iter()
            .map(|control| self.control_extent(control, true) + gap)
            .sum::<f32>()
            - gap;
        let overflow = compact_total > budget && wm_available && budget >= target * 2.0 + gap;
        let mut remaining = budget - if overflow { target + gap } else { 0.0 };
        let mut order: Vec<_> = (0..controls.len()).collect();
        order.sort_by_key(|index| status_priority(&controls[*index]));
        let mut extents = vec![0.0; controls.len()];
        let mut compact = vec![true; controls.len()];
        for &index in &order {
            if overflow && matches!(controls[index].id, NodeId::ShellHub(_)) {
                continue;
            }
            let preferred = self.control_extent(&controls[index], true);
            if remaining < target {
                continue;
            }
            let extent = preferred.min(remaining);
            extents[index] = extent;
            remaining = (remaining - extent - gap).max(0.0);
        }
        // Spend spare room on actual readings before expanding the clock's
        // date. On wide screens this reproduces the full horizontal content.
        order.sort_by_key(|index| {
            (
                controls[*index].id == NodeId::Clock,
                status_priority(&controls[*index]),
            )
        });
        if !axis.vertical {
            for index in order {
                if extents[index] == 0.0 {
                    continue;
                }
                let natural = self.control_extent(&controls[index], false);
                let extra = (natural - extents[index]).max(0.0);
                if extra <= remaining {
                    remaining -= extra;
                    extents[index] = natural;
                    compact[index] = false;
                }
            }
        }
        let hidden = extents.iter().filter(|extent| **extent == 0.0).count();
        let mut cursor = end;
        for (index, control) in controls.into_iter().enumerate() {
            let extent = extents[index];
            if extent == 0.0 {
                continue;
            }
            let bounds = axis.rect(cursor - extent, cross, extent, cross_size);
            self.push_pill(
                scene,
                bounds,
                self.pill_spec(control, compact[index]),
                interaction,
                palette,
                Some(axis.bounds),
            );
            cursor -= extent + gap;
        }
        if overflow && hidden > 0 {
            self.push_pill(
                scene,
                axis.rect(cursor - target, cross, target, cross_size),
                PillSpec {
                    id: NodeId::StatusOverflow,
                    text: "…".to_owned(),
                    lines: Vec::new(),
                    network: None,
                    percentage: None,
                    compact: true,
                    primary: Some(UserAction::OpenShellHub(ShellRoute::Hub)),
                    secondary: Some(UserAction::OpenAudioControl),
                    scroll_up: None,
                    scroll_down: None,
                    state: VisualState::default(),
                    progress: None,
                    tag_palette: None,
                },
                interaction,
                palette,
                Some(axis.bounds),
            );
            cursor -= target + gap;
        }
        cursor
    }

    fn push_workspace_group(
        &self,
        scene: &mut Scene,
        controls: Vec<ControlSpec>,
        budget: f32,
        start: f32,
        context: ControlGroupContext<'_>,
        tag_palette: TagPalette,
    ) -> f32 {
        let ControlGroupContext {
            axis,
            cross,
            cross_size,
            gap,
            interaction,
            palette,
            active_tag,
        } = context;
        let target = self.minimum_target_extent();
        if controls.is_empty() || budget < target {
            return start;
        }
        let natural: Vec<_> = controls
            .iter()
            .map(|control| self.control_extent(control, false).min(96.0))
            .collect();
        let full_width =
            natural.iter().sum::<f32>() + controls.len().saturating_sub(1) as f32 * gap;
        // Multiple tags can be selected at once. Their first selected index
        // is not necessarily the WM's active workspace, which must survive
        // even when this strip has room for just one target.
        let active = active_tag
            .and_then(|tag| {
                controls
                    .iter()
                    .position(|control| control.id == NodeId::Tag(tag))
            })
            .or_else(|| controls.iter().position(|control| control.state.selected))
            .unwrap_or(0);
        let mut selected = vec![false; controls.len()];
        // Try the complete compact strip before spending a target on
        // overflow. Long names alone must not hide otherwise usable tags.
        let compact_width =
            controls.len() as f32 * target + controls.len().saturating_sub(1) as f32 * gap;
        let overflow = compact_width > budget && budget >= target * 2.0 + gap;
        let mut remaining = budget - if overflow { target + gap } else { 0.0 };
        let mut order: Vec<_> = (0..controls.len()).collect();
        order.sort_by_key(|index| {
            let state = controls[*index].state;
            let is_active = *index == active && state.selected;
            (
                !is_active,
                !state.selected,
                !state.urgent,
                !state.occupied,
                index.abs_diff(active),
                *index,
            )
        });
        for index in order {
            let extent = if full_width <= budget {
                natural[index]
            } else {
                target
            };
            if extent <= remaining {
                selected[index] = true;
                remaining = (remaining - extent - gap).max(0.0);
            }
        }
        let missing = selected.iter().filter(|selected| !**selected).count();
        let hidden_state = controls
            .iter()
            .zip(&selected)
            .filter(|(_, visible)| !**visible)
            .fold(VisualState::default(), |mut state, (control, _)| {
                state.selected |= control.state.selected || control.state.filled;
                state.urgent |= control.state.urgent;
                state.occupied |= control.state.occupied;
                state
            });
        let previous = controls[active].bindings.scroll_up;
        let next = controls[active].bindings.scroll_down;
        let enabled = controls[active].state.enabled;
        let mut cursor = start;
        for (index, control) in controls.into_iter().enumerate() {
            if !selected[index] {
                continue;
            }
            let extent = if full_width <= budget {
                natural[index]
            } else {
                target
            };
            let bounds = axis.rect(cursor, cross, extent, cross_size);
            let mut spec = PillSpec {
                tag_palette: Some(tag_palette),
                ..self.pill_spec(control, full_width > budget)
            };
            if (spec.compact || axis.vertical)
                && let NodeId::Tag(id) = spec.id
            {
                // Names with the same prefix must not become identical
                // ellipses. Preserve readable short labels/icons; otherwise
                // show the stable workspace number. The semantic control
                // retains its complete configured name for tooltips.
                let padding = self.pill_text_padding(bounds.width, spec.compact);
                let minimum_size = self.font_size().min(bounds.height).min(9.0);
                if self.measurer.measure(&spec.text, minimum_size).width
                    > (bounds.width - padding * 2.0).max(0.0)
                {
                    spec.text = (usize::from(id) + 1).to_string();
                    spec.lines.clear();
                }
            }
            self.push_pill(scene, bounds, spec, interaction, palette, Some(axis.bounds));
            cursor += extent + gap;
        }
        if overflow && missing > 0 {
            self.push_pill(
                scene,
                axis.rect(cursor, cross, target, cross_size),
                PillSpec {
                    id: NodeId::WorkspaceOverflow,
                    // Keep hidden urgency perceptible without color alone.
                    text: format!("{}+{missing}", if hidden_state.urgent { "!" } else { "" }),
                    lines: Vec::new(),
                    network: None,
                    percentage: None,
                    compact: true,
                    primary: enabled.then_some(next).flatten(),
                    secondary: enabled.then_some(previous).flatten(),
                    scroll_up: enabled.then_some(previous).flatten(),
                    scroll_down: enabled.then_some(next).flatten(),
                    state: hidden_state,
                    progress: None,
                    tag_palette: Some(tag_palette),
                },
                interaction,
                palette,
                Some(axis.bounds),
            );
            cursor += target + gap;
        }
        cursor
    }

    /// The focused window's title, optionally preceded by its desktop icon.
    ///
    /// Icon and title are centred *as one group* rather than centring the text
    /// and hanging the icon off its left edge: the title is what the eye tracks
    /// across the bar, and a title that shifts sideways when an icon resolves
    /// (or fails to) reads as the bar twitching. The icon is dropped when the
    /// remaining width would leave the title too narrow to read, which keeps a
    /// crowded bar showing the more informative half.
    fn push_client_title(
        &self,
        scene: &mut Scene,
        bounds: Rect,
        title: &str,
        icon: Option<&crate::app_icon::AppIcon>,
        palette: Palette,
    ) {
        let gap = finite_non_negative(self.config.item_gap);
        let icon_size = icon
            .map(|_| self.client_icon_size(bounds.height))
            .filter(|size| {
                *size > 0.0
                    && bounds.width - (*size + gap)
                        >= self.minimum_visible_width().max(self.font_size() * 3.0)
            })
            .unwrap_or(0.0);
        let icon_advance = if icon_size > 0.0 {
            icon_size + gap
        } else {
            0.0
        };

        let text = self.fit_text(title, bounds.width - icon_advance, 0.0);
        if text.is_empty() {
            return;
        }
        let text_width = self
            .measurer
            .measure(&text, self.font_size())
            .width
            .min(bounds.width - icon_advance);
        let group_width = text_width + icon_advance;
        let group_left = bounds.x + ((bounds.width - group_width) * 0.5).max(0.0);

        if let (Some(icon), true) = (icon, icon_size > 0.0) {
            let top = bounds.y + ((bounds.height - icon_size) * 0.5).max(0.0);
            scene.nodes.push(SceneNode::Image {
                id: NodeId::ClientIcon,
                bounds: Rect::new(group_left, top, icon_size, icon_size),
                source: ImageSource::from(icon),
                state: VisualState::default(),
            });
        }

        scene.nodes.push(SceneNode::Text {
            id: NodeId::Client,
            bounds: Rect::new(
                group_left + icon_advance,
                bounds.y,
                text_width,
                bounds.height,
            ),
            text,
            size: self.font_size(),
            color: palette.muted_text,
            align: TextAlign::Center,
            state: VisualState::default(),
        });
    }

    /// Square edge of the title icon. Tied to the pill height so it matches the
    /// glyphs around it at any bar height or font size.
    fn client_icon_size(&self, pill_height: f32) -> f32 {
        (finite_non_negative(pill_height) * 0.72).floor().max(0.0)
    }

    fn push_dock_hover_title(
        &self,
        scene: &mut Scene,
        dock: Rect,
        view: BarView<'_>,
        interaction: &InteractionState,
        content_start: f32,
        gap: f32,
    ) {
        let Some(NodeId::MinimizedWindow(token)) = interaction.hovered() else {
            return;
        };
        let Some(window) = view
            .minimized_windows
            .iter()
            .find(|window| window.token == token)
        else {
            return;
        };
        let title = window.title.trim();
        if title.is_empty() {
            return;
        }
        let axis = BarAxis::from_rect(self.config.edge, dock);
        let available = (axis.main_start() - gap - content_start).max(0.0);
        if available < self.minimum_visible_width() {
            return;
        }
        let padding = self
            .horizontal_text_padding()
            .min(if axis.vertical { 3.0 } else { 8.0 });
        let bounds = if axis.vertical {
            // A short upright caption uses spare centre space. It never
            // extends beyond the bar where a native surface would crop it;
            // the compositor preview itself is placed inward by the WM.
            let height = (self.font_size() * 2.0 + 8.0).min(available);
            axis.rect(
                axis.main_start() - gap - height,
                axis.cross_start(),
                height,
                axis.cross_len(),
            )
        } else {
            let natural = self.measurer.measure(title, self.font_size()).width + padding * 2.0;
            let width = natural.min(180.0).min(available);
            axis.rect(
                axis.main_start() - gap - width,
                axis.cross_start(),
                width,
                axis.cross_len(),
            )
        };
        let text = self.fit_text(title, bounds.width, padding);
        if text.is_empty() {
            return;
        }
        let state = VisualState {
            hovered: true,
            ..VisualState::default()
        };
        let palette = Palette::for_theme(view.theme);
        scene.nodes.push(SceneNode::RoundedRect {
            id: NodeId::MinimizedWindow(token),
            bounds,
            radius: finite_non_negative(self.config.dock_corner_radius)
                .min(bounds.width.min(bounds.height) * 0.5),
            fill: palette.occupied,
            stroke: None,
            state,
        });
        scene.nodes.push(SceneNode::Text {
            id: NodeId::MinimizedWindow(token),
            bounds: bounds.inset(padding, 0.0),
            text,
            size: self.font_size(),
            color: palette.text,
            align: TextAlign::Center,
            state,
        });
    }

    /// Paint a shelf ending at the inner boundary of the status group. Fixed
    /// slots own their complete cross-axis strip, including the screen edge;
    /// magnification changes only the card image inside that target.
    fn push_dock(
        &self,
        scene: &mut Scene,
        available: Rect,
        controls: Vec<ControlSpec>,
        upstream_overflow: bool,
        interaction: &InteractionState,
        palette: Palette,
    ) -> Option<Rect> {
        if available.is_empty() {
            return None;
        }
        let available_axis = BarAxis::from_rect(self.config.edge, available);
        let metrics = DockMetrics::new(self.config, available_axis.cross_len())?;
        let max_slots = metrics.slot_capacity(available_axis.main_len());
        if max_slots == 0 {
            return None;
        }
        let empty_fallback = controls.is_empty() && !upstream_overflow;
        let show_overflow = upstream_overflow || controls.len() > max_slots;
        let item_capacity = max_slots.saturating_sub(usize::from(show_overflow));
        let first = controls.len().saturating_sub(item_capacity);
        let visible = &controls[first..];
        let count = visible.len() + usize::from(show_overflow || empty_fallback);
        let extent = metrics.width_for_slots(count);
        let bounds = available_axis
            .rect(
                available_axis.main_end() - extent,
                available_axis.cross_start(),
                extent,
                available_axis.cross_len(),
            )
            .intersection(available)?;
        let axis = BarAxis::from_rect(self.config.edge, bounds);
        scene.nodes.push(SceneNode::RoundedRect {
            id: NodeId::DockShelf,
            bounds,
            radius: finite_non_negative(self.config.dock_corner_radius)
                .min(bounds.width.min(bounds.height) * 0.5),
            fill: if empty_fallback {
                Rgba::new(0.0, 0.0, 0.0, 0.0)
            } else {
                palette.occupied
            },
            stroke: None,
            state: VisualState::default(),
        });
        if metrics.separator_width > 0.0 {
            let main = axis.main_start() + metrics.padding + metrics.separator_width * 0.5;
            let start = axis.point(main, axis.cross_start() + metrics.padding);
            let end = axis.point(
                main,
                axis.cross_start() + axis.cross_len() - metrics.padding,
            );
            scene.nodes.push(SceneNode::Polyline {
                id: NodeId::DockShelf,
                bounds: axis.rect(
                    main - metrics.separator_width * 0.5,
                    axis.cross_start() + metrics.padding,
                    metrics.separator_width,
                    (axis.cross_len() - metrics.padding * 2.0).max(0.0),
                ),
                points: vec![start, end],
                color: palette.muted_text,
                width: metrics.separator_width,
                state: VisualState::default(),
            });
        }
        let pointer = match interaction.hovered() {
            Some(NodeId::MinimizedWindow(_)) => interaction.pointer(),
            _ => None,
        };
        for (slot, control) in visible.iter().enumerate() {
            let center = metrics.slot_start(axis)
                + slot as f32 * metrics.slot_pitch
                + metrics.slot_extent * 0.5;
            let scale = self.dock_scale(axis, center, metrics.peak_scale, pointer);
            let item_bounds = metrics.card_rect(axis, slot, scale);
            let hovered = interaction.hovered() == Some(control.id);
            let state = VisualState {
                hovered,
                urgent: control.state.urgent,
                ..VisualState::default()
            };
            let fill = if state.urgent {
                palette.urgent
            } else if hovered {
                palette.selected
            } else {
                palette.hovered
            };
            scene.nodes.push(SceneNode::RoundedRect {
                id: control.id,
                bounds: item_bounds,
                radius: finite_non_negative(self.config.dock_corner_radius)
                    .min(item_bounds.height.min(item_bounds.width) * 0.5),
                fill,
                stroke: state.urgent.then_some(Stroke {
                    color: palette.urgent_stroke,
                    width: 1.0,
                }),
                state,
            });
            scene.nodes.push(SceneNode::Text {
                id: control.id,
                bounds: item_bounds,
                text: control.icon.clone(),
                size: (item_bounds.height * 0.56).max(1.0),
                color: if hovered {
                    palette.selected_text
                } else {
                    palette.text
                },
                align: TextAlign::Center,
                state,
            });
            let bindings = if control.state.enabled {
                control.bindings
            } else {
                crate::controls::InputBindings::default()
            };
            scene.hits.push(HitRegion {
                id: control.id,
                bounds: metrics.slot_rect(axis, slot),
                primary: bindings.primary,
                secondary: bindings.secondary,
                scroll_up: bindings.scroll_up,
                scroll_down: bindings.scroll_down,
            });
        }
        if show_overflow {
            let overflow_bounds = metrics.slot_rect(axis, visible.len());
            // Restore the most recently omitted window, then the next one on
            // the following frame. The old decorative ellipsis left hidden
            // windows completely unreachable by pointer.
            let hidden = controls.get(first.saturating_sub(1)).filter(|_| first > 0);
            let action = hidden
                .filter(|control| control.state.enabled)
                .and_then(|control| control.bindings.primary);
            scene.nodes.push(SceneNode::Text {
                id: NodeId::DockShelf,
                bounds: overflow_bounds,
                text: if first > 0 {
                    format!("+{first}")
                } else {
                    "…".to_owned()
                },
                size: self.font_size(),
                color: palette.muted_text,
                align: TextAlign::Center,
                state: VisualState::default(),
            });
            scene.hits.push(HitRegion {
                id: NodeId::DockShelf,
                bounds: overflow_bounds,
                primary: action,
                secondary: None,
                scroll_up: action,
                scroll_down: action,
            });
        }
        Some(bounds)
    }

    fn dock_scale(&self, axis: BarAxis, center: f32, peak: f32, pointer: Option<Point>) -> f32 {
        let Some(pointer) = pointer else {
            return 1.0;
        };
        let radius = finite_non_negative(self.config.dock_influence_radius).max(1.0);
        let normalized = (1.0 - (axis.main(pointer) - center).abs() / radius).clamp(0.0, 1.0);
        let influence = normalized * normalized * (3.0 - 2.0 * normalized);
        1.0 + (peak - 1.0) * influence
    }

    fn push_pill(
        &self,
        scene: &mut Scene,
        bounds: Rect,
        mut spec: PillSpec,
        interaction: &InteractionState,
        palette: Palette,
        clip: Option<Rect>,
    ) {
        let bounds = if let Some(clip) = clip {
            let Some(bounds) = bounds.intersection(clip) else {
                return;
            };
            bounds
        } else {
            bounds
        };
        if bounds.is_empty() {
            return;
        }
        spec.state.hovered |= interaction.hovered == Some(spec.id);
        let tag_colors = spec.tag_palette.map(|palette| palette.colors(spec.state));
        let fill = if let Some(colors) = tag_colors {
            colors.fill
        } else if spec.state.urgent {
            palette.urgent
        } else if spec.state.selected {
            palette.selected
        } else if spec.state.hovered {
            palette.hovered
        } else if spec.state.occupied {
            palette.occupied
        } else {
            palette.pill
        };
        // Tags use their explicit contrast boundary. Other controls retain
        // the existing material with a stroke reserved for urgency.
        let stroke = if let Some(colors) = tag_colors {
            colors.stroke
        } else if spec.state.urgent {
            Some(Stroke {
                color: palette.urgent_stroke,
                width: 1.0,
            })
        } else {
            None
        };
        scene.nodes.push(SceneNode::RoundedRect {
            id: spec.id,
            bounds,
            radius: finite_non_negative(self.config.corner_radius)
                .min(bounds.width.min(bounds.height) * 0.5),
            fill,
            stroke,
            state: spec.state,
        });

        // Text always lives in physical x/y boxes. A sidebar may stack two
        // short lines, but never rotates glyphs, titles or progress labels.
        let padding = self.pill_text_padding(bounds.width, spec.compact);
        let text_bounds = bounds.inset(padding, 0.0);
        let text = self.fit_text(&spec.text, text_bounds.width, 0.0);
        let marker_fits = tag_colors.is_some() && spec.state.occupied && bounds.width >= 10.0 && {
            let text_height = self.measurer.measure(&text, self.font_size()).height;
            text_height.is_finite() && bounds.height >= text_height.max(0.0) + 12.0
        };
        let color = if let Some(colors) = tag_colors {
            colors.foreground
        } else if spec.state.selected {
            palette.selected_text
        } else {
            palette.text
        };
        let network_columns = spec
            .network
            .as_ref()
            .filter(|_| !self.config.edge.is_vertical() && !spec.compact);
        let percent_columns = spec.percentage.as_ref().filter(|columns| {
            !self.config.edge.is_vertical() && (!spec.compact || columns[0].is_empty())
        });
        let lines: Vec<&str> = if let Some(columns) = network_columns {
            self.push_network_text(scene, text_bounds, columns, color, spec.state);
            Vec::new()
        } else if let Some(columns) = percent_columns {
            self.push_percent_text(scene, text_bounds, &spec, columns, color);
            Vec::new()
        } else if spec.lines.is_empty() {
            vec![&spec.text]
        } else {
            spec.lines.iter().map(String::as_str).collect()
        };
        let line_height = self.font_size() + 2.0;
        let block_height = (lines.len() as f32 * line_height).min(text_bounds.height);
        let row_height = block_height / lines.len().max(1) as f32;
        let top = text_bounds.y + (text_bounds.height - block_height) * 0.5;
        for (index, line) in lines.iter().enumerate() {
            let mut size = self.font_size().min(row_height.max(1.0));
            if self.config.edge.is_vertical() && spec.percentage.is_some() {
                // Fit the complete percentage range, not today's digits.
                // A fixed-physical HiDPI sidebar may need smaller logical
                // type; keep that size stable through 9% -> 100% -> --.
                size = self.percent_sidebar_font_size(
                    spec.id,
                    lines.len() > 1 && index == 0,
                    text_bounds.width,
                    size,
                );
            } else if self.config.edge.is_vertical() || spec.compact {
                let natural = self.measurer.measure(line, size).width;
                if natural > text_bounds.width && natural.is_finite() {
                    size = (size * text_bounds.width / natural).max(9.0).min(size);
                    // Pango's hinted advance is not perfectly proportional to
                    // point size, especially at fractional/retina scales. A
                    // bounded remeasure avoids avoidable ellipses such as
                    // VOL -> V… even though one smaller font step fits.
                    let minimum = 9.0_f32.min(size);
                    for _ in 0..6 {
                        if size <= minimum
                            || self.measurer.measure(line, size).width <= text_bounds.width
                        {
                            break;
                        }
                        size = (size - 0.5).max(minimum);
                    }
                }
            }
            let fitted = self.fit_text_sized(line, text_bounds.width, 0.0, size);
            if fitted.is_empty() || text_bounds.is_empty() {
                continue;
            }
            let line_bounds = if lines.len() == 1 {
                text_bounds
            } else {
                Rect::new(
                    text_bounds.x,
                    top + index as f32 * row_height,
                    text_bounds.width,
                    row_height,
                )
            };
            scene.nodes.push(SceneNode::Text {
                id: spec.id,
                bounds: line_bounds,
                text: fitted,
                size,
                color,
                align: TextAlign::Center,
                state: spec.state,
            });
        }

        // Occupancy is independent of selection and urgency. A high-contrast
        // bottom dash also distinguishes these states without relying on color.
        if let Some(colors) = tag_colors
            && spec.state.occupied
            && marker_fits
        {
            let center_x = bounds.x + bounds.width * 0.5;
            let marker_y = bounds.bottom() - 4.0;
            scene.nodes.push(SceneNode::Polyline {
                id: spec.id,
                bounds: Rect::new(center_x - 3.0, marker_y - 1.0, 6.0, 2.0),
                points: vec![
                    Point::new(center_x - 2.0, marker_y),
                    Point::new(center_x + 2.0, marker_y),
                ],
                color: colors.foreground,
                width: 2.0,
                state: spec.state,
            });
        }

        if let Some(percent) = spec.progress {
            let progress = (percent / 100.0).clamp(0.0, 1.0);
            if progress > 0.0 && bounds.width >= 2.0 {
                let inset = 2.0_f32.min(bounds.width * 0.5);
                let start = Point::new(bounds.x + inset, bounds.bottom() - 2.0);
                let end = Point::new(start.x + (bounds.width - inset * 2.0) * progress, start.y);
                let line_bounds = Rect::new(start.x, start.y - 0.5, end.x - start.x, 1.0);
                scene.nodes.push(SceneNode::Polyline {
                    id: spec.id,
                    bounds: line_bounds,
                    points: vec![start, end],
                    color: palette.accent,
                    width: 1.0,
                    state: spec.state,
                });
            }
        }

        // Extend acquisition to the screen edge without enlarging the paint
        // material. Adjacent controls retain separate main-axis intervals.
        let hit_bounds = clip.map_or(bounds, |bar| {
            let axis = BarAxis::from_rect(self.config.edge, bar);
            let item = BarAxis::from_rect(self.config.edge, bounds);
            axis.rect(
                item.main_start(),
                axis.cross_start(),
                item.main_len(),
                axis.cross_len(),
            )
        });
        scene.hits.push(HitRegion {
            id: spec.id,
            bounds: hit_bounds,
            primary: spec.primary,
            secondary: spec.secondary,
            scroll_up: spec.scroll_up,
            scroll_down: spec.scroll_down,
        });
    }

    fn fit_text(&self, text: &str, available: f32, outer_padding: f32) -> String {
        self.fit_text_sized(text, available, outer_padding, self.font_size())
    }

    fn percent_sidebar_font_size(
        &self,
        id: NodeId,
        icon_row: bool,
        available: f32,
        mut size: f32,
    ) -> f32 {
        let icons = self.config.metric_icon_candidates(id).unwrap_or_default();
        for _ in 0..8 {
            let width = if icon_row {
                icons
                    .iter()
                    .map(|icon| {
                        self.measurer
                            .measure(&bounded_borrowed_control_text(icon), size)
                            .width
                    })
                    .fold(0.0_f32, f32::max)
            } else {
                PercentTextMetrics::measure(&[], |text| self.measurer.measure(text, size).width)
                    .value_width
            };
            if !width.is_finite() || width <= available || size <= 1.0 {
                break;
            }
            size = (size * available / width).min(size - 0.5).max(1.0);
        }
        size
    }

    fn push_percent_text(
        &self,
        scene: &mut Scene,
        bounds: Rect,
        spec: &PillSpec,
        columns: &[String; 2],
        color: Rgba,
    ) {
        let Some(metrics) = self.percent_text_metrics(spec.id) else {
            return;
        };
        let value_width = metrics.value_width.min(bounds.width);
        let value_x = if spec.compact {
            bounds.right() - value_width
        } else {
            bounds.x
                + (bounds.width - metrics.width()).max(0.0) * 0.5
                + metrics.icon_width
                + metrics.gap
        };
        let icon_x = value_x - metrics.gap - metrics.icon_width;
        let size = self.font_size().min(bounds.height.max(1.0));
        let cells = [
            (
                columns[0].as_str(),
                Rect::new(icon_x, bounds.y, metrics.icon_width, bounds.height),
                TextAlign::Center,
            ),
            (
                columns[1].as_str(),
                Rect::new(value_x, bounds.y, value_width, bounds.height),
                TextAlign::End,
            ),
        ];
        for (text, cell, align) in cells {
            let Some(cell) = cell.intersection(bounds) else {
                continue;
            };
            let fitted = self.fit_text_sized(text, cell.width, 0.0, size);
            if !fitted.is_empty() {
                scene.nodes.push(SceneNode::Text {
                    id: spec.id,
                    bounds: cell,
                    text: fitted,
                    size,
                    color,
                    align,
                    state: spec.state,
                });
            }
        }
    }

    fn fit_text_sized(&self, text: &str, available: f32, outer_padding: f32, size: f32) -> String {
        let available = (available - 2.0 * finite_non_negative(outer_padding)).max(0.0);
        if available <= 0.0 {
            return String::new();
        }
        if self.measurer.measure(text, size).width <= available {
            return text.to_owned();
        }
        let ellipsis = "…";
        if self.measurer.measure(ellipsis, size).width > available {
            return String::new();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let mut candidate: String = chars.iter().collect();
            candidate.push('…');
            if self.measurer.measure(&candidate, size).width <= available {
                return candidate;
            }
        }
        ellipsis.to_owned()
    }

    fn push_network_text(
        &self,
        scene: &mut Scene,
        bounds: Rect,
        columns: &[String; 3],
        color: Rgba,
        state: VisualState,
    ) {
        let metrics = self.network_text_metrics();
        let icon_gap = if metrics.icon_width > 0.0 {
            metrics.gap * 2.0
        } else {
            0.0
        };
        let cells = [
            (
                columns[0].as_str(),
                metrics.icon_width,
                TextAlign::Center,
                icon_gap,
            ),
            ("\u{2193}", metrics.arrow_width, TextAlign::Center, 0.0),
            (
                columns[1].as_str(),
                metrics.rate_width,
                TextAlign::End,
                metrics.gap,
            ),
            ("\u{2191}", metrics.arrow_width, TextAlign::Center, 0.0),
            (columns[2].as_str(), metrics.rate_width, TextAlign::End, 0.0),
        ];
        let size = self.font_size().min(bounds.height.max(1.0));
        let mut x = bounds.x;
        for (text, width, align, gap) in cells {
            if let Some(cell) = Rect::new(x, bounds.y, width, bounds.height).intersection(bounds) {
                let fitted = self.fit_text_sized(text, cell.width, 0.0, size);
                if !fitted.is_empty() {
                    scene.nodes.push(SceneNode::Text {
                        id: NodeId::Network,
                        bounds: cell,
                        text: fitted,
                        size,
                        color,
                        align,
                        state,
                    });
                }
            }
            x += width + gap;
        }
    }

    fn font_size(&self) -> f32 {
        finite_non_negative(self.config.font_size).max(1.0)
    }

    fn horizontal_text_padding(&self) -> f32 {
        finite_non_negative(self.config.pill_horizontal_padding)
    }

    fn pill_text_padding(&self, width: f32, compact: bool) -> f32 {
        let padding = if self.config.edge.is_vertical() || compact {
            self.horizontal_text_padding().min(4.0)
        } else {
            self.horizontal_text_padding()
        };
        // Inner and outer sidebar padding share one width budget. Keep room
        // for two digits before allocating decorative space.
        if self.config.edge.is_vertical() {
            padding.min((width - self.font_size() * 2.0).max(0.0) * 0.5)
        } else {
            padding
        }
    }

    fn minimum_visible_width(&self) -> f32 {
        finite_non_negative(self.config.minimum_visible_width).max(1.0)
    }
}

#[derive(Debug, Clone)]
struct PillSpec {
    id: NodeId,
    text: String,
    lines: Vec<String>,
    /// Icon, receive rate and transmit rate, without presentation padding.
    network: Option<[String; 3]>,
    /// Icon and unpadded percentage/unknown value in independent fixed slots.
    percentage: Option<[String; 2]>,
    compact: bool,
    primary: Option<UserAction>,
    secondary: Option<UserAction>,
    scroll_up: Option<UserAction>,
    scroll_down: Option<UserAction>,
    state: VisualState,
    progress: Option<f32>,
    tag_palette: Option<TagPalette>,
}

impl From<ControlSpec> for PillSpec {
    fn from(control: ControlSpec) -> Self {
        let text = control.text();
        let network = control
            .network_rates()
            .map(|[rx, tx]| [control.icon.clone(), rx.to_owned(), tx.to_owned()]);
        let percentage =
            percent_metric_index(control.id).map(|_| [control.icon.clone(), control.value.clone()]);
        let bindings = if control.state.enabled {
            control.bindings
        } else {
            crate::controls::InputBindings::default()
        };
        Self {
            id: control.id,
            text,
            lines: Vec::new(),
            network,
            percentage,
            compact: false,
            primary: bindings.primary,
            secondary: bindings.secondary,
            scroll_up: bindings.scroll_up,
            scroll_down: bindings.scroll_down,
            state: VisualState {
                hovered: control.state.hovered,
                selected: control.state.selected
                    || (matches!(control.id, NodeId::Tag(_)) && control.state.filled),
                urgent: control.state.urgent,
                occupied: control.state.occupied
                    || (!matches!(control.id, NodeId::Tag(_)) && control.state.filled),
            },
            progress: control.progress.map(Percent::as_f32),
            tag_palette: None,
        }
    }
}

/// The colors every bar paints with, whether it draws a [`Scene`] itself or
/// hands the same values to a widget toolkit's stylesheet.
///
/// This is public precisely so a toolkit frontend does not have to restate the
/// material in its own CSS, where it would drift from what the Cairo bars draw.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub background: Rgba,
    pub pill: Rgba,
    pub occupied: Rgba,
    pub hovered: Rgba,
    pub selected: Rgba,
    pub urgent: Rgba,
    pub urgent_stroke: Rgba,
    pub text: Rgba,
    pub selected_text: Rgba,
    pub muted_text: Rgba,
    pub accent: Rgba,
}

impl Palette {
    // macOS menu-bar material. Overlay colors are translucent white/black so
    // they read correctly over any frosted background a frontend composites
    // beneath the scene; selection is the system accent blue.
    #[must_use]
    pub const fn for_theme(theme: ThemeMode) -> Self {
        match theme {
            ThemeMode::Dark => Self {
                background: Rgba::rgb8(28, 28, 30),
                pill: Rgba::new(1.0, 1.0, 1.0, 0.0),
                occupied: Rgba::new(1.0, 1.0, 1.0, 0.12),
                hovered: Rgba::new(1.0, 1.0, 1.0, 0.18),
                selected: Rgba::rgb8(10, 132, 255),
                urgent: Rgba::rgb8(255, 69, 58),
                urgent_stroke: Rgba::new(1.0, 1.0, 1.0, 0.35),
                text: Rgba::new(1.0, 1.0, 1.0, 0.88),
                selected_text: Rgba::rgb8(255, 255, 255),
                muted_text: Rgba::new(1.0, 1.0, 1.0, 0.55),
                accent: Rgba::rgb8(10, 132, 255),
            },
            ThemeMode::Light => Self {
                background: Rgba::rgb8(246, 246, 248),
                pill: Rgba::new(0.0, 0.0, 0.0, 0.0),
                occupied: Rgba::new(0.0, 0.0, 0.0, 0.08),
                hovered: Rgba::new(0.0, 0.0, 0.0, 0.12),
                selected: Rgba::rgb8(0, 122, 255),
                urgent: Rgba::rgb8(255, 59, 48),
                urgent_stroke: Rgba::new(0.0, 0.0, 0.0, 0.25),
                text: Rgba::new(0.0, 0.0, 0.0, 0.85),
                selected_text: Rgba::rgb8(255, 255, 255),
                muted_text: Rgba::new(0.24, 0.24, 0.26, 0.60),
                accent: Rgba::rgb8(0, 122, 255),
            },
        }
    }
}

fn status_priority(control: &ControlSpec) -> u8 {
    match control.id {
        NodeId::Clock => 0,
        NodeId::Battery if matches!(control.tone, Some(crate::display::MetricTone::Critical)) => 1,
        NodeId::Audio => 2,
        NodeId::Battery => 3,
        NodeId::Network => 4,
        NodeId::Brightness => 5,
        NodeId::ShellHub(ShellRoute::Hub) => 6,
        NodeId::Theme => 7,
        NodeId::Screenshot => 8,
        NodeId::Memory | NodeId::Cpu => 9,
        NodeId::Media => 10,
        _ => 11,
    }
}

fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AudioState, BatteryState, BrightnessState, MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
        MinimizedWindow, MonitorId, SystemDetails, SystemState, TagState, WindowToken,
    };
    use std::sync::LazyLock;

    static SYSTEM_DETAILS: LazyLock<SystemDetails> = LazyLock::new(SystemDetails::default);

    #[test]
    fn authoritative_physical_height_converts_to_logical_units() {
        assert_eq!(logical_bar_height(42, 1.0), Some(42.0));
        assert_eq!(logical_bar_height(84, 2.0), Some(42.0));
        assert_eq!(logical_bar_height(u32::MAX, 1.0), Some(u32::MAX as f32));
    }

    #[test]
    fn invalid_height_or_scale_has_no_logical_height() {
        assert_eq!(logical_bar_height(0, 1.0), None);
        for scale_factor in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(logical_bar_height(42, scale_factor), None);
        }
    }

    #[test]
    fn unrepresentable_logical_height_is_rejected() {
        assert_eq!(logical_bar_height(u32::MAX, f64::MIN_POSITIVE), None);
        assert_eq!(logical_bar_height(1, f64::MAX), None);
    }

    #[test]
    fn pointer_adapters_reject_unknown_and_zero_motion() {
        assert_eq!(
            PointerAction::from_x11_button(1),
            Some(PointerAction::Primary)
        );
        assert_eq!(
            PointerAction::from_x11_button(3),
            Some(PointerAction::Secondary)
        );
        assert_eq!(
            PointerAction::from_x11_button(4),
            Some(PointerAction::ScrollUp)
        );
        assert_eq!(
            PointerAction::from_x11_button(5),
            Some(PointerAction::ScrollDown)
        );
        assert_eq!(PointerAction::from_x11_button(2), None);

        assert_eq!(
            PointerAction::from_vertical_delta(1.0),
            Some(PointerAction::ScrollUp)
        );
        assert_eq!(
            PointerAction::from_vertical_delta(-1.0),
            Some(PointerAction::ScrollDown)
        );
        assert_eq!(PointerAction::from_vertical_delta(0.0), None);
        assert_eq!(PointerAction::from_vertical_delta(f64::NAN), None);
    }

    #[test]
    fn default_layout_choices_follow_explicit_jwm_protocol_ids() {
        let layouts = PresentationConfig::default().layouts;
        // Every layout the window manager can be put into is offered, in cycle
        // order, each carrying the wire ID rather than its position.
        assert_eq!(layouts.len(), crate::display::CANONICAL_LAYOUT_COUNT);
        assert_eq!(
            layouts.first(),
            Some(&LayoutChoice {
                id: LayoutId(0),
                label: "[]=".to_owned(),
            })
        );
        assert_eq!(
            layouts.last(),
            Some(&LayoutChoice {
                id: LayoutId(1),
                label: "><>".to_owned(),
            })
        );
        assert_eq!(
            layouts.iter().find(|choice| choice.id == LayoutId(2)),
            Some(&LayoutChoice {
                id: LayoutId(2),
                label: "[M]".to_owned(),
            })
        );
    }

    #[test]
    fn icon_set_populates_scene_and_dynamic_tag_labels_safely() {
        let icons = IconSet::nerd_font();
        let config = PresentationConfig::default().with_icon_set(&icons);

        assert_eq!(config.labels.cpu, icons.cpu);
        assert_eq!(config.labels.audio, icons.volume_high);
        assert_eq!(config.tag_labels.len(), MAX_MODEL_TAGS);
        assert_eq!(config.tag_labels[0], icons.tag_icon(0));
        assert_eq!(
            config.tag_labels[MAX_MODEL_TAGS - 1],
            MAX_MODEL_TAGS.to_string()
        );
    }

    #[test]
    fn icon_set_application_bounds_derived_string_fanout() {
        let oversized = "界".repeat(crate::MAX_PROJECTED_CONTROL_TEXT_BYTES * 32);
        let expected = bounded_borrowed_control_text(&oversized);
        let mut clock = String::with_capacity(crate::MAX_PROJECTED_CONTROL_TEXT_BYTES * 8);
        clock.push_str("clock");
        let icons = IconSet {
            tags: Vec::with_capacity(crate::MAX_PROJECTED_CONFIG_ITEMS * 8),
            tag_fallback: crate::TagFallback::Icon(oversized.clone()),
            cpu: oversized.clone(),
            clock,
            monitor_labels: Vec::with_capacity(crate::MAX_PROJECTED_CONFIG_ITEMS * 8),
            ..IconSet::default()
        };
        let directly_converted = PresentationLabels::from(&icons);
        assert_eq!(directly_converted.cpu, oversized);

        let config = PresentationConfig::default().with_icon_set(&icons);

        assert_eq!(config.tag_labels.len(), MAX_MODEL_TAGS);
        assert!(config.tag_labels.iter().all(|label| {
            label == &expected && label.capacity() <= crate::MAX_PROJECTED_CONTROL_TEXT_BYTES
        }));
        assert_eq!(config.labels.cpu, expected);
        assert!(config.labels.cpu.capacity() <= crate::MAX_PROJECTED_CONTROL_TEXT_BYTES);
        assert_eq!(config.labels.clock, "clock");
        assert!(config.labels.clock.capacity() <= crate::MAX_PROJECTED_CONTROL_TEXT_BYTES);
        assert!(icons.clock.capacity() > crate::MAX_PROJECTED_CONTROL_TEXT_BYTES);
        assert_eq!(
            config.icon_set.as_ref(),
            Some(&icons),
            "the exact public icon set remains available while derived visual strings are bounded"
        );
    }

    #[test]
    fn nerd_font_swap_replaces_tag_glyphs_not_only_semantic_labels() {
        let mut config = PresentationConfig::default();
        assert_eq!(config.tag_labels[0], "🖥", "stock preset is emoji");

        assert!(config.apply_nerd_font_icons_if_stock());

        let icons = IconSet::nerd_font();
        assert_eq!(config.tag_labels[0], icons.tag_icon(0));
        assert_eq!(config.labels.cpu, icons.cpu);
        assert_eq!(config.icon_set.as_ref(), Some(&icons));
    }

    #[test]
    fn nerd_font_swap_upgrades_each_stock_half_independently() {
        let icons = IconSet::nerd_font();

        // This is the shape of older xbar config files: custom Nerd Font tags
        // on the left, with the untouched emoji status preset on the right.
        // The tags must survive while the status icons are upgraded.
        let mut config = PresentationConfig {
            tag_labels: vec!["one".to_owned(), "two".to_owned()],
            ..PresentationConfig::default()
        };
        assert!(config.apply_nerd_font_icons_if_stock());
        assert_eq!(config.tag_labels, vec!["one".to_owned(), "two".to_owned()]);
        assert_eq!(config.labels.cpu, icons.cpu);
        assert_eq!(config.icon_set.as_ref(), Some(&icons));

        // The inverse customization is independent too: semantic labels stay
        // owned by the caller, while untouched emoji tags are upgraded.
        let mut config = PresentationConfig::default();
        config.labels.cpu = "CPU".to_owned();
        assert!(config.apply_nerd_font_icons_if_stock());
        assert_eq!(config.labels.cpu, "CPU");
        assert_eq!(config.tag_labels[0], icons.tag_icon(0));
        assert_eq!(config.icon_set, None);

        // Once both halves are customized there is no stock surface to touch.
        config.tag_labels = vec!["tag".to_owned()];
        assert!(!config.apply_nerd_font_icons_if_stock());
        assert_eq!(config.tag_labels, vec!["tag".to_owned()]);
        assert_eq!(config.labels.cpu, "CPU");
    }

    fn view<'a>(tags: &'a [TagState], client_name: &'a str) -> BarView<'a> {
        static NETWORK: crate::NetworkState = crate::NetworkState::disconnected();
        static MEDIA: crate::MediaState = crate::MediaState::inactive();
        BarView {
            network: &NETWORK,
            media: &MEDIA,
            wm_available: true,
            wm_sequence: Some(1),
            wm_session_id: 7,
            blur_status_bar: None,
            wallpaper_luminance: None,
            tags,
            active_tag: tags
                .iter()
                .position(|tag| tag.selected)
                .and_then(TagId::new),
            monitor: MonitorId(2),
            geometry: None,
            layout_symbol: "[]=",
            layout: None,
            layout_count: None,
            client_name,
            client_app_id: "",
            client_icon: None,
            minimized_windows: &[],
            minimized_overflow: false,
            time: "2026-07-14 12:34",
            show_seconds: false,
            layout_selector_open: false,
            theme: ThemeMode::Dark,
            audio: AudioState::new(Some(percent(42)), false),
            audio_device: None,
            system: SystemState::new(Some(percent(25)), Some(percent(50))),
            system_details: &SYSTEM_DETAILS,
            brightness: BrightnessState::new(Some(percent(70))),
            battery: BatteryState::present(Some(percent(80)), false),
        }
    }

    fn percent(value: u8) -> Percent {
        Percent::from_whole(value).unwrap()
    }

    fn engine() -> LayoutEngine<ApproximateTextMeasurer> {
        LayoutEngine::new(
            PresentationConfig::default(),
            ApproximateTextMeasurer::default(),
        )
    }

    fn dock_engine() -> LayoutEngine<ApproximateTextMeasurer> {
        LayoutEngine::new(
            PresentationConfig {
                visibility: PresentationVisibility {
                    client_name: false,
                    monitor: false,
                    system: false,
                    audio: false,
                    brightness: false,
                    battery: false,
                    network: false,
                    media: false,
                    theme: false,
                    screenshot: false,
                    clock: false,
                    shell_hub: false,
                    ..PresentationVisibility::default()
                },
                ..PresentationConfig::default()
            },
            ApproximateTextMeasurer::default(),
        )
    }

    fn minimized(token: u64, title: &str, app_id: &str) -> MinimizedWindow {
        MinimizedWindow {
            token: WindowToken(token),
            monitor: MonitorId(2),
            title: title.to_owned(),
            app_id: app_id.to_owned(),
            flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
        }
    }

    #[test]
    fn hit_test_maps_primary_secondary_and_scroll_actions() {
        let tags = vec![TagState::default(); 3];
        let scene = engine().build(
            view(&tags, "client"),
            Size::new(1600.0, 38.0),
            &InteractionState::default(),
        );
        let tag = scene
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::Tag(TagId::new(1).unwrap()))
            .unwrap();
        let point = Point::new(
            tag.bounds.x + tag.bounds.width * 0.5,
            tag.bounds.y + tag.bounds.height * 0.5,
        );
        assert_eq!(
            scene.action_at(point, PointerAction::Primary),
            Some(UserAction::ViewTag(TagId::new(1).unwrap()))
        );
        assert_eq!(
            scene.action_at(point, PointerAction::Secondary),
            Some(UserAction::ToggleTag(TagId::new(1).unwrap()))
        );

        let audio = scene
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::Audio)
            .unwrap();
        let point = Point::new(audio.bounds.x + 1.0, audio.bounds.y + 1.0);
        assert_eq!(
            scene.action_at(point, PointerAction::ScrollUp),
            Some(UserAction::VolumeUp)
        );
    }

    #[test]
    fn empty_dock_keeps_a_transparent_shelf_target_and_only_shows_separator() {
        let tags = Vec::new();
        let scene = dock_engine().build(
            view(&tags, ""),
            Size::new(640.0, 38.0),
            &InteractionState::default(),
        );
        let bounds = scene
            .bounds_for(NodeId::DockShelf)
            .expect("empty WM shelf remains reportable");
        assert!(bounds.width > 0.0 && bounds.height == 38.0);
        assert!(
            !scene
                .hits
                .iter()
                .any(|hit| matches!(hit.id, NodeId::MinimizedWindow(_)))
        );
        assert!(scene.nodes.iter().any(|node| {
            matches!(
                node,
                SceneNode::Polyline {
                    id: NodeId::DockShelf,
                    ..
                }
            )
        }));
        assert!(scene.nodes.iter().any(|node| {
            matches!(
                node,
                SceneNode::RoundedRect {
                    id: NodeId::DockShelf,
                    fill,
                    ..
                } if fill.alpha == 0.0
            )
        }));
    }

    #[test]
    fn sub_slot_dock_does_not_starve_status() {
        let tags = Vec::new();
        let mut engine = dock_engine();
        engine.config_mut().visibility.clock = true;

        // A shelf never wins the last usable target over the clock. Small
        // widths once reserved the Dock first and left an 8px status fragment.
        for width in [100.0, 106.0, 160.0] {
            let narrow = engine.build(
                view(&tags, ""),
                Size::new(width, 38.0),
                &InteractionState::default(),
            );
            let clock = narrow
                .hits
                .iter()
                .find(|hit| hit.id == NodeId::Clock)
                .expect("clock survives");
            assert!(clock.bounds.width >= 32.0);
            assert_eq!(narrow.bounds_for(NodeId::DockShelf), None);
        }
        let room = engine.build(
            view(&tags, ""),
            Size::new(640.0, 38.0),
            &InteractionState::default(),
        );
        assert!(room.hits.iter().any(|hit| hit.id == NodeId::Clock));
        assert!(room.bounds_for(NodeId::DockShelf).is_some());
    }

    #[test]
    fn dock_cards_are_window_shaped_restore_targets_and_overflow_is_explicit() {
        let tags = Vec::new();
        let windows = vec![
            minimized(11, "Terminal", "foot"),
            minimized(12, "Browser", "firefox"),
        ];
        let base = view(&tags, "");
        let scene = dock_engine().build(
            BarView {
                minimized_windows: &windows,
                minimized_overflow: true,
                ..base
            },
            Size::new(800.0, 38.0),
            &InteractionState::default(),
        );
        let hit = scene
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::MinimizedWindow(WindowToken(11)))
            .expect("first minimized card");
        let card = scene
            .nodes_for(hit.id)
            .find_map(|node| match node {
                SceneNode::RoundedRect { bounds, .. } => Some(*bounds),
                _ => None,
            })
            .expect("landscape card");
        assert!((card.width / card.height - 1.5).abs() < 0.01);
        assert!(hit.bounds.width >= card.width && hit.bounds.height >= card.height);
        assert_eq!(
            hit.primary,
            Some(UserAction::RestoreWindow {
                window: WindowToken(11),
                wm_session_id: 7,
                minimized_generation: 1,
                geometry: crate::DockItemGeometry::default(),
            })
        );
        assert!(scene.nodes.iter().any(|node| {
            matches!(
                node,
                SceneNode::Text {
                    id: NodeId::DockShelf,
                    text,
                    ..
                } if text == "…"
            )
        }));
    }

    #[test]
    fn dock_hover_magnifies_neighbours_and_adds_title_without_layout_shift() {
        let tags = Vec::new();
        let windows = vec![
            minimized(21, "Terminal", "foot"),
            minimized(22, "Browser", "firefox"),
        ];
        let dock = dock_engine();
        let base = view(&tags, "");
        let dock_view = BarView {
            minimized_windows: &windows,
            ..base
        };
        let initial = dock.build(
            dock_view,
            Size::new(800.0, 38.0),
            &InteractionState::default(),
        );
        let first = initial
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::MinimizedWindow(WindowToken(21)))
            .unwrap();
        let second = initial
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::MinimizedWindow(WindowToken(22)))
            .unwrap();
        let point = Point::new(
            first.bounds.x + first.bounds.width * 0.5,
            first.bounds.y + first.bounds.height * 0.5,
        );
        assert!(
            !initial
                .nodes
                .iter()
                .any(|node| { matches!(node, SceneNode::Text { text, .. } if text == "Terminal") })
        );

        let mut interaction = InteractionState::default();
        let entered = interaction.update_hover_transition(&initial, point);
        assert_eq!(entered.previous, None);
        assert_eq!(
            entered.current,
            Some(NodeId::MinimizedWindow(WindowToken(21)))
        );
        assert!(entered.target_changed() && entered.pointer_changed);
        let same_target =
            interaction.update_hover_transition(&initial, Point::new(point.x + 1.0, point.y));
        assert!(!same_target.target_changed());
        assert!(same_target.pointer_changed && same_target.needs_redraw());

        let hovered = dock.build(dock_view, Size::new(800.0, 38.0), &interaction);
        let hovered_first = hovered
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::MinimizedWindow(WindowToken(21)))
            .unwrap();
        let hovered_second = hovered
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::MinimizedWindow(WindowToken(22)))
            .unwrap();
        assert_eq!(
            hovered_first.bounds, first.bounds,
            "hover cannot move acquisition slots"
        );
        assert_eq!(hovered_second.bounds, second.bounds);
        let visual = |scene: &Scene, id| {
            scene
                .nodes_for(id)
                .find_map(|node| match node {
                    SceneNode::RoundedRect { bounds, .. } => Some(*bounds),
                    _ => None,
                })
                .expect("card material")
        };
        assert!(visual(&hovered, first.id).width > visual(&initial, first.id).width * 1.45);
        assert!(visual(&hovered, second.id).width > visual(&initial, second.id).width);
        assert!(
            hovered
                .nodes
                .iter()
                .any(|node| { matches!(node, SceneNode::Text { text, .. } if text == "Terminal") })
        );
        assert_eq!(
            hovered.bounds_for(NodeId::DockShelf),
            initial.bounds_for(NodeId::DockShelf),
            "fixed slots keep the shelf and adjacent layout stationary"
        );
    }

    #[test]
    fn tag_states_use_shared_contrast_palette() {
        let mut tags = vec![TagState::default(); 3];
        tags[1].occupied = true;
        tags[2].occupied = true;
        tags[2].selected = true;
        let scene = engine().build(
            view(&tags, "client"),
            Size::new(1600.0, 38.0),
            &InteractionState::default(),
        );
        let palette = TagPalette::for_wallpaper(ThemeMode::Dark, None, TagContrastMode::Auto);

        let tag_rect = |index| {
            scene
                .nodes_for(NodeId::Tag(TagId::new(index).unwrap()))
                .find_map(|node| match node {
                    SceneNode::RoundedRect { fill, stroke, .. } => Some((*fill, *stroke)),
                    _ => None,
                })
                .unwrap()
        };

        assert_eq!(tag_rect(0), (palette.empty.fill, palette.empty.stroke));
        assert_eq!(
            tag_rect(1),
            (palette.occupied.fill, palette.occupied.stroke)
        );
        assert_eq!(
            tag_rect(2),
            (palette.selected.fill, palette.selected.stroke)
        );
    }

    #[test]
    fn occupancy_marker_survives_every_state_without_moving_label_or_hit_target() {
        for sample in [0, 255] {
            for selected in [false, true] {
                for urgent in [false, true] {
                    for hovered in [false, true] {
                        let mut pair = Vec::new();
                        for occupied in [false, true] {
                            let tags = vec![TagState {
                                occupied,
                                selected,
                                urgent,
                                ..TagState::default()
                            }];
                            let id = NodeId::Tag(TagId::new(0).unwrap());
                            let interaction = InteractionState {
                                hovered: hovered.then_some(id),
                                ..InteractionState::default()
                            };
                            let mut sample_view = view(&tags, "client");
                            sample_view.wallpaper_luminance = Some(sample);
                            let scene =
                                engine().build(sample_view, Size::new(1600.0, 38.0), &interaction);
                            let expected = TagPalette::for_wallpaper(
                                sample_view.theme,
                                sample_view.wallpaper_luminance,
                                TagContrastMode::Auto,
                            )
                            .colors(VisualState {
                                occupied,
                                selected,
                                urgent,
                                hovered,
                            });
                            let markers: Vec<_> = scene
                                .nodes_for(id)
                                .filter_map(|node| {
                                    if let SceneNode::Polyline {
                                        bounds,
                                        color,
                                        width,
                                        ..
                                    } = node
                                    {
                                        Some((*bounds, *color, *width))
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            assert_eq!(markers.len(), usize::from(occupied));
                            if occupied {
                                assert_eq!(markers[0].0.width, 6.0);
                                assert_eq!(markers[0].0.height, 2.0);
                                assert_eq!(markers[0].1, expected.foreground);
                                assert_eq!(markers[0].2, 2.0);
                            }
                            let label = scene
                                .nodes_for(id)
                                .find_map(|node| {
                                    if let SceneNode::Text { bounds, .. } = node {
                                        Some(*bounds)
                                    } else {
                                        None
                                    }
                                })
                                .unwrap();
                            pair.push((scene.hits, label));
                        }
                        assert_eq!(pair[0], pair[1]);
                    }
                }
            }
        }
    }

    #[test]
    fn cramped_tag_typography_keeps_original_label_bounds_and_occupied_material() {
        struct TallGlyph(f32);
        impl TextMeasurer for TallGlyph {
            fn measure(&self, text: &str, size: f32) -> Size {
                Size::new(text.chars().count() as f32 * size * 0.5, self.0)
            }
        }
        let id = NodeId::Tag(TagId::new(0).unwrap());
        let tags = vec![TagState {
            occupied: true,
            ..TagState::default()
        }];
        for (height, expected_marker) in [(18.0, true), (19.0, false), (30.0, false)] {
            let engine = LayoutEngine::new(PresentationConfig::default(), TallGlyph(height));
            let scene = engine.build(
                view(&tags, "client"),
                Size::new(1600.0, 42.0),
                &InteractionState::default(),
            );
            let hit = scene.hits.iter().find(|hit| hit.id == id).unwrap();
            let label_bounds = scene
                .nodes_for(id)
                .find_map(|node| {
                    if let SceneNode::Text { bounds, .. } = node {
                        Some(*bounds)
                    } else {
                        None
                    }
                })
                .unwrap();
            assert_eq!(
                label_bounds,
                hit.bounds
                    .inset(0.0, engine.config().vertical_padding)
                    .inset(engine.config().pill_horizontal_padding, 0.0)
            );
            assert_eq!(
                scene
                    .nodes_for(id)
                    .any(|node| matches!(node, SceneNode::Polyline { .. })),
                expected_marker
            );
            let expected =
                TagPalette::for_wallpaper(ThemeMode::Dark, None, TagContrastMode::Auto).occupied;
            assert!(scene.nodes_for(id).any(
                |node| matches!(node, SceneNode::RoundedRect { fill, .. } if *fill == expected.fill)
            ));
        }
    }

    #[test]
    fn wallpaper_adaptation_changes_only_tag_paint_and_preserves_layout() {
        let tags = vec![TagState::default(); 3];
        let mut dark = view(&tags, "client");
        dark.wallpaper_luminance = Some(0);
        let mut bright = dark;
        bright.wallpaper_luminance = Some(255);
        let a = engine().build(dark, Size::new(1600.0, 38.0), &InteractionState::default());
        let b = engine().build(
            bright,
            Size::new(1600.0, 38.0),
            &InteractionState::default(),
        );
        assert_eq!(a.hits, b.hits);
        assert_ne!(a.nodes, b.nodes);
        let non_tags = |scene: &Scene| {
            scene
                .nodes
                .iter()
                .filter(|node| !matches!(node.id(), NodeId::Tag(_)))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            non_tags(&a),
            non_tags(&b),
            "including the bar background and every non-tag control"
        );
    }

    #[test]
    fn filled_only_tag_uses_selected_palette() {
        let tags = vec![TagState {
            filled: true,
            ..TagState::default()
        }];
        let sample = view(&tags, "client");
        let scene = engine().build(
            sample,
            Size::new(1600.0, 38.0),
            &InteractionState::default(),
        );
        let expected = TagPalette::for_wallpaper(
            sample.theme,
            sample.wallpaper_luminance,
            TagContrastMode::Auto,
        )
        .selected;
        assert!(scene.nodes_for(NodeId::Tag(TagId::new(0).unwrap())).any(|node| matches!(node, SceneNode::RoundedRect { fill, state, .. } if *fill == expected.fill && state.selected)));
        assert!(
            scene
                .nodes_for(NodeId::Tag(TagId::new(0).unwrap()))
                .all(|node| {
                    !node.visual_state().occupied && !matches!(node, SceneNode::Polyline { .. })
                }),
            "filled/selected without clients must remain visibly empty"
        );
    }

    #[test]
    fn urgent_selected_tags_share_the_actual_fill_foreground_pair() {
        let tags = vec![TagState {
            urgent: true,
            selected: true,
            occupied: true,
            ..TagState::default()
        }];
        let interaction = InteractionState {
            hovered: Some(NodeId::Tag(TagId::new(0).unwrap())),
            ..InteractionState::default()
        };
        let mut sample = view(&tags, "client");
        sample.wallpaper_luminance = Some(255);
        let scene = engine().build(sample, Size::new(1600.0, 38.0), &interaction);
        let colors = TagPalette::for_wallpaper(
            sample.theme,
            sample.wallpaper_luminance,
            TagContrastMode::Auto,
        )
        .urgent;
        let mut found_fill = false;
        let mut found_text = false;
        for node in scene.nodes_for(NodeId::Tag(TagId::new(0).unwrap())) {
            match node {
                SceneNode::RoundedRect { fill, stroke, .. } => {
                    assert_eq!(*fill, colors.fill);
                    assert_eq!(*stroke, colors.stroke);
                    found_fill = true;
                }
                SceneNode::Text { color, .. } => {
                    assert_eq!(*color, colors.foreground);
                    found_text = true;
                }
                _ => {}
            }
        }
        assert!(found_fill && found_text);
    }

    #[test]
    fn hit_test_rejects_clip_area_outside_the_viewport() {
        let tag = TagId::new(0).unwrap();
        let scene = Scene {
            viewport: Size::new(100.0, 40.0),
            clip: Rect::new(0.0, 0.0, 200.0, 40.0),
            nodes: Vec::new(),
            hits: vec![HitRegion {
                id: NodeId::Tag(tag),
                bounds: Rect::new(120.0, 5.0, 30.0, 20.0),
                primary: Some(UserAction::ViewTag(tag)),
                secondary: None,
                scroll_up: None,
                scroll_down: None,
            }],
        };

        assert_eq!(scene.hit_test(Point::new(125.0, 10.0)), None);
        assert_eq!(
            scene.action_at(Point::new(125.0, 10.0), PointerAction::Primary),
            None
        );

        let mut malformed = scene;
        malformed.hits[0].bounds = Rect::new(10.0, 5.0, 30.0, 20.0);
        assert!(malformed.hit_test(Point::new(15.0, 10.0)).is_some());
        malformed.clip.x = f32::NAN;
        assert_eq!(malformed.hit_test(Point::new(15.0, 10.0)), None);
    }

    #[test]
    fn shell_entries_reach_the_scene_and_carry_their_bindings() {
        let tags = vec![TagState::default(); 4];
        let config = PresentationConfig {
            shell_routes: vec![ShellRoute::Hub, ShellRoute::Notifications],
            ..PresentationConfig::default()
        };
        let engine = LayoutEngine::new(config, ApproximateTextMeasurer::default());
        let scene = engine.build(
            view(&tags, ""),
            Size::new(2400.0, 38.0),
            &InteractionState::default(),
        );

        // Every Cairo-based bar goes through this path, so reaching the scene
        // is what "the entry works without frontend code" actually means.
        let shell: Vec<_> = scene
            .hits
            .iter()
            .filter(|hit| matches!(hit.id, NodeId::ShellHub(_)))
            .collect();
        assert_eq!(
            shell.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            [
                NodeId::ShellHub(ShellRoute::Hub),
                NodeId::ShellHub(ShellRoute::Notifications),
            ]
        );

        let hub = shell[0];
        assert_eq!(
            hub.action(PointerAction::Primary),
            Some(UserAction::OpenShellHub(ShellRoute::Hub))
        );
        assert_eq!(
            hub.action(PointerAction::ScrollDown),
            Some(UserAction::OpenShellHub(ShellRoute::Applications))
        );

        // Clicking the cell must resolve through hit testing, not just exist
        // in the display list.
        let inside = Point::new(hub.bounds.x + 1.0, hub.bounds.y + 1.0);
        assert_eq!(
            scene.action_at(inside, PointerAction::Primary),
            Some(UserAction::OpenShellHub(ShellRoute::Hub))
        );
    }

    #[test]
    fn duplicate_shell_routes_keep_one_stable_hit_and_the_first_order() {
        let tags = vec![TagState::default(); 4];
        let unique_config = PresentationConfig {
            shell_routes: vec![
                ShellRoute::Notifications,
                ShellRoute::Hub,
                ShellRoute::Clipboard,
            ],
            ..PresentationConfig::default()
        };
        let expected = LayoutEngine::new(unique_config.clone(), ApproximateTextMeasurer::default())
            .build(
                view(&tags, ""),
                Size::new(2400.0, 38.0),
                &InteractionState::default(),
            );
        let duplicate_config = PresentationConfig {
            shell_routes: vec![
                ShellRoute::Notifications,
                ShellRoute::Hub,
                ShellRoute::Notifications,
                ShellRoute::Clipboard,
                ShellRoute::Hub,
            ],
            ..unique_config
        };
        let scene = LayoutEngine::new(duplicate_config, ApproximateTextMeasurer::default()).build(
            view(&tags, ""),
            Size::new(2400.0, 38.0),
            &InteractionState::default(),
        );

        assert_eq!(
            scene, expected,
            "duplicate configuration must produce the same scene as its first occurrences"
        );
        let shell: Vec<_> = scene
            .hits
            .iter()
            .filter(|hit| matches!(hit.id, NodeId::ShellHub(_)))
            .collect();
        assert_eq!(
            shell.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            [
                NodeId::ShellHub(ShellRoute::Notifications),
                NodeId::ShellHub(ShellRoute::Hub),
                NodeId::ShellHub(ShellRoute::Clipboard),
            ]
        );
        for hit in &shell {
            let NodeId::ShellHub(route) = hit.id else {
                unreachable!("the iterator retained only shell hits");
            };
            let point = Point::new(
                hit.bounds.x + hit.bounds.width * 0.5,
                hit.bounds.y + hit.bounds.height * 0.5,
            );
            assert_eq!(scene.hit_test(point).map(|region| region.id), Some(hit.id));
            assert_eq!(
                scene.action_at(point, PointerAction::Primary),
                Some(UserAction::OpenShellHub(route))
            );
        }

        let point = |hit: &HitRegion| {
            Point::new(
                hit.bounds.x + hit.bounds.width * 0.5,
                hit.bounds.y + hit.bounds.height * 0.5,
            )
        };
        let mut interaction = InteractionState::default();
        assert!(interaction.press(&scene, point(shell[0])));
        assert_eq!(
            interaction.release(&scene, point(shell[1]), PointerAction::Primary),
            None,
            "releasing over another shell cell must not inherit the pressed identity"
        );
        assert!(interaction.press(&scene, point(shell[1])));
        assert_eq!(
            interaction.release(&scene, point(shell[1]), PointerAction::Primary),
            Some(UserAction::OpenShellHub(ShellRoute::Hub))
        );
    }

    #[test]
    fn shell_entries_are_dropped_before_the_clock_on_a_narrow_bar() {
        let tags = vec![TagState::default(); 4];
        let engine = LayoutEngine::new(
            PresentationConfig::default(),
            ApproximateTextMeasurer::default(),
        );
        let scene = engine.build(
            view(&tags, ""),
            Size::new(220.0, 38.0),
            &InteractionState::default(),
        );

        // A launcher is worth less bar width than a clock or a battery
        // reading, so the shell cells are the first status cells to go.
        assert!(
            !scene
                .hits
                .iter()
                .any(|hit| matches!(hit.id, NodeId::ShellHub(_)))
        );
        assert!(scene.hits.iter().any(|hit| hit.id == NodeId::Clock));
    }

    #[test]
    fn tag_count_and_labels_are_dynamic() {
        let mut tags = vec![TagState::default(); 12];
        tags[11].occupied = true;
        let config = PresentationConfig {
            tag_labels: vec!["one".to_owned(), "two".to_owned()],
            visibility: PresentationVisibility {
                client_name: false,
                monitor: false,
                system: false,
                audio: false,
                brightness: false,
                battery: false,
                network: false,
                media: false,
                theme: false,
                screenshot: false,
                clock: false,
                shell_hub: false,
                ..PresentationVisibility::default()
            },
            ..PresentationConfig::default()
        };
        let engine = LayoutEngine::new(config, ApproximateTextMeasurer::default());
        let scene = engine.build(
            view(&tags, ""),
            Size::new(2400.0, 38.0),
            &InteractionState::default(),
        );

        let tag_hits: Vec<_> = scene
            .hits
            .iter()
            .filter(|hit| matches!(hit.id, NodeId::Tag(_)))
            .collect();
        assert_eq!(tag_hits.len(), 12);
        let last = TagId::new(11).unwrap();
        assert_eq!(
            tag_hits.last().unwrap().primary,
            Some(UserAction::ViewTag(last))
        );
        assert!(
            scene
                .nodes_for(NodeId::Tag(last))
                .any(|node| { matches!(node, SceneNode::Text { text, .. } if text == "12") })
        );
    }

    fn icon_bounds(scene: &Scene) -> Option<Rect> {
        scene.nodes.iter().find_map(|node| match node {
            SceneNode::Image { bounds, .. } => Some(*bounds),
            _ => None,
        })
    }

    fn title_bounds(scene: &Scene) -> Option<Rect> {
        scene.nodes_for(NodeId::Client).find_map(|node| match node {
            SceneNode::Text { bounds, .. } => Some(*bounds),
            _ => None,
        })
    }

    /// The icon leads the title, and the pair is centred where the title alone
    /// would have been — so resolving an icon does not shove the title sideways.
    #[test]
    fn the_window_icon_and_its_title_are_centred_as_one_group() {
        let tags = vec![TagState::default(); 2];
        let icon = crate::app_icon::AppIcon::new(std::path::PathBuf::from("/icons/editor.png"));
        let engine = engine();
        let viewport = Size::new(1200.0, 38.0);

        let mut with_icon = view(&tags, "Editor");
        with_icon.client_icon = Some(&icon);
        let scene = engine.build(with_icon, viewport, &InteractionState::default());
        let image = icon_bounds(&scene).expect("an icon node");
        let title = title_bounds(&scene).expect("a title node");

        assert!(
            image.right() <= title.x + f32::EPSILON,
            "icon {image:?} must precede title {title:?}"
        );
        assert!(image.width > 0.0 && (image.width - image.height).abs() < f32::EPSILON);

        let plain = engine.build(
            view(&tags, "Editor"),
            viewport,
            &InteractionState::default(),
        );
        assert!(icon_bounds(&plain).is_none());
        let plain_title = title_bounds(&plain).expect("a title node");
        let group_centre = (image.x + title.right()) * 0.5;
        let plain_centre = plain_title.x + plain_title.width * 0.5;
        assert!(
            (group_centre - plain_centre).abs() < 1.0,
            "group centre {group_centre} drifted from {plain_centre}"
        );
    }

    /// On a bar with no room to spare the title wins: it carries more meaning
    /// than the icon, and half an icon carries none.
    #[test]
    fn a_crowded_bar_drops_the_icon_before_the_title() {
        let tags = vec![TagState::default(); 9];
        let icon = crate::app_icon::AppIcon::new(std::path::PathBuf::from("/icons/editor.png"));
        let mut narrow = view(&tags, "Editor");
        narrow.client_icon = Some(&icon);
        let scene = engine().build(narrow, Size::new(360.0, 38.0), &InteractionState::default());
        assert!(icon_bounds(&scene).is_none());
    }

    #[test]
    fn hiding_the_icon_leaves_the_title_exactly_where_it_was() {
        let tags = vec![TagState::default(); 2];
        let icon = crate::app_icon::AppIcon::new(std::path::PathBuf::from("/icons/editor.png"));
        let mut hidden = view(&tags, "Editor");
        hidden.client_icon = Some(&icon);
        let engine = LayoutEngine::new(
            PresentationConfig {
                visibility: PresentationVisibility {
                    client_icon: false,
                    ..PresentationVisibility::default()
                },
                ..PresentationConfig::default()
            },
            ApproximateTextMeasurer::default(),
        );
        let scene = engine.build(
            hidden,
            Size::new(1200.0, 38.0),
            &InteractionState::default(),
        );
        assert!(icon_bounds(&scene).is_none());
        assert!(title_bounds(&scene).is_some());
    }

    #[test]
    fn narrow_viewport_clips_every_node_and_hit_region() {
        let tags = vec![TagState::default(); 9];
        let scene = engine().build(
            view(&tags, "a very long client title"),
            Size::new(96.0, 30.0),
            &InteractionState::default(),
        );
        assert!(!scene.nodes.is_empty());
        for node in &scene.nodes {
            let bounds = node.bounds();
            assert!(bounds.x >= scene.clip.x);
            assert!(bounds.y >= scene.clip.y);
            assert!(bounds.right() <= scene.clip.right() + f32::EPSILON);
            assert!(bounds.bottom() <= scene.clip.bottom() + f32::EPSILON);
        }
        for hit in &scene.hits {
            assert!(hit.bounds.x >= scene.clip.x);
            assert!(hit.bounds.right() <= scene.clip.right() + f32::EPSILON);
        }
    }

    #[test]
    fn hover_is_stable_and_changes_visual_state() {
        let tags = vec![TagState::default(); 2];
        let engine = engine();
        let initial = engine.build(
            view(&tags, ""),
            Size::new(1200.0, 38.0),
            &InteractionState::default(),
        );
        let id = NodeId::Tag(TagId::new(0).unwrap());
        let bounds = initial.hits.iter().find(|hit| hit.id == id).unwrap().bounds;
        let point = Point::new(bounds.x + 1.0, bounds.y + 1.0);

        let mut interaction = InteractionState::default();
        assert!(interaction.update_hover(&initial, point));
        assert!(!interaction.update_hover(&initial, point));
        assert_eq!(interaction.hovered(), Some(id));

        let hovered = engine.build(view(&tags, ""), Size::new(1200.0, 38.0), &interaction);
        assert!(hovered.nodes_for(id).all(|node| {
            matches!(node, SceneNode::RoundedRect { state, .. } | SceneNode::Text { state, .. } if state.hovered)
        }));
        assert_eq!(hovered.hit_test(point).map(|region| region.id), Some(id));
    }

    fn damage_scene(nodes: Vec<SceneNode>) -> Scene {
        Scene {
            viewport: Size::new(200.0, 40.0),
            clip: Rect::new(0.0, 0.0, 200.0, 40.0),
            nodes,
            hits: Vec::new(),
        }
    }

    fn damage_node(id: NodeId, bounds: Rect, text: &str) -> SceneNode {
        SceneNode::Text {
            id,
            bounds,
            text: text.to_owned(),
            size: 12.0,
            color: Rgba::rgb8(255, 255, 255),
            align: TextAlign::Center,
            state: VisualState::default(),
        }
    }

    #[test]
    fn scene_damage_is_empty_for_identical_frames() {
        let scene = damage_scene(vec![damage_node(
            NodeId::Clock,
            Rect::new(10.0, 5.0, 40.0, 20.0),
            "12:34",
        )]);
        assert!(scene.damage_from(&scene).is_empty());
    }

    #[test]
    fn scene_damage_covers_old_and_new_bounds_for_move_remove_and_content_change() {
        let previous = damage_scene(vec![
            damage_node(NodeId::Clock, Rect::new(10.0, 5.0, 40.0, 20.0), "12:34"),
            damage_node(NodeId::Client, Rect::new(80.0, 5.0, 30.0, 20.0), "old"),
        ]);
        let current = damage_scene(vec![damage_node(
            NodeId::Clock,
            Rect::new(30.0, 5.0, 50.0, 20.0),
            "12:35",
        )]);

        let damage = current.damage_from(&previous);
        assert_eq!(damage.regions().len(), 1);
        assert_eq!(damage.regions()[0], Rect::new(10.0, 5.0, 100.0, 20.0));
    }

    #[test]
    fn scene_damage_detects_overlapping_display_list_reorder() {
        let first = damage_node(NodeId::Client, Rect::new(10.0, 5.0, 60.0, 20.0), "first");
        let second = damage_node(NodeId::Clock, Rect::new(40.0, 5.0, 60.0, 20.0), "second");
        let previous = damage_scene(vec![first.clone(), second.clone()]);
        let current = damage_scene(vec![second, first]);

        let damage = current.damage_from(&previous);
        assert_eq!(damage.regions(), &[Rect::new(10.0, 5.0, 90.0, 20.0)]);
    }

    #[test]
    fn scene_damage_detects_reorder_combined_with_node_addition() {
        let first = damage_node(NodeId::Client, Rect::new(10.0, 5.0, 60.0, 20.0), "first");
        let second = damage_node(NodeId::Clock, Rect::new(40.0, 5.0, 60.0, 20.0), "second");
        let added = damage_node(NodeId::Theme, Rect::new(150.0, 5.0, 20.0, 20.0), "added");
        let previous = damage_scene(vec![first.clone(), second.clone()]);
        let current = damage_scene(vec![second, first, added]);

        let damage = current.damage_from(&previous);
        assert_eq!(
            damage.regions(),
            &[
                Rect::new(10.0, 5.0, 90.0, 20.0),
                Rect::new(150.0, 5.0, 20.0, 20.0),
            ]
        );
    }

    #[test]
    fn viewport_change_invalidates_the_union_of_both_clips() {
        let previous = damage_scene(Vec::new());
        let mut current = damage_scene(Vec::new());
        current.viewport = Size::new(240.0, 50.0);
        current.clip = Rect::new(0.0, 0.0, 240.0, 50.0);

        assert_eq!(
            current.damage_from(&previous).regions(),
            &[Rect::new(0.0, 0.0, 240.0, 50.0)]
        );
    }

    fn first_card(scene: &Scene, id: NodeId) -> Rect {
        scene
            .nodes_for(id)
            .find_map(|node| match node {
                SceneNode::RoundedRect { bounds, .. } => Some(*bounds),
                _ => None,
            })
            .expect("card material")
    }

    #[test]
    fn four_edges_keep_targets_disjoint_clipped_and_acquirable_at_the_screen_edge() {
        use crate::placement::BarEdge;
        let mut tags = vec![TagState::default(); MAX_MODEL_TAGS];
        tags[MAX_MODEL_TAGS - 1].selected = true;
        tags[3].urgent = true;
        let windows: Vec<_> = (1..=12)
            .map(|id| minimized(id, "A long minimized application", "test"))
            .collect();
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            for extent in [80.0, 100.0, 160.0, 220.0, 320.0, 640.0, 1080.0, 1920.0] {
                for thickness in [12.0, 24.0, 38.0, 42.0, 64.0] {
                    let viewport = if edge.is_vertical() {
                        Size::new(thickness, extent)
                    } else {
                        Size::new(extent, thickness)
                    };
                    let engine = LayoutEngine::new(
                        PresentationConfig {
                            edge,
                            bar_height: thickness,
                            ..PresentationConfig::default()
                        },
                        ApproximateTextMeasurer::default(),
                    );
                    let scene = engine.build(
                        BarView {
                            minimized_windows: &windows,
                            ..view(&tags, "An enormously long focused title")
                        },
                        viewport,
                        &InteractionState::default(),
                    );
                    let clip = Rect::from_size(viewport);
                    for node in &scene.nodes {
                        let bounds = node.bounds();
                        assert!(!bounds.is_empty(), "{edge:?} {viewport:?}: {node:?}");
                        assert!(
                            bounds.x >= -0.001
                                && bounds.y >= -0.001
                                && bounds.right() <= clip.right() + 0.001
                                && bounds.bottom() <= clip.bottom() + 0.001,
                            "{edge:?} {viewport:?}: node escapes {bounds:?}"
                        );
                    }
                    for (index, hit) in scene.hits.iter().enumerate() {
                        assert!(!hit.bounds.is_empty());
                        let axis = BarAxis::from_rect(edge, hit.bounds);
                        assert!(
                            axis.main_len() >= 31.999,
                            "unusable fragment {edge:?} {viewport:?} {hit:?}"
                        );
                        assert!(
                            (axis.cross_len() - thickness).abs() < 0.001,
                            "target must reach the edge"
                        );
                        let middle = Point::new(
                            hit.bounds.x + hit.bounds.width * 0.5,
                            hit.bounds.y + hit.bounds.height * 0.5,
                        );
                        assert_eq!(scene.hit_test(middle).map(|hit| hit.id), Some(hit.id));
                        let edge_point = match edge {
                            BarEdge::Top => Point::new(middle.x, 0.0),
                            BarEdge::Bottom => Point::new(middle.x, thickness - 0.01),
                            BarEdge::Left => Point::new(0.0, middle.y),
                            BarEdge::Right => Point::new(thickness - 0.01, middle.y),
                        };
                        assert_eq!(scene.hit_test(edge_point).map(|hit| hit.id), Some(hit.id));
                        for other in &scene.hits[index + 1..] {
                            assert!(
                                hit.bounds.intersection(other.bounds).is_none(),
                                "{edge:?} {viewport:?}: overlapping {:?} and {:?}",
                                hit.id,
                                other.id
                            );
                        }
                    }
                    if thickness >= 38.0
                        && scene.hits.iter().any(|hit| hit.id == NodeId::LayoutButton)
                    {
                        assert!(
                            scene
                                .nodes_for(NodeId::LayoutButton)
                                .any(|node| matches!(node,
                            SceneNode::Text { text, .. } if !text.is_empty())),
                            "invisible layout button on {edge:?} {viewport:?}"
                        );
                    }
                    if thickness > 12.0 && extent >= 100.0 {
                        assert!(
                            scene.hits.iter().any(|hit| hit.id
                                == NodeId::Tag(TagId::new(MAX_MODEL_TAGS - 1).unwrap())),
                            "active workspace lost on {edge:?} {viewport:?}"
                        );
                        assert!(
                            scene.hits.iter().any(|hit| hit.id == NodeId::Clock),
                            "clock lost on {edge:?} {viewport:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn sidebars_stack_upright_clock_and_readings_and_keep_long_titles_out_of_the_strip() {
        use crate::placement::BarEdge;
        let tags = vec![TagState::default(); 3];
        for edge in [BarEdge::Left, BarEdge::Right] {
            let engine = LayoutEngine::new(
                PresentationConfig {
                    edge,
                    ..PresentationConfig::default()
                },
                ApproximateTextMeasurer::default(),
            );
            let scene = engine.build(
                view(&tags, "This title must not become a vertical tower"),
                Size::new(42.0, 1080.0),
                &InteractionState::default(),
            );
            let clock: Vec<_> = scene
                .nodes_for(NodeId::Clock)
                .filter_map(|node| match node {
                    SceneNode::Text { bounds, text, .. } => Some((bounds, text.as_str())),
                    _ => None,
                })
                .collect();
            assert_eq!(
                clock.iter().map(|(_, text)| *text).collect::<Vec<_>>(),
                ["12", "34"]
            );
            assert!(clock[0].0.bottom() <= clock[1].0.y);
            assert_eq!(clock[0].0.x, clock[1].0.x);
            assert!(!scene.nodes.iter().any(|node| node.id() == NodeId::Client));
            let audio = scene
                .hits
                .iter()
                .find(|hit| hit.id == NodeId::Audio)
                .expect("usable volume target");
            assert!(audio.bounds.height >= 38.0);
            assert_eq!(
                scene.action_at(
                    Point::new(0.0, audio.bounds.y + 1.0),
                    PointerAction::ScrollUp
                ),
                Some(UserAction::VolumeUp)
            );
        }
    }

    #[test]
    fn workspace_strip_compacts_every_tag_before_hiding_any() {
        use crate::placement::BarEdge;
        let mut tags = vec![TagState::default(); 4];
        tags[3].selected = true;
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            let engine = LayoutEngine::new(
                PresentationConfig {
                    edge,
                    tag_labels: (1..=4).map(|index| format!("Workspace {index}")).collect(),
                    ..PresentationConfig::default()
                },
                ApproximateTextMeasurer::default(),
            );
            // Four 32px targets plus their three gaps fit, while their full
            // labels do not. An overflow button would consume a usable tag.
            let viewport = if edge.is_vertical() {
                Size::new(42.0, 460.0)
            } else {
                Size::new(460.0, 42.0)
            };
            let scene = engine.build(view(&tags, ""), viewport, &InteractionState::default());
            for index in 0..tags.len() {
                assert!(
                    scene
                        .hits
                        .iter()
                        .any(|hit| hit.id == NodeId::Tag(TagId::new(index).unwrap())),
                    "compact workspace {} was needlessly hidden on {edge:?}",
                    index + 1,
                );
                assert!(scene.nodes_for(NodeId::Tag(TagId::new(index).unwrap())).any(|node| {
                    matches!(node, SceneNode::Text { text, .. } if *text == (index + 1).to_string())
                }), "compact labels must preserve workspace identity on {edge:?}");
            }
            assert!(
                scene
                    .hits
                    .iter()
                    .all(|hit| hit.id != NodeId::WorkspaceOverflow)
            );
        }
    }

    #[test]
    fn actual_active_workspace_wins_when_several_tags_are_selected() {
        use crate::placement::BarEdge;
        let tags = vec![
            TagState {
                selected: true,
                ..TagState::default()
            };
            MAX_MODEL_TAGS
        ];
        let active = TagId::new(MAX_MODEL_TAGS - 1).unwrap();
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            let engine = LayoutEngine::new(
                PresentationConfig {
                    edge,
                    ..PresentationConfig::default()
                },
                ApproximateTextMeasurer::default(),
            );
            for extent in [100.0, 160.0, 220.0, 300.0] {
                let viewport = if edge.is_vertical() {
                    Size::new(42.0, extent)
                } else {
                    Size::new(extent, 42.0)
                };
                let scene = engine.build(
                    BarView {
                        active_tag: Some(active),
                        ..view(&tags, "")
                    },
                    viewport,
                    &InteractionState::default(),
                );
                let hit = scene
                    .hits
                    .iter()
                    .find(|hit| hit.id == NodeId::Tag(active))
                    .unwrap_or_else(|| {
                        panic!("actual active workspace disappeared on {edge:?} at {extent}")
                    });
                assert_eq!(hit.primary, Some(UserAction::ViewTag(active)));
                assert_eq!(
                    hit.scroll_down,
                    Some(UserAction::ViewTag(TagId::new(0).unwrap()))
                );
                if let Some(overflow) = scene.nodes_for(NodeId::WorkspaceOverflow).next() {
                    assert!(
                        matches!(overflow, SceneNode::RoundedRect { state, .. } if state.selected)
                    );
                }
            }
        }
    }

    #[test]
    fn workspace_overflow_reports_hidden_urgency_without_replacing_navigation() {
        use crate::placement::BarEdge;
        let mut tags = vec![TagState::default(); MAX_MODEL_TAGS];
        tags[MAX_MODEL_TAGS - 1].selected = true;
        for tag in &mut tags[..8] {
            tag.urgent = true;
            tag.occupied = true;
        }
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            let engine = LayoutEngine::new(
                PresentationConfig {
                    edge,
                    ..PresentationConfig::default()
                },
                ApproximateTextMeasurer::default(),
            );
            let viewport = if edge.is_vertical() {
                Size::new(42.0, 300.0)
            } else {
                Size::new(300.0, 42.0)
            };
            let scene = engine.build(view(&tags, ""), viewport, &InteractionState::default());
            assert!(
                scene.nodes_for(NodeId::WorkspaceOverflow).any(|node| {
                    matches!(node, SceneNode::Text { text, state, .. }
                    if text.contains('!') && state.urgent && state.occupied)
                }),
                "hidden attention was lost on {edge:?}"
            );
            let hit = scene
                .hits
                .iter()
                .find(|hit| hit.id == NodeId::WorkspaceOverflow)
                .expect("workspace overflow");
            assert_eq!(
                hit.primary,
                Some(UserAction::ViewTag(TagId::new(0).unwrap()))
            );
            let mut quiet_tags = tags.clone();
            quiet_tags.iter_mut().for_each(|tag| tag.urgent = false);
            let quiet = engine.build(
                view(&quiet_tags, ""),
                viewport,
                &InteractionState::default(),
            );
            assert!(quiet.nodes_for(NodeId::WorkspaceOverflow).any(|node| {
                matches!(node, SceneNode::Text { text, state, .. }
                    if !text.contains('!') && !state.urgent)
            }));
            assert_eq!(
                quiet
                    .hits
                    .iter()
                    .find(|hit| hit.id == NodeId::WorkspaceOverflow),
                Some(hit)
            );
        }
    }

    #[test]
    fn tiny_bars_release_padding_before_dropping_the_active_workspace() {
        use crate::placement::BarEdge;
        let mut tags = vec![TagState::default(); MAX_MODEL_TAGS];
        tags[MAX_MODEL_TAGS - 1].selected = true;
        let active = TagId::new(MAX_MODEL_TAGS - 1).unwrap();
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            for thickness in [8.0, 12.0, 24.0, 42.0] {
                let engine = LayoutEngine::new(
                    PresentationConfig {
                        edge,
                        bar_height: thickness,
                        ..PresentationConfig::default()
                    },
                    ApproximateTextMeasurer::default(),
                );
                for extent in [32.0, 40.0, 51.0] {
                    let viewport = if edge.is_vertical() {
                        Size::new(thickness, extent)
                    } else {
                        Size::new(extent, thickness)
                    };
                    let scene =
                        engine.build(view(&tags, ""), viewport, &InteractionState::default());
                    let hit = scene
                        .hits
                        .iter()
                        .find(|hit| hit.id == NodeId::Tag(active))
                        .unwrap_or_else(|| {
                            panic!(
                                "padding swallowed the active workspace on {edge:?} in {viewport:?}"
                            )
                        });
                    assert_eq!(hit.bounds.intersection(scene.clip), Some(hit.bounds));
                    let point = Point::new(hit.bounds.x, hit.bounds.y);
                    assert_eq!(
                        scene.action_at(point, PointerAction::Primary),
                        Some(UserAction::ViewTag(active))
                    );
                    assert!(BarAxis::from_rect(edge, hit.bounds).main_len() >= 32.0);
                    for node in &scene.nodes {
                        assert!(!node.bounds().is_empty());
                        assert_eq!(node.bounds().intersection(scene.clip), Some(node.bounds()));
                    }
                }
            }
        }
    }

    #[test]
    fn workspace_overflow_reaches_every_tag_and_keeps_the_active_one_visible() {
        use crate::placement::BarEdge;
        for edge in [BarEdge::Top, BarEdge::Left] {
            let engine = LayoutEngine::new(
                PresentationConfig {
                    edge,
                    ..PresentationConfig::default()
                },
                ApproximateTextMeasurer::default(),
            );
            for active in 0..MAX_MODEL_TAGS {
                let mut tags = vec![TagState::default(); MAX_MODEL_TAGS];
                tags[active].selected = true;
                let viewport = if edge.is_vertical() {
                    Size::new(42.0, 300.0)
                } else {
                    Size::new(300.0, 42.0)
                };
                let scene = engine.build(view(&tags, ""), viewport, &InteractionState::default());
                let id = NodeId::Tag(TagId::new(active).unwrap());
                assert!(scene.hits.iter().any(|hit| hit.id == id));
                let overflow = scene
                    .hits
                    .iter()
                    .find(|hit| hit.id == NodeId::WorkspaceOverflow)
                    .expect("overflow affordance");
                assert_eq!(
                    overflow.primary,
                    Some(UserAction::ViewTag(
                        TagId::new((active + 1) % MAX_MODEL_TAGS).unwrap()
                    ))
                );
                assert_eq!(
                    overflow.scroll_up,
                    Some(UserAction::ViewTag(
                        TagId::new((active + MAX_MODEL_TAGS - 1) % MAX_MODEL_TAGS).unwrap()
                    ))
                );
            }
        }
    }

    #[test]
    fn all_dock_axes_magnify_landscape_cards_without_moving_hits_or_static_targets() {
        use crate::placement::BarEdge;
        let tags = Vec::new();
        let windows = [
            minimized(1, "First", "test"),
            minimized(2, "Second", "test"),
        ];
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            let mut engine = dock_engine();
            engine.config_mut().edge = edge;
            let viewport = if edge.is_vertical() {
                Size::new(42.0, 800.0)
            } else {
                Size::new(800.0, 42.0)
            };
            let input = BarView {
                minimized_windows: &windows,
                ..view(&tags, "")
            };
            let initial = engine.build(input, viewport, &InteractionState::default());
            let hit = initial
                .hits
                .iter()
                .find(|hit| hit.id == NodeId::MinimizedWindow(WindowToken(1)))
                .unwrap();
            let point = Point::new(
                hit.bounds.x + hit.bounds.width * 0.5,
                hit.bounds.y + hit.bounds.height * 0.5,
            );
            let mut interaction = InteractionState::default();
            interaction.update_hover(&initial, point);
            let hovered = engine.build(input, viewport, &interaction);
            assert_eq!(initial.hits, hovered.hits);
            assert_eq!(
                initial.bounds_for(NodeId::DockShelf),
                hovered.bounds_for(NodeId::DockShelf)
            );
            for id in [
                NodeId::MinimizedWindow(WindowToken(1)),
                NodeId::MinimizedWindow(WindowToken(2)),
            ] {
                let resting = first_card(&initial, id);
                let magnified = first_card(&hovered, id);
                assert!((resting.width / resting.height - 1.5).abs() < 0.001);
                assert!(magnified.width > resting.width && magnified.height > resting.height);
            }
            for scale in [1.0, 1.25, 2.0] {
                let geometry = crate::MonitorGeometry {
                    x: -1920,
                    y: 1060,
                    width: 1920,
                    height: 1080,
                };
                let before =
                    crate::dock::dock_geometry_reports(&initial, engine.config(), geometry, scale);
                let after =
                    crate::dock::dock_geometry_reports(&hovered, engine.config(), geometry, scale);
                assert_eq!(
                    before, after,
                    "stable targets changed on {edge:?} at scale {scale}"
                );
                let interactive =
                    crate::dock::interactive_dock_geometry_reports(&hovered, geometry, scale);
                let card = first_card(&hovered, hit.id);
                let expected = crate::dock::global_physical_geometry(card, geometry, scale);
                assert_eq!(interactive[0].geometry, expected);
            }
        }
    }

    #[test]
    fn background_and_edge_hits_follow_bottom_and_right_in_oversized_viewports() {
        use crate::placement::BarEdge;
        for (edge, viewport, expected) in [
            (
                BarEdge::Bottom,
                Size::new(640.0, 120.0),
                Rect::new(0.0, 78.0, 640.0, 42.0),
            ),
            (
                BarEdge::Right,
                Size::new(120.0, 640.0),
                Rect::new(78.0, 0.0, 42.0, 640.0),
            ),
        ] {
            let engine = LayoutEngine::new(
                PresentationConfig {
                    edge,
                    ..PresentationConfig::default()
                },
                ApproximateTextMeasurer::default(),
            );
            let scene = engine.build(
                view(&[TagState::default()], ""),
                viewport,
                &InteractionState::default(),
            );
            assert_eq!(scene.bounds_for(NodeId::Background), Some(expected));
            for hit in &scene.hits {
                assert_eq!(hit.bounds.intersection(expected), Some(hit.bounds));
            }
        }
    }

    #[test]
    fn authoritative_physical_thickness_is_converted_once_and_invalid_scale_is_bounded() {
        let config = PresentationConfig {
            physical_thickness: Some(42),
            bar_height: 99.0,
            ..PresentationConfig::default()
        };
        assert_eq!(config.logical_thickness(2.0), 21.0);
        assert_eq!(config.logical_thickness(1.25), 33.6);
        assert_eq!(config.logical_thickness(f64::NAN), 42.0);
        let restored: PresentationConfig = serde_json::from_str("{\"bar_height\":36}").unwrap();
        assert_eq!(restored.edge, crate::placement::BarEdge::Top);
        assert_eq!(restored.logical_thickness(2.0), 36.0);
    }

    #[test]
    fn press_release_cannot_rebind_an_overflow_or_a_new_window_incarnation() {
        let make_scene = |action| Scene {
            viewport: Size::new(42.0, 100.0),
            clip: Rect::new(0.0, 0.0, 42.0, 100.0),
            nodes: Vec::new(),
            hits: vec![HitRegion {
                id: NodeId::WorkspaceOverflow,
                bounds: Rect::new(0.0, 0.0, 42.0, 32.0),
                primary: Some(action),
                secondary: None,
                scroll_up: None,
                scroll_down: None,
            }],
        };
        let point = Point::new(0.0, 10.0);
        let first = make_scene(UserAction::ViewTag(TagId::new(2).unwrap()));
        let replaced = make_scene(UserAction::ViewTag(TagId::new(3).unwrap()));
        let mut interaction = InteractionState::default();
        interaction.press(&first, point);
        assert_eq!(
            interaction.release(&replaced, point, PointerAction::Primary),
            None
        );
        interaction.press(&replaced, point);
        assert_eq!(
            interaction.release(&replaced, point, PointerAction::Primary),
            Some(UserAction::ViewTag(TagId::new(3).unwrap()))
        );

        let restore = |generation| UserAction::RestoreWindow {
            window: WindowToken(7),
            wm_session_id: 4,
            minimized_generation: generation,
            geometry: crate::DockItemGeometry::default(),
        };
        let mut original = make_scene(restore(1));
        original.hits[0].id = NodeId::MinimizedWindow(WindowToken(7));
        let mut replacement = make_scene(restore(2));
        replacement.hits[0].id = original.hits[0].id;
        interaction.press(&original, point);
        assert_eq!(
            interaction.release(&replacement, point, PointerAction::Primary),
            None
        );
        assert_eq!(interaction.pressed(), None);
    }

    #[test]
    fn dock_hover_title_temporarily_replaces_the_focused_title_without_overpainting() {
        let tags = vec![TagState::default(); 2];
        let windows = [minimized(1, "Browser", "browser")];
        let input = BarView {
            minimized_windows: &windows,
            ..view(&tags, "Focused editor")
        };
        let engine = engine();
        let initial = engine.build(input, Size::new(1600.0, 42.0), &InteractionState::default());
        assert!(initial.nodes_for(NodeId::Client).next().is_some());
        let hit = initial
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::MinimizedWindow(WindowToken(1)))
            .unwrap();
        let mut interaction = InteractionState::default();
        interaction.update_hover(&initial, Point::new(hit.bounds.x + 1.0, hit.bounds.y + 1.0));
        let hovered = engine.build(input, Size::new(1600.0, 42.0), &interaction);
        assert!(hovered.nodes_for(NodeId::Client).next().is_none());
        assert!(
            hovered
                .nodes
                .iter()
                .any(|node| matches!(node, SceneNode::Text { text, .. } if text == "Browser"))
        );
    }
}
