//! Geometry-free control projection shared by scene and widget frontends.
//!
//! [`PresentationProjector`] turns a borrowed [`BarView`] into owned semantic
//! controls. Renderers remain responsible for geometry, clipping, colors, and
//! transient pointer interaction.

use serde::{Deserialize, Serialize};

use crate::ThemeMode;
use crate::display::{MetricTone, VolumeLevel};
use crate::model::{
    BarView, DockItemGeometry, MediaPlayback, MediaState, NetworkState, Percent, ShellRoute, TagId,
    UserAction,
};
use crate::presentation::{NodeId, PresentationConfig};

/// Text used when a visible metric has no value.
pub const UNKNOWN_VALUE: &str = "--";

/// Fixed status controls in the same right-to-left order used by
/// `LayoutEngine`.
///
/// The first visible entry is anchored at the right edge. Hidden entries are
/// omitted without changing the relative order of the remaining controls. Any
/// configured [`NodeId::ShellHub`] entries follow this list, so they land at
/// the inner (left) edge of the status cluster.
pub const STATUS_ORDER_RIGHT_TO_LEFT: [NodeId; 11] = [
    NodeId::Clock,
    NodeId::Screenshot,
    NodeId::Theme,
    NodeId::Battery,
    NodeId::Brightness,
    NodeId::Audio,
    NodeId::Network,
    NodeId::Memory,
    NodeId::Cpu,
    NodeId::Media,
    NodeId::Monitor,
];

/// Semantic state retained independently of renderer-specific styling.
///
/// `filled` and `occupied` intentionally remain distinct: some window-manager
/// transports expose both signals and a frontend may style their combination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlState {
    pub hovered: bool,
    pub pressed: bool,
    pub selected: bool,
    pub urgent: bool,
    pub filled: bool,
    pub occupied: bool,
    pub enabled: bool,
}

/// Model actions associated with conventional pointer inputs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputBindings {
    pub primary: Option<UserAction>,
    pub secondary: Option<UserAction>,
    pub scroll_up: Option<UserAction>,
    pub scroll_down: Option<UserAction>,
}

impl InputBindings {
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.primary.is_none()
            && self.secondary.is_none()
            && self.scroll_up.is_none()
            && self.scroll_down.is_none()
    }
}

/// One owned semantic control without bounds, colors, or renderer resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlSpec {
    pub id: NodeId,
    /// Glyph or short semantic label, kept separate from the changing value.
    pub icon: String,
    /// Dynamic value text. Unknown visible metrics use [`UNKNOWN_VALUE`].
    pub value: String,
    /// Metric severity when severity applies. Unknown metrics are explicitly
    /// `Some(MetricTone::Unavailable)`; non-metric controls use `None`.
    pub tone: Option<MetricTone>,
    /// Audio's semantic band is not coerced into a metric severity.
    pub volume_level: Option<VolumeLevel>,
    /// Exact normalized progress, avoiding lossy renderer-specific floats.
    pub progress: Option<Percent>,
    /// Whether the backing model/provider value is authoritative and usable.
    pub available: bool,
    pub state: ControlState,
    pub bindings: InputBindings,
}

impl ControlSpec {
    /// Join icon and value without introducing leading or trailing whitespace.
    /// Two spaces, not one: Nerd Font icon ink regularly overflows its advance
    /// to the right, swallowing a single space and gluing icon to value.
    #[must_use]
    pub fn text(&self) -> String {
        match (self.icon.is_empty(), self.value.is_empty()) {
            (true, _) => self.value.clone(),
            (_, true) => self.icon.clone(),
            (false, false) => format!("{}  {}", self.icon, self.value),
        }
    }

    #[must_use]
    fn with_availability(mut self, available: bool) -> Self {
        self.available = available;
        self
    }

    #[must_use]
    fn with_enabled(mut self, enabled: bool) -> Self {
        self.state.enabled = enabled && !self.bindings.is_empty();
        self
    }
}

/// Complete geometry-free bar presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarPresentation {
    /// Theme remains available to renderers without retaining the source view.
    pub theme: ThemeMode,
    pub tags: Vec<ControlSpec>,
    pub layout_button: ControlSpec,
    /// Empty while the selector is closed; otherwise follows configured order.
    pub layout_choices: Vec<ControlSpec>,
    /// Omitted when hidden or empty, matching the current layout behavior.
    pub client_name: Option<ControlSpec>,
    /// Desktop icon for the focused window, when one was resolved and the
    /// title itself is visible — an icon with nothing to sit beside would be
    /// a mystery button rather than a label.
    pub client_icon: Option<crate::app_icon::AppIcon>,
    /// Stable minimized-window controls in compositor-provided order.
    pub minimized_windows: Vec<ControlSpec>,
    /// The shared fixed-capacity list omitted additional minimized windows.
    pub minimized_overflow: bool,
    /// Visible subset of [`STATUS_ORDER_RIGHT_TO_LEFT`] in right-to-left order.
    pub status: Vec<ControlSpec>,
}

/// Stateless projection from model values to semantic controls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationProjector;

