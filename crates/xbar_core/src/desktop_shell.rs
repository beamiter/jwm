//! Session-owned companion shell contract. This module has no GUI or IPC
//! dependencies: hosts and compositors share configuration and state validation.
use serde::{Deserialize, Serialize};

use crate::edge_shell::{EdgeShellConfig, RevealMode};
use crate::{BarEdge, BarSnapshot, MonitorGeometry};

pub const SHELL_PROTOCOL_VERSION: u32 = 1;
pub const MAX_SHELL_OUTPUTS: usize = 128;

/// Explicit JWM settings. Absence of the table is distinct from enabled=false:
/// an explicit disabled table suppresses every legacy companion configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopShellConfig {
    pub enabled: bool,
    pub left: RevealMode,
    pub right: RevealMode,
    pub bottom: RevealMode,
    pub reveal_delay_ms: u64,
    pub hide_delay_ms: u64,
    pub trigger_pixels: u32,
}

impl Default for DesktopShellConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            left: RevealMode::Hover,
            right: RevealMode::Hover,
            bottom: RevealMode::Hover,
            reveal_delay_ms: 80,
            hide_delay_ms: 400,
            trigger_pixels: 2,
        }
    }
}

impl DesktopShellConfig {
    /// Pin all enabled edges together, or return a fully pinned set to hover.
    /// A mixed set converges to pinned; disabled edges and ownership stay put.
    #[must_use]
    pub fn toggled_mode(&self) -> Self {
        let target = if [self.left, self.right, self.bottom].contains(&RevealMode::Hover) {
            RevealMode::Always
        } else {
            RevealMode::Hover
        };
        let toggle = |mode| {
            if mode == RevealMode::Disabled {
                mode
            } else {
                target
            }
        };
        Self {
            left: toggle(self.left),
            right: toggle(self.right),
            bottom: toggle(self.bottom),
            ..self.clone()
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.as_edge_shell().validate()
    }

    pub fn as_edge_shell(&self) -> EdgeShellConfig {
        EdgeShellConfig {
            enabled: self.enabled,
            left: self.left,
            right: self.right,
            bottom: self.bottom,
            reveal_delay_ms: self.reveal_delay_ms,
            hide_delay_ms: self.hide_delay_ms,
            trigger_pixels: self.trigger_pixels,
        }
    }

    pub fn mode(&self, edge: BarEdge) -> RevealMode {
        self.as_edge_shell().mode(edge)
    }

    /// Workspace depths in left/right/top/bottom order. Hover panels and
    /// sensors overlay the workspace; only pinned panels reserve their actual
    /// physical cross-axis extent. The primary bar owns the top reservation.
    pub fn reservation_depths(&self, output: MonitorGeometry) -> [u32; 4] {
        let depth = |edge| {
            if self.mode(edge) != RevealMode::Always {
                return 0;
            }
            let panel = crate::edge_shell::panel_geometry(output, edge, true, self.trigger_pixels);
            if edge.is_vertical() {
                panel.width
            } else {
                panel.height
            }
        };
        [
            depth(BarEdge::Left),
            depth(BarEdge::Right),
            0,
            depth(BarEdge::Bottom),
        ]
    }

    /// Resolve ownership exactly once. Legacy hover settings are considered
    /// only when an actual legacy table was found, never their implicit defaults.
    pub fn resolve(
        explicit: Option<&Self>,
        legacy_four_edge: bool,
        legacy_edge_shell: Option<&EdgeShellConfig>,
    ) -> (Self, DesktopShellSource) {
        if let Some(config) = explicit {
            return (config.clone(), DesktopShellSource::DesktopShell);
        }
        if legacy_four_edge {
            return (
                Self {
                    enabled: true,
                    left: RevealMode::Always,
                    right: RevealMode::Always,
                    bottom: RevealMode::Always,
                    ..Self::default()
                },
                DesktopShellSource::LegacyFourEdge,
            );
        }
        if let Some(config) = legacy_edge_shell {
            return (
                Self {
                    enabled: config.enabled,
                    left: config.left,
                    right: config.right,
                    bottom: config.bottom,
                    reveal_delay_ms: config.reveal_delay_ms,
                    hide_delay_ms: config.hide_delay_ms,
                    trigger_pixels: config.trigger_pixels,
                },
                DesktopShellSource::LegacyEdgeShell,
            );
        }
        (Self::default(), DesktopShellSource::Default)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopShellSource {
    #[default]
    Default,
    DesktopShell,
    LegacyFourEdge,
    LegacyEdgeShell,
}

impl DesktopShellSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::DesktopShell => "desktop_shell",
            Self::LegacyFourEdge => "legacy_four_edge",
            Self::LegacyEdgeShell => "legacy_edge_shell",
        }
    }
}

