//! Renderer-neutral layout, scene, and interaction primitives.
//!
//! All geometry in this module is expressed in logical `f32` units. A
//! frontend applies its output scale only while translating a [`Scene`] to
//! device pixels. This keeps layout and hit testing identical across Cairo,
//! wgpu, toolkit, and web renderers.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::ThemeMode;
use crate::controls::{BarPresentation, ControlSpec, PresentationProjector};
use crate::display::{BatteryThresholds, IconSet, UsageThresholds, VolumeThresholds};
use crate::model::{
    BarView, LayoutId, MAX_MODEL_TAGS, Percent, ShellRoute, TagId, UserAction, WindowToken,
};

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
        let pointer_changed = self.pointer.take().is_some();
        hover_changed || press_changed || pointer_changed
    }

    /// Cancel a pending activation without changing hover state.
    pub fn cancel_press(&mut self) -> bool {
        self.pressed.take().is_some()
    }

    pub fn press(&mut self, scene: &Scene, point: Point) -> bool {
        self.pointer = Some(point);
        let next = scene.hit_test(point).map(|region| region.id);
        let changed = self.pressed != next;
        self.pressed = next;
        changed
    }

    /// Activates only when press and release resolve to the same stable node.
    pub fn release(
        &mut self,
        scene: &Scene,
        point: Point,
        input: PointerAction,
    ) -> Option<UserAction> {
        let released = scene.hit_test(point);
        let action = released
            .filter(|region| Some(region.id) == self.pressed)
            .and_then(|region| region.action(input));
        self.pressed = None;
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
    pub shell_routes: [String; 6],
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
    pub const DEFAULT_SHELL_ROUTE_ICONS: [&'static str; 6] = ["◈", "🚀", "🔔", "📋", "📅", "🖼"];

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
}

impl From<&IconSet> for PresentationLabels {
    fn from(icons: &IconSet) -> Self {
        Self {
            clock: icons.clock.clone(),
            screenshot: icons.screenshot.clone(),
            theme_dark: icons.theme_dark.clone(),
            theme_light: icons.theme_light.clone(),
            monitor: icons.monitor.clone(),
            cpu: icons.cpu.clone(),
            memory: icons.memory.clone(),
            audio: icons.volume_high.clone(),
            muted: icons.volume_muted.clone(),
            brightness: icons.brightness.clone(),
            battery: icons.battery.clone(),
            charging: icons.battery_charging.clone(),
            network: "\u{f05a9}".to_owned(),
            network_offline: "\u{f05aa}".to_owned(),
            media_playing: "\u{f040a}".to_owned(),
            media_paused: "\u{f03e4}".to_owned(),
            // Deliberately the same glyphs JWM's own shell rows use, so the
            // bar entry and the page it opens read as one surface.
            shell_routes: PresentationLabels::NERD_FONT_SHELL_ROUTE_ICONS.map(str::to_owned),
        }
    }
}

impl PresentationLabels {
    /// Labels matching the shared toolkit Nerd Font preset.
    #[must_use]
    pub fn nerd_font() -> Self {
        Self::from(&IconSet::nerd_font())
    }

    pub const NERD_FONT_SHELL_ROUTE_ICONS: [&'static str; 6] = [
        "\u{f009}", // grid — hub home
        "\u{f135}", // rocket — applications
        "\u{f0f3}", // bell — notifications
        "\u{f0ea}", // clipboard
        "\u{f073}", // calendar
        "\u{f03e}", // image — wallpaper
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
            monitor: true,
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
    pub bar_height: f32,
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
    /// Logical horizontal distance over which neighbouring cards magnify.
    pub dock_influence_radius: f32,
    pub dock_separator_width: f32,
    /// Preferred fraction reserved for tags/layout before right-side items.
    pub left_fraction: f32,
    pub tag_labels: Vec<String>,
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
    /// stable [`NodeId`].
    pub shell_routes: Vec<ShellRoute>,
    pub visibility: PresentationVisibility,
}

impl Default for PresentationConfig {
    fn default() -> Self {
        Self {
            bar_height: 38.0,
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
    /// Replace semantic labels and dynamic tag labels from one shared icon
    /// preset while preserving geometry, visibility, and threshold policy.
    pub fn apply_icon_set(&mut self, icons: &IconSet) {
        self.tag_labels = (0..MAX_MODEL_TAGS)
            .map(|index| icons.tag_icon(index).into_owned())
            .collect();
        self.labels = PresentationLabels::from(icons);
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
            self.labels = PresentationLabels::from(&icons);
            // Dynamic battery and volume bands are part of the status preset,
            // so activate them only when that half was still stock.
            self.icon_set = Some(icons.clone());
        }
        if replace_tags {
            self.tag_labels = (0..MAX_MODEL_TAGS)
                .map(|index| icons.tag_icon(index).into_owned())
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

/// Geometry shared by Dock reservation and painting. Keeping the capacity
/// calculation here guarantees that width withheld from status controls can
/// actually produce at least one shelf slot in [`LayoutEngine::push_dock`].
#[derive(Debug, Clone, Copy)]
struct DockMetrics {
    padding: f32,
    gap: f32,
    separator_width: f32,
    item_height: f32,
    item_width: f32,
    peak_scale: f32,
    slot_width: f32,
    slot_pitch: f32,
    fixed_width: f32,
}

impl DockMetrics {
    fn new(config: &PresentationConfig, available_height: f32) -> Option<Self> {
        let padding = finite_non_negative(config.dock_shelf_padding);
        let gap = finite_non_negative(config.dock_item_gap);
        let separator_width = finite_non_negative(config.dock_separator_width);
        let usable_height = (available_height - padding * 2.0).max(0.0);
        let item_height = finite_non_negative(config.dock_item_size).min(usable_height);
        if item_height < 1.0 {
            return None;
        }

        let aspect_ratio = if config.dock_item_aspect_ratio.is_finite() {
            config.dock_item_aspect_ratio.max(1.0)
        } else {
            1.0
        };
        let configured_scale = if config.dock_hover_scale.is_finite() {
            config.dock_hover_scale.max(1.0)
        } else {
            1.0
        };
        let peak_scale = configured_scale.min((usable_height / item_height).max(1.0));
        let item_width = item_height * aspect_ratio;
        let slot_width = item_width * peak_scale;
        let slot_pitch = slot_width + gap;
        let fixed_width = padding * 3.0 + separator_width;
        Some(Self {
            padding,
            gap,
            separator_width,
            item_height,
            item_width,
            peak_scale,
            slot_width,
            slot_pitch,
            fixed_width,
        })
    }

    fn minimum_width(self) -> f32 {
        self.fixed_width + self.slot_width
    }

    fn slot_capacity(self, available_width: f32) -> usize {
        let minimum_width = self.minimum_width();
        if available_width < minimum_width {
            return 0;
        }
        1_usize
            .saturating_add(((available_width - minimum_width) / self.slot_pitch).floor() as usize)
    }

    fn width_for_slots(self, slot_count: usize) -> f32 {
        if slot_count == 0 {
            return 0.0;
        }
        self.fixed_width
            + slot_count as f32 * self.slot_width
            + slot_count.saturating_sub(1) as f32 * self.gap
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
        } = PresentationProjector::project(view, &self.config);
        let viewport = viewport.normalized();
        let clip = Rect::from_size(viewport);
        let bar_height = finite_non_negative(self.config.bar_height).min(viewport.height);
        let bar = Rect::new(0.0, 0.0, viewport.width, bar_height);
        let palette = Palette::for_theme(theme);
        let mut scene = Scene {
            viewport,
            clip,
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

        let horizontal_padding =
            finite_non_negative(self.config.horizontal_padding).min(bar.width * 0.5);
        let vertical_padding =
            finite_non_negative(self.config.vertical_padding).min(bar.height * 0.5);
        let gap = finite_non_negative(self.config.item_gap);
        let pill_height = (bar.height - 2.0 * vertical_padding).max(0.0);
        if pill_height <= 0.0 {
            return scene;
        }

        let y = bar.y + vertical_padding;
        let content_left = bar.x + horizontal_padding;
        let content_right = (bar.right() - horizontal_padding).max(content_left);
        let left_fraction = if self.config.left_fraction.is_finite() {
            self.config.left_fraction.clamp(0.2, 0.8)
        } else {
            0.42
        };
        let right_floor = content_left + (content_right - content_left) * left_fraction;
        let mut right_cursor = content_right;
        let mut dock_bounds = None;
        let dock_enabled = self.config.visibility.minimized_windows && view.wm_available;
        let dock_available_width = (content_right - right_floor).max(0.0);
        let dock_reserve = if dock_enabled {
            self.dock_reserve_width(
                minimized_windows.len(),
                minimized_overflow,
                dock_available_width,
                bar.height,
            )
        } else {
            0.0
        };
        let dock_gap = if dock_reserve > 0.0 { gap } else { 0.0 };

        for control in status {
            let spec = PillSpec::from(control);
            let available = (right_cursor - right_floor - dock_reserve - dock_gap).max(0.0);
            if available < self.minimum_visible_width() {
                break;
            }
            let natural = self.pill_width(&spec.text);
            let width = natural.min(available);
            let candidate = Rect::new(right_cursor - width, y, width, pill_height);
            let Some(bounds) = candidate.intersection(bar) else {
                break;
            };
            self.push_pill(&mut scene, bounds, spec, interaction, palette, Some(bar));
            right_cursor = (bounds.x - gap).max(content_left);
        }

        if dock_enabled {
            let available = (right_cursor - right_floor).max(0.0);
            if let Some(bounds) = self.push_dock(
                &mut scene,
                Rect::new(right_floor, bar.y, available, bar.height),
                minimized_windows,
                minimized_overflow,
                interaction,
                palette,
            ) {
                dock_bounds = Some(bounds);
                right_cursor = (bounds.x - gap).max(content_left);
            }
        }

        let left_limit = (right_cursor - gap).max(content_left);
        let mut left_cursor = content_left;

        for control in tags {
            let available = (left_limit - left_cursor).max(0.0);
            if available < self.minimum_visible_width() {
                break;
            }
            let label = control.text();
            let width = self.pill_width(&label).min(available);
            let bounds = Rect::new(left_cursor, y, width, pill_height);
            self.push_pill(
                &mut scene,
                bounds,
                PillSpec::from(control),
                interaction,
                palette,
                Some(bar),
            );
            left_cursor = bounds.right() + gap;
        }

        let layout_text = layout_button.text();
        if let Some(bounds) =
            self.place_left_pill(left_cursor, left_limit, y, pill_height, &layout_text)
        {
            self.push_pill(
                &mut scene,
                bounds,
                PillSpec::from(layout_button),
                interaction,
                palette,
                Some(bar),
            );
            left_cursor = bounds.right() + gap;
        }

        for control in layout_choices {
            let text = control.text();
            let Some(bounds) = self.place_left_pill(left_cursor, left_limit, y, pill_height, &text)
            else {
                break;
            };
            self.push_pill(
                &mut scene,
                bounds,
                PillSpec::from(control),
                interaction,
                palette,
                Some(bar),
            );
            left_cursor = bounds.right() + gap;
        }

        if let Some(client_name) = client_name {
            let client_left = left_cursor.max(content_left);
            let client_right = right_cursor.min(content_right);
            let available = client_right - client_left;
            if available >= self.minimum_visible_width() {
                self.push_client_title(
                    &mut scene,
                    Rect::new(client_left, y, available, pill_height),
                    &client_name.value,
                    client_icon.as_ref(),
                    palette,
                );
            }
        }

        if let Some(bounds) = dock_bounds {
            self.push_dock_hover_title(&mut scene, bounds, view, interaction, left_cursor, gap);
        }

        scene
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
                *size > 0.0 && bounds.width - (*size + gap) >= self.minimum_visible_width()
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
        content_left: f32,
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
        let available = (dock.x - gap - content_left).max(0.0);
        if available < self.minimum_visible_width() {
            return;
        }
        let padding = self.horizontal_text_padding().min(8.0);
        let natural = self.measurer.measure(title, self.font_size()).width + padding * 2.0;
        let width = natural.min(180.0).min(available);
        let bounds = Rect::new(dock.x - gap - width, dock.y, width, dock.height);
        let text = self.fit_text(title, width, padding);
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
            radius: finite_non_negative(self.config.dock_corner_radius).min(bounds.height * 0.5),
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

    /// Paint a right-aligned macOS-style minimized-window shelf.
    ///
    /// Every card owns a fixed slot large enough for peak magnification. The
    /// pointer therefore grows the hovered card and its neighbours without
    /// shifting client text or status controls between frames.
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
        let metrics = DockMetrics::new(&self.config, available.height)?;
        let max_slots = metrics.slot_capacity(available.width);
        if max_slots == 0 {
            return None;
        }

        let empty_fallback = controls.is_empty() && !upstream_overflow;
        let local_overflow = controls.len() > max_slots;
        let show_overflow = upstream_overflow || local_overflow;
        let item_capacity = max_slots.saturating_sub(usize::from(show_overflow));
        let first = controls.len().saturating_sub(item_capacity);
        let visible = &controls[first..];
        let slot_count = visible.len() + usize::from(show_overflow || empty_fallback);

        let width = metrics.width_for_slots(slot_count);
        let candidate = Rect::new(
            available.right() - width,
            available.y,
            width,
            available.height,
        );
        let bounds = candidate.intersection(available)?;
        if bounds.is_empty() {
            return None;
        }

        scene.nodes.push(SceneNode::RoundedRect {
            id: NodeId::DockShelf,
            bounds,
            radius: finite_non_negative(self.config.dock_corner_radius).min(bounds.height * 0.5),
            // An empty shelf is still a full animation target for the first
            // minimized window, but macOS only exposes its separator until a
            // real thumbnail arrives.
            fill: if empty_fallback {
                Rgba::new(0.0, 0.0, 0.0, 0.0)
            } else {
                palette.occupied
            },
            stroke: None,
            state: VisualState::default(),
        });

        if metrics.separator_width > 0.0 {
            let x = bounds.x + metrics.padding + metrics.separator_width * 0.5;
            let start = Point::new(x, bounds.y + metrics.padding);
            let end = Point::new(x, bounds.bottom() - metrics.padding);
            scene.nodes.push(SceneNode::Polyline {
                id: NodeId::DockShelf,
                bounds: Rect::new(
                    x - metrics.separator_width * 0.5,
                    start.y,
                    metrics.separator_width,
                    (end.y - start.y).max(0.0),
                ),
                points: vec![start, end],
                color: palette.muted_text,
                width: metrics.separator_width,
                state: VisualState::default(),
            });
        }

        let mut slot_x = bounds.x + metrics.padding * 2.0 + metrics.separator_width;
        let dock_pointer = match interaction.hovered() {
            Some(NodeId::MinimizedWindow(_)) => interaction.pointer(),
            _ => None,
        };
        for control in visible {
            let id = control.id;
            let center_x = slot_x + metrics.slot_width * 0.5;
            let scale = self.dock_scale(center_x, metrics.peak_scale, dock_pointer);
            let width = metrics.item_width * scale;
            let height = metrics.item_height * scale;
            let item_bounds = Rect::new(
                center_x - width * 0.5,
                bounds.y + (bounds.height - height) * 0.5,
                width,
                height,
            );
            let hovered = interaction.hovered() == Some(id);
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
                id,
                bounds: item_bounds,
                radius: finite_non_negative(self.config.dock_corner_radius)
                    .min(item_bounds.height * 0.5),
                fill,
                stroke: state.urgent.then_some(Stroke {
                    color: palette.urgent_stroke,
                    width: 1.0,
                }),
                state,
            });
            scene.nodes.push(SceneNode::Text {
                id,
                bounds: item_bounds,
                text: control.icon.clone(),
                size: (height * 0.56).max(1.0),
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
                id,
                bounds: item_bounds,
                primary: bindings.primary,
                secondary: bindings.secondary,
                scroll_up: bindings.scroll_up,
                scroll_down: bindings.scroll_down,
            });
            slot_x += metrics.slot_pitch;
        }

        if show_overflow {
            let overflow_bounds = Rect::new(
                slot_x + (metrics.slot_width - metrics.item_width) * 0.5,
                bounds.y + (bounds.height - metrics.item_height) * 0.5,
                metrics.item_width,
                metrics.item_height,
            );
            scene.nodes.push(SceneNode::Text {
                id: NodeId::DockShelf,
                bounds: overflow_bounds,
                text: "…".to_owned(),
                size: (metrics.item_height * 0.65).max(1.0),
                color: palette.muted_text,
                align: TextAlign::Center,
                state: VisualState::default(),
            });
        }

        Some(bounds)
    }

    fn dock_reserve_width(
        &self,
        item_count: usize,
        upstream_overflow: bool,
        available_width: f32,
        available_height: f32,
    ) -> f32 {
        let Some(metrics) = DockMetrics::new(&self.config, available_height) else {
            return 0.0;
        };
        if metrics.slot_capacity(available_width) == 0 {
            return 0.0;
        }
        let slot_count =
            item_count.saturating_add(usize::from(upstream_overflow || item_count == 0));
        metrics.width_for_slots(slot_count).min(available_width)
    }

    fn dock_scale(&self, center_x: f32, peak_scale: f32, pointer: Option<Point>) -> f32 {
        let Some(pointer) = pointer else {
            return 1.0;
        };
        let radius = finite_non_negative(self.config.dock_influence_radius).max(1.0);
        let normalized = (1.0 - (pointer.x - center_x).abs() / radius).clamp(0.0, 1.0);
        // Smoothstep gives the centre a macOS-like soft plateau while keeping
        // the edge derivative zero, so neighbours settle without a snap.
        let influence = normalized * normalized * (3.0 - 2.0 * normalized);
        1.0 + (peak_scale - 1.0) * influence
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
        let bounds = clip
            .and_then(|clip| bounds.intersection(clip))
            .unwrap_or(bounds);
        if bounds.is_empty() {
            return;
        }
        spec.state.hovered |= interaction.hovered == Some(spec.id);
        let fill = if spec.state.urgent {
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
        // macOS keeps unselected items flat: occupancy shows as a translucent
        // fill only, and a stroke is reserved for the urgent state.
        let stroke = if spec.state.urgent {
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
            radius: finite_non_negative(self.config.corner_radius).min(bounds.height * 0.5),
            fill,
            stroke,
            state: spec.state,
        });

        let text_bounds = bounds.inset(self.horizontal_text_padding(), 0.0);
        let text = self.fit_text(&spec.text, text_bounds.width, 0.0);
        if !text.is_empty() && !text_bounds.is_empty() {
            scene.nodes.push(SceneNode::Text {
                id: spec.id,
                bounds: text_bounds,
                text,
                size: self.font_size(),
                color: if spec.state.selected {
                    palette.selected_text
                } else {
                    palette.text
                },
                align: TextAlign::Center,
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

        scene.hits.push(HitRegion {
            id: spec.id,
            bounds,
            primary: spec.primary,
            secondary: spec.secondary,
            scroll_up: spec.scroll_up,
            scroll_down: spec.scroll_down,
        });
    }

    fn place_left_pill(
        &self,
        cursor: f32,
        limit: f32,
        y: f32,
        height: f32,
        text: &str,
    ) -> Option<Rect> {
        let available = (limit - cursor).max(0.0);
        if available < self.minimum_visible_width() {
            return None;
        }
        Some(Rect::new(
            cursor,
            y,
            self.pill_width(text).min(available),
            height,
        ))
    }

    fn pill_width(&self, text: &str) -> f32 {
        self.measurer.measure(text, self.font_size()).width + 2.0 * self.horizontal_text_padding()
    }

    fn fit_text(&self, text: &str, available: f32, outer_padding: f32) -> String {
        let available = (available - 2.0 * finite_non_negative(outer_padding)).max(0.0);
        if available <= 0.0 {
            return String::new();
        }
        if self.measurer.measure(text, self.font_size()).width <= available {
            return text.to_owned();
        }
        let ellipsis = "…";
        if self.measurer.measure(ellipsis, self.font_size()).width > available {
            return String::new();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let mut candidate: String = chars.iter().collect();
            candidate.push('…');
            if self.measurer.measure(&candidate, self.font_size()).width <= available {
                return candidate;
            }
        }
        ellipsis.to_owned()
    }

    fn font_size(&self) -> f32 {
        finite_non_negative(self.config.font_size).max(1.0)
    }

    fn horizontal_text_padding(&self) -> f32 {
        finite_non_negative(self.config.pill_horizontal_padding)
    }

    fn minimum_visible_width(&self) -> f32 {
        finite_non_negative(self.config.minimum_visible_width).max(1.0)
    }
}

#[derive(Debug, Clone)]
struct PillSpec {
    id: NodeId,
    text: String,
    primary: Option<UserAction>,
    secondary: Option<UserAction>,
    scroll_up: Option<UserAction>,
    scroll_down: Option<UserAction>,
    state: VisualState,
    progress: Option<f32>,
}

impl From<ControlSpec> for PillSpec {
    fn from(control: ControlSpec) -> Self {
        let text = control.text();
        let bindings = if control.state.enabled {
            control.bindings
        } else {
            crate::controls::InputBindings::default()
        };
        Self {
            id: control.id,
            text,
            primary: bindings.primary,
            secondary: bindings.secondary,
            scroll_up: bindings.scroll_up,
            scroll_down: bindings.scroll_down,
            state: VisualState {
                hovered: control.state.hovered,
                selected: control.state.selected,
                urgent: control.state.urgent,
                occupied: control.state.occupied || control.state.filled,
            },
            progress: control.progress.map(Percent::as_f32),
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

        // With the default 38 px geometry, the Dock needs 48.85 logical px
        // for its fixed chrome and first peak-sized slot. A 100 px viewport
        // leaves only 46.4 px in the right partition, so the unusable shelf
        // must not reserve that partition away from the clock.
        let narrow = engine.build(
            view(&tags, ""),
            Size::new(100.0, 38.0),
            &InteractionState::default(),
        );
        assert!(narrow.hits.iter().any(|hit| hit.id == NodeId::Clock));
        assert_eq!(narrow.bounds_for(NodeId::DockShelf), None);

        // Six more pixels put the right partition safely beyond the one-slot
        // threshold. Reservation and painting now agree, so the shelf appears.
        let one_slot = engine.build(
            view(&tags, ""),
            Size::new(106.0, 38.0),
            &InteractionState::default(),
        );
        assert!(one_slot.bounds_for(NodeId::DockShelf).is_some());
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
        assert!((hit.bounds.width / hit.bounds.height - 1.5).abs() < 0.01);
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
        assert!(hovered_first.bounds.width > first.bounds.width * 1.45);
        assert!(hovered_second.bounds.width > second.bounds.width);
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
    fn occupied_tags_have_an_accent_stroke_unless_selected() {
        let mut tags = vec![TagState::default(); 3];
        tags[1].occupied = true;
        tags[2].occupied = true;
        tags[2].selected = true;
        let scene = engine().build(
            view(&tags, "client"),
            Size::new(1600.0, 38.0),
            &InteractionState::default(),
        );
        let palette = Palette::for_theme(ThemeMode::Dark);

        let tag_rect = |index| {
            scene
                .nodes_for(NodeId::Tag(TagId::new(index).unwrap()))
                .find_map(|node| match node {
                    SceneNode::RoundedRect { fill, stroke, .. } => Some((*fill, *stroke)),
                    _ => None,
                })
                .unwrap()
        };

        assert_eq!(tag_rect(0), (palette.pill, None));
        assert_eq!(tag_rect(1), (palette.occupied, None));
        assert_eq!(tag_rect(2), (palette.selected, None));
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
}