impl PresentationProjector {
    #[must_use]
    pub fn project(view: BarView<'_>, config: &PresentationConfig) -> BarPresentation {
        let tags = view
            .tags
            .iter()
            .copied()
            .enumerate()
            .map_while(|(index, tag)| {
                let id = TagId::new(index)?;
                let bindings = InputBindings {
                    primary: Some(UserAction::ViewTag(id)),
                    secondary: Some(UserAction::ToggleTag(id)),
                    ..InputBindings::default()
                };
                Some(
                    control(
                        NodeId::Tag(id),
                        config
                            .tag_labels
                            .get(index)
                            .cloned()
                            .unwrap_or_else(|| index.saturating_add(1).to_string()),
                        String::new(),
                        None,
                        None,
                        None,
                        ControlState {
                            selected: tag.selected || view.active_tag == Some(id),
                            urgent: tag.urgent,
                            filled: tag.filled,
                            occupied: tag.occupied,
                            ..ControlState::default()
                        },
                        bindings,
                    )
                    .with_availability(view.wm_available)
                    .with_enabled(view.wm_available),
                )
            })
            .collect();

        let layout_button = control(
            NodeId::LayoutButton,
            String::new(),
            if view.layout_symbol.is_empty() {
                "?".to_owned()
            } else {
                view.layout_symbol.to_owned()
            },
            None,
            None,
            None,
            ControlState {
                selected: view.layout_selector_open,
                ..ControlState::default()
            },
            InputBindings {
                primary: Some(UserAction::ToggleLayoutSelector),
                ..InputBindings::default()
            },
        )
        .with_availability(view.wm_available)
        .with_enabled(view.wm_available);

        let layout_choices = if view.layout_selector_open {
            layout_menu(view, config)
        } else {
            Vec::new()
        };

        let client_name =
            (config.visibility.client_name && !view.client_name.is_empty()).then(|| {
                control(
                    NodeId::Client,
                    String::new(),
                    view.client_name.to_owned(),
                    None,
                    None,
                    None,
                    ControlState::default(),
                    InputBindings::default(),
                )
            });

        let minimized_windows = if config.visibility.minimized_windows {
            view.minimized_windows
                .iter()
                .map(|window| {
                    control(
                        NodeId::MinimizedWindow(window.token),
                        window.initial().to_string(),
                        window.title.clone(),
                        None,
                        None,
                        None,
                        ControlState {
                            urgent: window.urgent(),
                            ..ControlState::default()
                        },
                        InputBindings {
                            primary: Some(UserAction::RestoreWindow {
                                window: window.token,
                                wm_session_id: view.wm_session_id,
                                minimized_generation: view.wm_sequence.unwrap_or_default(),
                                geometry: DockItemGeometry::default(),
                            }),
                            ..InputBindings::default()
                        },
                    )
                    .with_availability(view.wm_available)
                    .with_enabled(view.wm_available)
                })
                .collect()
        } else {
            Vec::new()
        };

        let client_icon = client_name
            .is_some()
            .then(|| {
                config
                    .visibility
                    .client_icon
                    .then(|| view.client_icon.cloned())
            })
            .flatten()
            .flatten();

        BarPresentation {
            theme: view.theme,
            tags,
            layout_button,
            layout_choices,
            client_name,
            client_icon,
            minimized_windows,
            minimized_overflow: view.minimized_overflow,
            status: status_controls(view, config),
        }
    }
}

/// The open layout menu: one entry per layout the *window manager* offers.
///
/// [`PresentationConfig::layouts`] is what this bar build knows how to label,
/// and [`BarView::layout_count`] is what the compositor on the other end of the
/// transport actually has. They agree when both come from the same tree, and
/// this reconciles them when they do not, in both directions:
///
/// * a compositor with fewer layouts drops the entries it cannot enter, so the
///   menu has no button that quietly does nothing;
/// * a compositor with more gets the extra identifiers appended, labelled by
///   number, so its new layouts are still reachable from a bar that predates
///   them.
///
/// Both rely on identifiers being assigned densely from zero as layouts are
/// added — the property `canonical_layout_ids_are_dense_and_appended` pins.
fn layout_menu(view: BarView<'_>, config: &PresentationConfig) -> Vec<ControlSpec> {
    let offered = view.layout_count;
    let extra = crate::display::unknown_layout_ids(offered)
        .map(crate::LayoutId)
        // A host that configured its own catalog may already label an
        // identifier past the canonical table; listing it twice would give one
        // layout two buttons.
        .filter(|id| !config.layouts.iter().any(|layout| layout.id == *id))
        .map(|id| (id, crate::display::unknown_layout_label(id)));

    config
        .layouts
        .iter()
        .filter(|layout| crate::display::layout_is_offered(layout.id, offered))
        .map(|layout| (layout.id, layout.label.clone()))
        .chain(extra)
        .take(crate::display::MAX_LAYOUT_CHOICES)
        .map(|(id, label)| {
            control(
                NodeId::LayoutOption(id),
                String::new(),
                label,
                None,
                None,
                None,
                ControlState {
                    // Only the compositor's own answer marks the entry in use.
                    // Matching the symbol back to an identifier would guess for
                    // exactly the layouts this bar does not know.
                    selected: view.layout == Some(id),
                    ..ControlState::default()
                },
                InputBindings {
                    primary: Some(UserAction::SetLayout(id)),
                    ..InputBindings::default()
                },
            )
            .with_availability(view.wm_available)
            .with_enabled(view.wm_available)
        })
        .collect()
}