/// Complete state, obtained independently of the top bar's destructive queue.
/// Every revision is replaceable as a unit, so a host needs only one pending
/// snapshot while input and rendering are busy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellSnapshot {
    pub protocol_version: u32,
    pub session: u64,
    pub revision: u64,
    pub config: DesktopShellConfig,
    pub source: DesktopShellSource,
    pub outputs: Vec<ShellOutput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellOutput {
    pub id: i32,
    /// Session-local output slot; numeric monitor IDs may be reused.
    #[serde(default)]
    pub identity: u64,
    /// Stable connector/output key for preferences and diagnostics.
    #[serde(default)]
    pub connector: Option<String>,
    /// Actual physical output rectangle, not the padded primary-bar rectangle.
    pub geometry: MonitorGeometry,
    pub scale: f64,
    /// Combined workspace, fullscreen and monitor-lock visibility.
    pub visible: bool,
    pub snapshot: BarSnapshot,
    /// Atomically captured task identities. A raw native window number may
    /// be recycled; actions must retain its compositor-owned incarnation too.
    #[serde(default)]
    pub tasks: Vec<ShellTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellTask {
    pub id: u64,
    pub incarnation: u64,
    pub name: String,
    pub class: String,
    pub monitor: i32,
    pub is_focused: bool,
    pub is_minimized: bool,
}

impl ShellSnapshot {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.protocol_version != SHELL_PROTOCOL_VERSION {
            return Err("unsupported desktop shell protocol version");
        }
        if self.session == 0 {
            return Err("desktop shell session must be nonzero");
        }
        self.config.validate()?;
        if self.outputs.len() > MAX_SHELL_OUTPUTS {
            return Err("too many desktop shell outputs");
        }
        for (index, output) in self.outputs.iter().enumerate() {
            if output.id < 0
                || output.identity == 0
                || self.outputs[..index]
                    .iter()
                    .any(|prior| prior.id == output.id || prior.identity == output.identity)
            {
                return Err("invalid or duplicate desktop shell output ID");
            }
            if output.geometry.width == 0
                || output.geometry.height == 0
                || output.geometry.width > i32::MAX as u32
                || output.geometry.height > i32::MAX as u32
                || !output.scale.is_finite()
                || !(0.25..=8.0).contains(&output.scale)
            {
                return Err("invalid desktop shell output geometry or scale");
            }
            if output.snapshot.wm_session_id != self.session {
                return Err("desktop shell output belongs to another session");
            }
            if output.tasks.len() > 64 {
                return Err("too many desktop shell tasks");
            }
            for (task_index, task) in output.tasks.iter().enumerate() {
                if task.id == 0
                    || task.incarnation == 0
                    || task.monitor != output.id
                    || task.name.len() > 4096
                    || task.class.len() > 4096
                    || output.tasks[..task_index]
                        .iter()
                        .any(|prior| prior.id == task.id)
                {
                    return Err("invalid or duplicate desktop shell task identity");
                }
            }
        }
        Ok(())
    }
}

/// Reordered and duplicate state never rolls a rendered session backwards.
/// A reconnect must explicitly reset this cursor after obtaining its complete
/// snapshot; an unsolicited different session cannot replace live identities.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ShellRevision {
    current: Option<(u64, u64)>,
}

impl ShellRevision {
    pub fn reset(&mut self) {
        self.current = None;
    }

    pub fn accept(&mut self, snapshot: &ShellSnapshot) -> Result<bool, &'static str> {
        snapshot.validate()?;
        if let Some((session, revision)) = self.current {
            if session != snapshot.session {
                return Err("desktop shell session changed; resynchronize first");
            }
            if snapshot.revision <= revision {
                return Ok(false);
            }
        }
        self.current = Some((snapshot.session, snapshot.revision));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_edges_auto_hide_but_legacy_four_edge_stays_pinned() {
        let config = DesktopShellConfig::default();
        assert!(config.enabled);
        assert_eq!(
            [config.left, config.right, config.bottom],
            [RevealMode::Hover; 3]
        );
        let legacy = DesktopShellConfig::resolve(None, true, None).0;
        assert!(legacy.enabled);
        assert_eq!(
            [legacy.left, legacy.right, legacy.bottom],
            [RevealMode::Always; 3]
        );
    }

    #[test]
    fn mode_toggle_preserves_ownership_disabled_edges_and_timing() {
        let config = DesktopShellConfig {
            enabled: true,
            left: RevealMode::Hover,
            right: RevealMode::Disabled,
            bottom: RevealMode::Always,
            reveal_delay_ms: 123,
            hide_delay_ms: 456,
            ..Default::default()
        };
        let pinned = config.toggled_mode();
        assert_eq!(
            [pinned.left, pinned.right, pinned.bottom],
            [RevealMode::Always, RevealMode::Disabled, RevealMode::Always]
        );
        let hovered = pinned.toggled_mode();
        assert_eq!(
            [hovered.left, hovered.right, hovered.bottom],
            [RevealMode::Hover, RevealMode::Disabled, RevealMode::Hover]
        );
        assert!(hovered.enabled);
        assert_eq!((hovered.reveal_delay_ms, hovered.hide_delay_ms), (123, 456));
        let disabled = DesktopShellConfig {
            left: RevealMode::Disabled,
            right: RevealMode::Disabled,
            bottom: RevealMode::Disabled,
            ..config
        };
        assert_eq!(disabled.toggled_mode(), disabled);
    }

