//! Geometry-free control projection shared by scene and widget frontends.
//!
//! [`PresentationProjector`] turns a borrowed [`BarView`] into owned semantic
//! controls. Renderers remain responsible for geometry, clipping, colors, and
//! transient pointer interaction.

use serde::{Deserialize, Serialize};

use crate::ThemeMode;
use crate::display::{MetricTone, VolumeLevel};
use crate::model::{BarView, Percent, TagId, UserAction};
use crate::presentation::{NodeId, PresentationConfig};

/// Text used when a visible metric has no value.
pub const UNKNOWN_VALUE: &str = "--";

/// Status controls in the same right-to-left order used by `LayoutEngine`.
///
/// The first visible entry is anchored at the right edge. Hidden entries are
/// omitted without changing the relative order of the remaining controls.
pub const STATUS_ORDER_RIGHT_TO_LEFT: [NodeId; 9] = [
    NodeId::Clock,
    NodeId::Screenshot,
    NodeId::Theme,
    NodeId::Battery,
    NodeId::Brightness,
    NodeId::Audio,
    NodeId::Memory,
    NodeId::Cpu,
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
    #[must_use]
    pub fn text(&self) -> String {
        match (self.icon.is_empty(), self.value.is_empty()) {
            (true, _) => self.value.clone(),
            (_, true) => self.icon.clone(),
            (false, false) => format!("{} {}", self.icon, self.value),
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
            config
                .layouts
                .iter()
                .map(|layout| {
                    control(
                        NodeId::LayoutOption(layout.id),
                        String::new(),
                        layout.label.clone(),
                        None,
                        None,
                        None,
                        ControlState::default(),
                        InputBindings {
                            primary: Some(UserAction::SetLayout(layout.id)),
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

        BarPresentation {
            theme: view.theme,
            tags,
            layout_button,
            layout_choices,
            client_name,
            status: status_controls(view, config),
        }
    }
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

    status
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
        TagState,
    };
    use crate::presentation::{LayoutChoice, PresentationVisibility};

    fn percent(value: f64) -> Percent {
        Percent::new(value).expect("valid test percentage")
    }

    fn snapshot() -> BarSnapshot {
        BarSnapshot {
            wm_available: true,
            wm_sequence: Some(7),
            tags: vec![TagState::default()],
            active_tag: None,
            monitor: MonitorId(2),
            geometry: None,
            layout_symbol: "[]=".to_owned(),
            client_name: "terminal".to_owned(),
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
        assert_eq!(control.text(), "I V");
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

    #[test]
    fn status_order_visibility_and_client_match_layout_semantics() {
        let snapshot = snapshot();
        let mut config = PresentationConfig::default();
        let all = PresentationProjector::project(snapshot.view(), &config);
        assert_eq!(
            all.status
                .iter()
                .map(|control| control.id)
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
            theme: false,
            screenshot: true,
            clock: false,
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