fn status_controls(view: BarView<'_>, config: &PresentationConfig) -> Vec<ControlSpec> {
    let labels = &config.labels;
    let visibility = config.visibility;
    let mut status = Vec::new();

    if visibility.clock {
        status.push(control(
            NodeId::Clock,
            labels.clock.clone(),
            view.time.to_owned(),
            None,
            None,
            None,
            ControlState::default(),
            InputBindings {
                primary: Some(UserAction::ToggleSeconds),
                ..InputBindings::default()
            },
        ));
    }
    if visibility.screenshot {
        status.push(control(
            NodeId::Screenshot,
            labels.screenshot.clone(),
            String::new(),
            None,
            None,
            None,
            ControlState::default(),
            InputBindings {
                primary: Some(UserAction::Screenshot),
                ..InputBindings::default()
            },
        ));
    }
    if visibility.theme {
        let icon = match view.theme {
            ThemeMode::Dark => &labels.theme_dark,
            ThemeMode::Light => &labels.theme_light,
        };
        status.push(control(
            NodeId::Theme,
            icon.clone(),
            String::new(),
            None,
            None,
            None,
            ControlState::default(),
            InputBindings {
                primary: Some(UserAction::ToggleTheme),
                ..InputBindings::default()
            },
        ));
    }
    if visibility.battery {
        let icon = if let Some(icons) = config.icon_set.as_ref() {
            icons.battery_icon(view.battery).to_owned()
        } else if view.battery.present && view.battery.charging {
            labels.charging.clone()
        } else {
            labels.battery.clone()
        };
        status.push(control(
            NodeId::Battery,
            icon,
            percent_value(view.battery.percent),
            Some(config.battery_thresholds.tone_for(view.battery)),
            None,
            view.battery.percent,
            ControlState::default(),
            InputBindings {
                primary: Some(UserAction::RefreshBattery),
                ..InputBindings::default()
            },
        ));
    }
    if visibility.brightness {
        status.push(control(
            NodeId::Brightness,
            labels.brightness.clone(),
            percent_value(view.brightness.percent),
            view.brightness
                .percent
                .is_none()
                .then_some(MetricTone::Unavailable),
            None,
            view.brightness.percent,
            ControlState::default(),
            InputBindings {
                primary: Some(UserAction::BrightnessUp),
                secondary: Some(UserAction::BrightnessDown),
                scroll_up: Some(UserAction::BrightnessUp),
                scroll_down: Some(UserAction::BrightnessDown),
            },
        ));
    }
    if visibility.audio {
        let level = config.volume_thresholds.level_for(view.audio);
        let icon = if let Some(icons) = config.icon_set.as_ref() {
            icons.volume_icon(level).to_owned()
        } else if view.audio.muted {
            labels.muted.clone()
        } else {
            labels.audio.clone()
        };
        status.push(control(
            NodeId::Audio,
            icon,
            percent_value(view.audio.volume_percent),
            (level == VolumeLevel::Unavailable).then_some(MetricTone::Unavailable),
            Some(level),
            view.audio.volume_percent,
            ControlState::default(),
            InputBindings {
                primary: Some(UserAction::ToggleMute),
                secondary: Some(UserAction::OpenAudioControl),
                scroll_up: Some(UserAction::VolumeUp),
                scroll_down: Some(UserAction::VolumeDown),
            },
        ));
    }
    if visibility.network {
        let icon = if view.network.connected {
            labels.network.clone()
        } else {
            labels.network_offline.clone()
        };
        status.push(
            control(
                NodeId::Network,
                icon,
                network_value(view.network),
                (!view.network.connected).then_some(MetricTone::Unavailable),
                None,
                None,
                ControlState::default(),
                InputBindings::default(),
            )
            .with_availability(view.network.connected),
        );
    }
    if visibility.system {
        status.push(control(
            NodeId::Memory,
            labels.memory.clone(),
            percent_value(view.system.memory_percent),
            Some(config.usage_thresholds.tone(view.system.memory_percent)),
            None,
            view.system.memory_percent,
            ControlState::default(),
            InputBindings::default(),
        ));
        status.push(control(
            NodeId::Cpu,
            labels.cpu.clone(),
            percent_value(view.system.cpu_percent),
            Some(config.usage_thresholds.tone(view.system.cpu_percent)),
            None,
            view.system.cpu_percent,
            ControlState::default(),
            InputBindings::default(),
        ));
    }
    if visibility.media && view.media.is_active() {
        let icon = match view.media.playback {
            MediaPlayback::Playing => labels.media_playing.clone(),
            MediaPlayback::Paused | MediaPlayback::Stopped => labels.media_paused.clone(),
        };
        status.push(control(
            NodeId::Media,
            icon,
            media_value(view.media),
            None,
            None,
            None,
            ControlState::default(),
            InputBindings::default(),
        ));
    }
    if visibility.monitor {
        let monitor_value = config.icon_set.as_ref().map_or_else(
            || view.monitor.0.to_string(),
            |icons| icons.monitor_label(view.monitor).into_owned(),
        );
        status.push(
            control(
                NodeId::Monitor,
                labels.monitor.clone(),
                monitor_value,
                None,
                None,
                None,
                ControlState::default(),
                InputBindings::default(),
            )
            .with_availability(view.wm_available),
        );
    }
    if visibility.shell_hub {
        // Appended last, so the shell entries sit at the inner edge of the
        // right-to-left status cluster. They are also the first cells the
        // layout engine drops on a narrow bar, which is the right trade: a
        // clock or a battery reading is worth more width than a launcher.
        let mut seen_routes = [false; ShellRoute::ALL.len()];
        for &route in &config.shell_routes {
            let route_index = ShellRoute::ALL
                .iter()
                .position(|candidate| *candidate == route)
                .expect("ShellRoute::ALL must enumerate every route");
            if seen_routes[route_index] {
                continue;
            }
            seen_routes[route_index] = true;
            status.push(
                control(
                    NodeId::ShellHub(route),
                    labels.shell_route(route).to_owned(),
                    String::new(),
                    None,
                    None,
                    None,
                    ControlState::default(),
                    shell_bindings(route),
                )
                // The surface lives in the window manager, so an unreachable
                // window manager must gray the entry out rather than let a
                // click vanish into a closed transport.
                .with_availability(view.wm_available)
                .with_enabled(view.wm_available),
            );
        }
    }

    status
}