    fn snapshot() -> ShellSnapshot {
        ShellSnapshot {
            protocol_version: SHELL_PROTOCOL_VERSION,
            session: 17,
            revision: 1,
            config: DesktopShellConfig::default(),
            source: DesktopShellSource::Default,
            outputs: Vec::new(),
        }
    }

    #[test]
    fn explicit_disable_suppresses_both_legacy_owners() {
        let explicit = DesktopShellConfig {
            enabled: false,
            ..DesktopShellConfig::default()
        };
        let legacy = EdgeShellConfig::default();
        let (effective, source) = DesktopShellConfig::resolve(Some(&explicit), true, Some(&legacy));
        assert!(!effective.enabled);
        assert_eq!(source, DesktopShellSource::DesktopShell);
        let (effective, source) = DesktopShellConfig::resolve(None, true, Some(&legacy));
        assert!(effective.enabled);
        assert_eq!(effective.mode(BarEdge::Left), RevealMode::Always);
        assert_eq!(source, DesktopShellSource::LegacyFourEdge);
        let (effective, source) = DesktopShellConfig::resolve(None, false, Some(&legacy));
        assert_eq!(effective.mode(BarEdge::Left), RevealMode::Hover);
        assert_eq!(source, DesktopShellSource::LegacyEdgeShell);
        assert!(DesktopShellConfig::resolve(None, false, None).0.enabled);
    }

    #[test]
    fn every_mode_and_timing_roundtrips_with_unknown_fields_rejected() {
        let value = serde_json::json!({"enabled":true,"left":"always","right":"hover","bottom":"disabled","hide_delay_ms":0});
        let config: DesktopShellConfig = serde_json::from_value(value).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.mode(BarEdge::Bottom), RevealMode::Disabled);
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(
            serde_json::from_value::<DesktopShellConfig>(encoded).unwrap(),
            config
        );
        assert!(
            serde_json::from_value::<DesktopShellConfig>(
                serde_json::json!({"enabled":true,"mystery":1})
            )
            .is_err()
        );
        assert!(
            DesktopShellConfig {
                hide_delay_ms: 5_001,
                ..config
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn reconnect_is_required_for_new_sessions_and_revisions_only_advance() {
        let mut state = snapshot();
        let mut cursor = ShellRevision::default();
        assert_eq!(cursor.accept(&state), Ok(true));
        assert_eq!(cursor.accept(&state), Ok(false));
        state.revision = 0;
        assert_eq!(cursor.accept(&state), Ok(false));
        state.revision = 2;
        assert_eq!(cursor.accept(&state), Ok(true));
        state.session = 18;
        assert!(cursor.accept(&state).is_err());
        cursor.reset();
        assert_eq!(cursor.accept(&state), Ok(true));
        state.protocol_version += 1;
        assert!(cursor.accept(&state).is_err());
    }

    #[test]
    fn snapshots_reject_duplicate_outputs_invalid_scale_and_foreign_identities() {
        let mut state = snapshot();
        let mut bar = crate::model::BarModel::new(crate::model::ModelConfig::default())
            .unwrap()
            .snapshot();
        bar.wm_session_id = state.session;
        let output = ShellOutput {
            id: 0,
            identity: 7,
            connector: Some("DP-1".into()),
            geometry: MonitorGeometry {
                x: -1280,
                y: 0,
                width: 1280,
                height: 800,
            },
            scale: 1.5,
            visible: true,
            snapshot: bar,
            tasks: Vec::new(),
        };
        state.outputs.push(output.clone());
        assert!(state.validate().is_ok());
        state.outputs.push(output);
        assert!(state.validate().is_err());
        state.outputs.pop();
        state.outputs[0].scale = f64::NAN;
        assert!(state.validate().is_err());
        state.outputs[0].scale = 1.0;
        state.outputs[0].snapshot.wm_session_id += 1;
        assert!(state.validate().is_err());
        state.outputs.clear();
        state.session = 0;
        assert!(state.validate().is_err());
    }

    #[test]
    fn only_pinned_edges_reserve_the_actual_host_geometry() {
        let config = DesktopShellConfig {
            enabled: true,
            left: RevealMode::Always,
            right: RevealMode::Hover,
            bottom: RevealMode::Disabled,
            ..DesktopShellConfig::default()
        };
        let output = MonitorGeometry {
            x: -1280,
            y: 120,
            width: 1280,
            height: 800,
        };
        assert_eq!(config.reservation_depths(output), [90, 0, 0, 0]);
        assert_eq!(
            DesktopShellConfig::default().reservation_depths(output),
            [0; 4]
        );
        let tiny = MonitorGeometry {
            width: 20,
            height: 10,
            ..output
        };
        assert_eq!(config.reservation_depths(tiny), [20, 0, 0, 0]);
    }
}