/// Primary opens the route. Secondary is "up": a page returns to the hub, and
/// the hub jumps to the launcher, which is the page most hosts want first.
/// Scroll walks the pages so a single cell can reach all of them.
fn shell_bindings(route: ShellRoute) -> InputBindings {
    InputBindings {
        primary: Some(UserAction::OpenShellHub(route)),
        secondary: Some(UserAction::OpenShellHub(if route == ShellRoute::Hub {
            ShellRoute::Applications
        } else {
            ShellRoute::Hub
        })),
        scroll_up: Some(UserAction::OpenShellHub(route.previous())),
        scroll_down: Some(UserAction::OpenShellHub(route.next())),
    }
}

#[allow(clippy::too_many_arguments)]
fn control(
    id: NodeId,
    icon: String,
    value: String,
    tone: Option<MetricTone>,
    volume_level: Option<VolumeLevel>,
    progress: Option<Percent>,
    mut state: ControlState,
    bindings: InputBindings,
) -> ControlSpec {
    let available = tone != Some(MetricTone::Unavailable);
    state.enabled = !bindings.is_empty();
    ControlSpec {
        id,
        icon,
        value,
        tone,
        volume_level,
        progress,
        available,
        state,
        bindings,
    }
}

/// `↓rate ↑rate` with explicit unknowns; rates never display as a healthy 0.
fn network_value(network: &NetworkState) -> String {
    if !network.connected {
        return UNKNOWN_VALUE.to_owned();
    }
    let rate = |value: Option<u64>| {
        value.map_or_else(
            || UNKNOWN_VALUE.to_owned(),
            crate::display::format_transfer_rate,
        )
    };
    format!(
        "\u{2193}{} \u{2191}{}",
        rate(network.rx_bytes_per_second),
        rate(network.tx_bytes_per_second)
    )
}

fn media_value(media: &MediaState) -> String {
    match (media.title.as_deref(), media.artist.as_deref()) {
        (Some(title), Some(artist)) => format!("{title} \u{2014} {artist}"),
        (Some(title), None) => title.to_owned(),
        (None, _) => String::new(),
    }
}

fn percent_value(value: Option<Percent>) -> String {
    value.map_or_else(
        || UNKNOWN_VALUE.to_owned(),
        |percent| format!("{}%", percent.rounded()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AudioState, BarSnapshot, BatteryState, BrightnessState, LayoutId, MonitorId, SystemState,
        TagState, WindowToken,
    };
    use crate::presentation::{LayoutChoice, PresentationVisibility};

    fn percent(value: f64) -> Percent {
        Percent::new(value).expect("valid test percentage")
    }

    fn snapshot() -> BarSnapshot {
        BarSnapshot {
            wm_available: true,
            wm_sequence: Some(7),
            wm_session_id: 11,
            tags: vec![TagState::default()],
            active_tag: None,
            monitor: MonitorId(2),
            geometry: None,
            layout_symbol: "[]=".to_owned(),
            layout: None,
            layout_count: None,
            client_name: "terminal".to_owned(),
            client_app_id: String::new(),
            client_icon: None,
            minimized_windows: Vec::new(),
            minimized_overflow: false,
            time: "12:34".to_owned(),
            show_seconds: false,
            layout_selector_open: false,
            theme: ThemeMode::Dark,
            audio: AudioState::default(),
            audio_device: None,
            system: SystemState::default(),
            system_details: Default::default(),
            brightness: BrightnessState::default(),
            battery: BatteryState::absent(),
            network: NetworkState::disconnected(),
            media: MediaState::inactive(),
        }
    }

    fn by_id(presentation: &BarPresentation, id: NodeId) -> &ControlSpec {
        presentation
            .status
            .iter()
            .find(|control| control.id == id)
            .expect("status control exists")
    }

    #[test]
    fn text_joins_empty_parts_without_stray_whitespace() {
        let mut control = super::control(
            NodeId::Clock,
            String::new(),
            String::new(),
            None,
            None,
            None,
            ControlState::default(),
            InputBindings::default(),
        );
        assert_eq!(control.text(), "");
        control.icon = "I".to_owned();
        assert_eq!(control.text(), "I");
        control.icon.clear();
        control.value = "V".to_owned();
        assert_eq!(control.text(), "V");
        control.icon = "I".to_owned();
        assert_eq!(control.text(), "I  V");
    }

    #[test]
    fn tags_preserve_distinct_transport_state_and_existing_actions() {
        let mut snapshot = snapshot();
        snapshot.tags = vec![
            TagState {
                selected: false,
                urgent: true,
                filled: true,
                occupied: false,
            },
            TagState {
                selected: true,
                urgent: false,
                filled: false,
                occupied: true,
            },
        ];
        snapshot.active_tag = TagId::new(0);
        let config = PresentationConfig {
            tag_labels: vec!["A".to_owned()],
            ..PresentationConfig::default()
        };

        let projected = PresentationProjector::project(snapshot.view(), &config);
        assert_eq!(projected.tags[0].icon, "A");
        assert_eq!(projected.tags[1].icon, "2");
        assert!(projected.tags[0].state.selected);
        assert!(projected.tags[0].state.urgent);
        assert!(projected.tags[0].state.filled);
        assert!(!projected.tags[0].state.occupied);
        assert!(!projected.tags[1].state.filled);
        assert!(projected.tags[1].state.occupied);
        assert_eq!(
            projected.tags[0].bindings.primary,
            Some(UserAction::ViewTag(TagId::new(0).unwrap()))
        );
        assert_eq!(
            projected.tags[0].bindings.secondary,
            Some(UserAction::ToggleTag(TagId::new(0).unwrap()))
        );
        assert!(projected.tags[0].state.enabled);
        assert!(!projected.tags[0].state.hovered);
        assert!(!projected.tags[0].state.pressed);
    }

    #[test]
    fn layout_projection_is_dynamic_and_only_expands_when_open() {
        let mut snapshot = snapshot();
        snapshot.layout_symbol.clear();
        let config = PresentationConfig {
            layouts: vec![
                LayoutChoice {
                    id: LayoutId(11),
                    label: "spiral".to_owned(),
                },
                LayoutChoice {
                    id: LayoutId(42),
                    label: "deck".to_owned(),
                },
            ],
            ..PresentationConfig::default()
        };

        let closed = PresentationProjector::project(snapshot.view(), &config);
        assert_eq!(closed.layout_button.text(), "?");
        assert_eq!(
            closed.layout_button.bindings.primary,
            Some(UserAction::ToggleLayoutSelector)
        );
        assert!(closed.layout_choices.is_empty());

        snapshot.layout_selector_open = true;
        let open = PresentationProjector::project(snapshot.view(), &config);
        assert!(open.layout_button.state.selected);
        assert_eq!(
            open.layout_choices
                .iter()
                .map(|control| control.id)
                .collect::<Vec<_>>(),
            vec![
                NodeId::LayoutOption(LayoutId(11)),
                NodeId::LayoutOption(LayoutId(42))
            ]
        );
        assert_eq!(
            open.layout_choices[1].bindings.primary,
            Some(UserAction::SetLayout(LayoutId(42)))
        );
    }

    /// The stock menu is every layout the window manager reported, in cycle
    /// order, with the one in use marked from the identifier it sent.
    #[test]
    fn the_open_menu_offers_every_layout_the_window_manager_reports() {
        let mut snapshot = snapshot();
        snapshot.layout_selector_open = true;
        snapshot.layout_count = Some(crate::display::CANONICAL_LAYOUT_COUNT);
        snapshot.layout = Some(LayoutId(2));

        let open = PresentationProjector::project(snapshot.view(), &PresentationConfig::default());
        assert_eq!(
            open.layout_choices.len(),
            crate::display::CANONICAL_LAYOUT_COUNT
        );
        let selected: Vec<_> = open
            .layout_choices
            .iter()
            .filter(|choice| choice.state.selected)
            .map(|choice| choice.id)
            .collect();
        assert_eq!(selected, vec![NodeId::LayoutOption(LayoutId(2))]);
        assert!(
            open.layout_choices
                .iter()
                .all(|choice| choice.state.enabled)
        );
    }

    /// A compositor with fewer layouts than this bar knows must not leave dead
    /// entries in the menu, and one with more must still be reachable.
    #[test]
    fn the_menu_follows_the_window_manager_in_both_directions() {
        let mut snapshot = snapshot();
        snapshot.layout_selector_open = true;
        let config = PresentationConfig::default();

        snapshot.layout_count = Some(3);
        let narrowed = PresentationProjector::project(snapshot.view(), &config);
        assert_eq!(
            narrowed
                .layout_choices
                .iter()
                .map(|choice| choice.id)
                .collect::<Vec<_>>(),
            vec![
                NodeId::LayoutOption(LayoutId(0)),
                NodeId::LayoutOption(LayoutId(2)),
                NodeId::LayoutOption(LayoutId(1)),
            ],
            "only the identifiers a three-layout compositor accepts, still in cycle order"
        );

        let known = crate::display::CANONICAL_LAYOUT_COUNT;
        snapshot.layout_count = Some(known + 2);
        let widened = PresentationProjector::project(snapshot.view(), &config);
        assert_eq!(widened.layout_choices.len(), known + 2);
        let appended: Vec<_> = widened.layout_choices[known..]
            .iter()
            .map(|choice| (choice.id, choice.text()))
            .collect();
        assert_eq!(
            appended,
            vec![
                (
                    NodeId::LayoutOption(LayoutId(known as u32)),
                    format!("L{known}")
                ),
                (
                    NodeId::LayoutOption(LayoutId(known as u32 + 1)),
                    format!("L{}", known + 1)
                ),
            ]
        );
    }

    #[test]
    fn malformed_layout_counts_cannot_expand_the_menu_without_bound() {
        let mut snapshot = snapshot();
        snapshot.layout_selector_open = true;
        snapshot.layout_count = Some(usize::MAX);

        let projected =
            PresentationProjector::project(snapshot.view(), &PresentationConfig::default());

        assert_eq!(
            projected.layout_choices.len(),
            crate::display::MAX_LAYOUT_CHOICES
        );
        assert_eq!(
            projected.layout_choices.last().map(|choice| choice.id),
            Some(NodeId::LayoutOption(LayoutId(
                (crate::display::MAX_LAYOUT_CHOICES - 1) as u32
            )))
        );
    }

    /// A window manager that never reports a count — an older one, or a
    /// transport that does not carry it — leaves the bar on its own catalog
    /// rather than emptying the menu.
    #[test]
    fn a_silent_window_manager_keeps_the_compiled_catalog() {
        let mut snapshot = snapshot();
        snapshot.layout_selector_open = true;
        snapshot.layout_count = None;
        snapshot.layout = None;

        let open = PresentationProjector::project(snapshot.view(), &PresentationConfig::default());
        assert_eq!(
            open.layout_choices.len(),
            crate::display::CANONICAL_LAYOUT_COUNT
        );
        assert!(
            open.layout_choices
                .iter()
                .all(|choice| !choice.state.selected)
        );
    }

    #[test]
    fn minimized_projection_preserves_order_title_urgency_and_restore_binding() {
        let mut snapshot = snapshot();
        snapshot.minimized_overflow = true;
        snapshot.minimized_windows = vec![
            crate::MinimizedWindow {
                token: WindowToken(11),
                monitor: MonitorId(2),
                title: "Terminal".into(),
                app_id: "foot".into(),
                flags: crate::MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
            },
            crate::MinimizedWindow {
                token: WindowToken(12),
                monitor: MonitorId(3),
                title: "Urgent chat".into(),
                app_id: String::new(),
                flags: crate::MINIMIZED_WINDOW_FLAG_URGENT,
            },
        ];

        let projected =
            PresentationProjector::project(snapshot.view(), &PresentationConfig::default());
        assert!(projected.minimized_overflow);
        assert_eq!(
            projected
                .minimized_windows
                .iter()
                .map(|control| (control.id, control.icon.as_str(), control.value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (NodeId::MinimizedWindow(WindowToken(11)), "F", "Terminal"),
                (NodeId::MinimizedWindow(WindowToken(12)), "U", "Urgent chat"),
            ]
        );
        assert!(!projected.minimized_windows[0].state.urgent);
        assert!(projected.minimized_windows[1].state.urgent);
        assert_eq!(
            projected.minimized_windows[0].bindings.primary,
            Some(UserAction::RestoreWindow {
                window: WindowToken(11),
                wm_session_id: 11,
                minimized_generation: 7,
                geometry: DockItemGeometry::default(),
            })
        );
    }

    #[test]
    fn network_and_media_pills_track_connection_and_activity() {
        let mut snapshot = snapshot();
        let config = PresentationConfig::default();

        let projected = PresentationProjector::project(snapshot.view(), &config);
        let network = projected
            .status
            .iter()
            .find(|control| control.id == NodeId::Network)
            .expect("network pill is visible by default");
        assert!(!network.available);
        assert_eq!(network.value, UNKNOWN_VALUE);
        assert_eq!(network.tone, Some(MetricTone::Unavailable));
        assert!(
            !projected
                .status
                .iter()
                .any(|control| control.id == NodeId::Media),
            "inactive media must not occupy bar space"
        );

        snapshot.network = NetworkState::connected("wlan0", Some(1_572_864), None);
        snapshot.media = MediaState {
            playback: MediaPlayback::Paused,
            title: Some("track".to_owned()),
            artist: Some("artist".to_owned()),
            player: Some("mpv".to_owned()),
        };
        let projected = PresentationProjector::project(snapshot.view(), &config);
        let network = projected
            .status
            .iter()
            .find(|control| control.id == NodeId::Network)
            .unwrap();
        assert!(network.available);
        assert_eq!(network.value, "\u{2193}1.5 MiB/s \u{2191}--");
        let media = projected
            .status
            .iter()
            .find(|control| control.id == NodeId::Media)
            .unwrap();
        assert_eq!(media.value, "track \u{2014} artist");
        assert_eq!(media.icon, config.labels.media_paused);
    }

    #[test]
    fn status_order_visibility_and_client_match_layout_semantics() {
        let mut snapshot = snapshot();
        // Media only appears while a player is active; activate it so the
        // complete status order is projected.
        snapshot.media = MediaState {
            playback: MediaPlayback::Playing,
            title: Some("song".to_owned()),
            artist: Some("artist".to_owned()),
            player: Some("test".to_owned()),
        };
        let mut config = PresentationConfig::default();
        let all = PresentationProjector::project(snapshot.view(), &config);
        assert_eq!(
            // Shell entries are appended after the fixed cluster and are
            // covered separately; this case is about the fixed order alone.
            all.status
                .iter()
                .map(|control| control.id)
                .filter(|id| !matches!(id, NodeId::ShellHub(_)))
                .collect::<Vec<_>>(),
            STATUS_ORDER_RIGHT_TO_LEFT
        );
        assert_eq!(all.client_name.as_ref().unwrap().value, "terminal");

        config.visibility = PresentationVisibility {
            client_name: false,
            monitor: true,
            system: false,
            audio: true,
            brightness: false,
            battery: true,
            network: false,
            media: false,
            theme: false,
            screenshot: true,
            clock: false,
            shell_hub: false,
            ..PresentationVisibility::default()
        };
        let filtered = PresentationProjector::project(snapshot.view(), &config);
        assert_eq!(
            filtered
                .status
                .iter()
                .map(|control| control.id)
                .collect::<Vec<_>>(),
            vec![
                NodeId::Screenshot,
                NodeId::Battery,
                NodeId::Audio,
                NodeId::Monitor,
            ]
        );
        assert!(filtered.client_name.is_none());
    }

    #[test]
    fn shell_entries_are_optional_and_land_left_of_the_status_cluster() {
        let snapshot = snapshot();
        let mut config = PresentationConfig::default();
        assert!(
            config.visibility.shell_hub,
            "the default bar exposes the shell"
        );

        config.visibility.shell_hub = false;
        let hidden: Vec<_> = PresentationProjector::project(snapshot.view(), &config)
            .status
            .iter()
            .map(|control| control.id)
            .collect();
        assert!(!hidden.iter().any(|id| matches!(id, NodeId::ShellHub(_))));

        config.visibility.shell_hub = true;
        config.shell_routes = vec![ShellRoute::Hub, ShellRoute::Notifications];
        let shown: Vec<_> = PresentationProjector::project(snapshot.view(), &config)
            .status
            .iter()
            .map(|control| control.id)
            .collect();

        let split = hidden.len();
        assert_eq!(
            &shown[..split],
            hidden,
            "shell entries must not reorder or displace the fixed status controls"
        );
        assert_eq!(
            &shown[split..],
            [
                NodeId::ShellHub(ShellRoute::Hub),
                NodeId::ShellHub(ShellRoute::Notifications),
            ],
            "configured routes keep their order at the inner edge of the cluster"
        );
    }

    #[test]
    fn duplicate_shell_routes_project_once_in_first_configured_order() {
        let snapshot = snapshot();
        let unique_config = PresentationConfig {
            shell_routes: vec![
                ShellRoute::Notifications,
                ShellRoute::Hub,
                ShellRoute::Clipboard,
            ],
            ..PresentationConfig::default()
        };
        let expected = PresentationProjector::project(snapshot.view(), &unique_config);
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
        let projected = PresentationProjector::project(snapshot.view(), &duplicate_config);

        assert_eq!(
            projected, expected,
            "duplicates must not change the projection produced by the first occurrences"
        );
        assert_eq!(
            projected
                .status
                .iter()
                .filter_map(|control| match control.id {
                    NodeId::ShellHub(route) => Some(route),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [
                ShellRoute::Notifications,
                ShellRoute::Hub,
                ShellRoute::Clipboard,
            ]
        );
    }

    #[test]
    fn shell_entries_reach_every_route_from_one_cell() {
        let snapshot = snapshot();
        let config = PresentationConfig {
            visibility: PresentationVisibility {
                shell_hub: true,
                ..PresentationVisibility::default()
            },
            shell_routes: vec![ShellRoute::Hub],
            ..PresentationConfig::default()
        };
        let projected = PresentationProjector::project(snapshot.view(), &config);
        let hub = projected
            .status
            .iter()
            .find(|control| control.id == NodeId::ShellHub(ShellRoute::Hub))
            .expect("hub entry");

        assert_eq!(
            hub.bindings.primary,
            Some(UserAction::OpenShellHub(ShellRoute::Hub))
        );
        assert_eq!(
            hub.bindings.secondary,
            Some(UserAction::OpenShellHub(ShellRoute::Applications))
        );

        // Walking scroll_down from the hub must visit every page and return.
        let mut route = ShellRoute::Hub;
        let mut seen = vec![route];
        for _ in 1..ShellRoute::ALL.len() {
            route = route.next();
            seen.push(route);
        }
        assert_eq!(route.next(), ShellRoute::Hub);
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ShellRoute::ALL.len());

        // A page's secondary binding goes back up to the hub.
        let config = PresentationConfig {
            shell_routes: vec![ShellRoute::Clipboard],
            ..config
        };
        let projected = PresentationProjector::project(snapshot.view(), &config);
        let clipboard = projected
            .status
            .iter()
            .find(|control| control.id == NodeId::ShellHub(ShellRoute::Clipboard))
            .expect("clipboard entry");
        assert_eq!(
            clipboard.bindings.secondary,
            Some(UserAction::OpenShellHub(ShellRoute::Hub))
        );
    }

    #[test]
    fn shell_entries_are_disabled_while_the_window_manager_is_unreachable() {
        let mut snapshot = snapshot();
        let config = PresentationConfig {
            visibility: PresentationVisibility {
                shell_hub: true,
                ..PresentationVisibility::default()
            },
            ..PresentationConfig::default()
        };

        let connected = PresentationProjector::project(snapshot.view(), &config);
        let entry = connected
            .status
            .iter()
            .find(|control| matches!(control.id, NodeId::ShellHub(_)))
            .expect("hub entry");
        assert!(entry.state.enabled);
        assert!(entry.available);

        snapshot.wm_available = false;
        let offline = PresentationProjector::project(snapshot.view(), &config);
        let entry = offline
            .status
            .iter()
            .find(|control| matches!(control.id, NodeId::ShellHub(_)))
            .expect("hub entry");
        assert!(
            !entry.state.enabled,
            "a click must not vanish into a closed transport"
        );
        assert!(!entry.available);
    }

    #[test]
    fn unknown_metrics_are_visible_unavailable_and_have_no_progress() {
        let snapshot = snapshot();
        let projected =
            PresentationProjector::project(snapshot.view(), &PresentationConfig::default());

        for id in [
            NodeId::Battery,
            NodeId::Brightness,
            NodeId::Audio,
            NodeId::Memory,
            NodeId::Cpu,
        ] {
            let control = by_id(&projected, id);
            assert_eq!(control.value, UNKNOWN_VALUE);
            assert_eq!(control.tone, Some(MetricTone::Unavailable));
            assert_eq!(control.progress, None);
            assert!(!control.available);
        }
        assert_eq!(
            by_id(&projected, NodeId::Audio).volume_level,
            Some(VolumeLevel::Unavailable)
        );
    }

    #[test]
    fn metrics_keep_exact_progress_tones_levels_and_actions() {
        let mut snapshot = snapshot();
        snapshot.system = SystemState::new(Some(percent(80.01)), Some(percent(30.0)));
        snapshot.brightness = BrightnessState::new(Some(percent(49.49)));
        snapshot.battery = BatteryState::present(Some(percent(20.0)), true);
        snapshot.audio = AudioState::new(Some(percent(50.0)), false);

        let projected =
            PresentationProjector::project(snapshot.view(), &PresentationConfig::default());
        assert_eq!(
            by_id(&projected, NodeId::Cpu).tone,
            Some(MetricTone::Critical)
        );
        assert_eq!(
            by_id(&projected, NodeId::Memory).tone,
            Some(MetricTone::Good)
        );
        assert_eq!(
            by_id(&projected, NodeId::Battery).tone,
            Some(MetricTone::Critical)
        );
        assert_eq!(
            by_id(&projected, NodeId::Battery).progress,
            Some(percent(20.0))
        );
        assert!(by_id(&projected, NodeId::Battery).available);
        assert_eq!(by_id(&projected, NodeId::Brightness).value, "49%");
        assert_eq!(by_id(&projected, NodeId::Brightness).tone, None);
        assert_eq!(
            by_id(&projected, NodeId::Audio).volume_level,
            Some(VolumeLevel::Medium)
        );
        assert_eq!(
            by_id(&projected, NodeId::Audio).bindings,
            InputBindings {
                primary: Some(UserAction::ToggleMute),
                secondary: Some(UserAction::OpenAudioControl),
                scroll_up: Some(UserAction::VolumeUp),
                scroll_down: Some(UserAction::VolumeDown),
            }
        );
        assert!(!by_id(&projected, NodeId::Cpu).state.enabled);
        assert!(by_id(&projected, NodeId::Audio).state.enabled);
    }

    #[test]
    fn projector_honors_custom_thresholds_and_wm_availability() {
        let mut snapshot = snapshot();
        snapshot.wm_available = false;
        snapshot.system = SystemState::new(Some(percent(25.0)), None);
        let config = PresentationConfig {
            usage_thresholds: crate::UsageThresholds::new(
                percent(10.0),
                percent(20.0),
                percent(30.0),
            )
            .unwrap(),
            ..PresentationConfig::default()
        };

        let projected = PresentationProjector::project(snapshot.view(), &config);

        assert_eq!(by_id(&projected, NodeId::Cpu).tone, Some(MetricTone::High));
        assert!(!projected.tags[0].available);
        assert!(!projected.tags[0].state.enabled);
        assert!(!projected.layout_button.available);
        assert!(!projected.layout_button.state.enabled);
        assert!(!by_id(&projected, NodeId::Monitor).available);
    }

    #[test]
    fn projector_uses_band_aware_icons_when_a_preset_is_installed() {
        let mut snapshot = snapshot();
        snapshot.audio = AudioState::new(Some(percent(50.0)), false);
        let icons = crate::IconSet::nerd_font();
        let config = PresentationConfig::default().with_icon_set(&icons);

        let projected = PresentationProjector::project(snapshot.view(), &config);

        assert_eq!(by_id(&projected, NodeId::Audio).icon, icons.volume_medium);
        assert_eq!(by_id(&projected, NodeId::Monitor).value, "M2");
        assert_eq!(projected.tags[0].icon, icons.tag_icon(0));
    }
}
