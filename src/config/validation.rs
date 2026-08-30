//! Semantic configuration diagnostics.
//!
//! TOML deserialization proves that a file has the expected shape, but it does
//! not catch unreachable shortcuts, unsafe ranges, or misspelled enum-like
//! values.  Keeping those checks here gives startup, `--check-config`, and live
//! reload one source of truth.

use super::{ArgumentConfig, Config, KeyConfig, RuleConfig};
use crate::core::layout::LayoutEnum;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigDiagnosticLevel {
    Error,
    Warning,
}

impl fmt::Display for ConfigDiagnosticLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => f.write_str("error"),
            Self::Warning => f.write_str("warning"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConfigDiagnostic {
    pub level: ConfigDiagnosticLevel,
    pub path: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ConfigDiagnostic {
    fn new(
        level: ConfigDiagnosticLevel,
        path: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            level,
            path: path.into(),
            message: message.into(),
            hint,
        }
    }
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}]: {}", self.level, self.path, self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, " (hint: {hint})")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ConfigDiagnostics {
    issues: Vec<ConfigDiagnostic>,
}

impl ConfigDiagnostics {
    #[must_use]
    pub fn issues(&self) -> &[ConfigDiagnostic] {
        &self.issues
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.issues.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.issues.len()
    }

    #[must_use]
    pub fn error_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.level == ConfigDiagnosticLevel::Error)
            .count()
    }

    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.level == ConfigDiagnosticLevel::Warning)
            .count()
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.error_count() != 0
    }

    fn error(
        &mut self,
        path: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<Option<String>>,
    ) {
        self.issues.push(ConfigDiagnostic::new(
            ConfigDiagnosticLevel::Error,
            path,
            message,
            hint.into(),
        ));
    }

    fn warning(
        &mut self,
        path: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<Option<String>>,
    ) {
        self.issues.push(ConfigDiagnostic::new(
            ConfigDiagnosticLevel::Warning,
            path,
            message,
            hint.into(),
        ));
    }
}

impl fmt::Display for ConfigDiagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "configuration has {} error(s) and {} warning(s)",
            self.error_count(),
            self.warning_count()
        )?;
        for issue in &self.issues {
            write!(f, "\n  - {issue}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigDiagnostics {}

fn validate_f32_range(
    diagnostics: &mut ConfigDiagnostics,
    path: &str,
    value: f32,
    min: f32,
    max: f32,
) {
    if !value.is_finite() {
        diagnostics.error(
            path,
            format!("value {value} is not finite"),
            Some(format!("use a number in [{min}, {max}]")),
        );
    } else if !(min..=max).contains(&value) {
        diagnostics.warning(
            path,
            format!("value {value} is outside the supported range [{min}, {max}]"),
            None,
        );
    }
}

fn require_f32_range(
    diagnostics: &mut ConfigDiagnostics,
    path: &str,
    value: f32,
    min: f32,
    max: f32,
) {
    if !value.is_finite() || !(min..=max).contains(&value) {
        diagnostics.error(
            path,
            format!("value {value} is outside the required range [{min}, {max}]"),
            None,
        );
    }
}

fn validate_positive_f32(diagnostics: &mut ConfigDiagnostics, path: &str, value: f32) {
    if !value.is_finite() || value <= 0.0 {
        diagnostics.error(
            path,
            format!("value {value} must be a finite positive number"),
            None,
        );
    }
}

fn validate_positive_f64(diagnostics: &mut ConfigDiagnostics, path: &str, value: f64) {
    if !value.is_finite() || value <= 0.0 {
        diagnostics.error(
            path,
            format!("value {value} must be a finite positive number"),
            None,
        );
    }
}

fn validate_nonnegative_f32(diagnostics: &mut ConfigDiagnostics, path: &str, value: f32) {
    if !value.is_finite() || value < 0.0 {
        diagnostics.error(
            path,
            format!("value {value} must be a finite non-negative number"),
            None,
        );
    }
}

fn validate_rgba(diagnostics: &mut ConfigDiagnostics, path: &str, rgba: [f32; 4]) {
    for (index, value) in rgba.into_iter().enumerate() {
        validate_f32_range(diagnostics, &format!("{path}[{index}]"), value, 0.0, 1.0);
    }
}

fn validate_choice(diagnostics: &mut ConfigDiagnostics, path: &str, value: &str, choices: &[&str]) {
    if !choices.contains(&value) {
        diagnostics.warning(
            path,
            format!("unknown value {value:?}"),
            Some(format!("expected one of: {}", choices.join(", "))),
        );
    }
}

fn validate_choice_ignore_ascii_case(
    diagnostics: &mut ConfigDiagnostics,
    path: &str,
    value: &str,
    choices: &[&str],
) {
    if !choices
        .iter()
        .any(|choice| choice.eq_ignore_ascii_case(value))
    {
        diagnostics.warning(
            path,
            format!("unknown value {value:?}"),
            Some(format!("expected one of: {}", choices.join(", "))),
        );
    }
}

fn canonical_modifier(value: &str) -> Option<&'static str> {
    match value {
        "Mod1" | "Alt" => Some("Alt"),
        "Mod2" => Some("Mod2"),
        "Mod3" => Some("Mod3"),
        "Mod4" | "Super" | "Win" => Some("Super"),
        "Mod5" => Some("Mod5"),
        "Control" | "Ctrl" => Some("Control"),
        "Shift" => Some("Shift"),
        "Lock" | "CapsLock" => Some("CapsLock"),
        _ => None,
    }
}

fn canonical_chord(modifiers: &[String], key: &str) -> String {
    let mut canonical = modifiers
        .iter()
        .filter_map(|modifier| canonical_modifier(modifier))
        // `Jwm::clean_mask` deliberately removes lock state before shortcut
        // dispatch.  Treating CapsLock as part of the diagnostic identity
        // would therefore miss collisions that are identical at runtime.
        .filter(|modifier| *modifier != "CapsLock")
        .collect::<Vec<_>>();
    canonical.sort_unstable();
    canonical.dedup();
    if canonical.is_empty() {
        key.to_string()
    } else {
        format!("{}+{key}", canonical.join("+"))
    }
}

fn validate_modifiers(diagnostics: &mut ConfigDiagnostics, path: &str, modifiers: &[String]) {
    let mut seen = Vec::new();
    for (index, modifier) in modifiers.iter().enumerate() {
        let Some(canonical) = canonical_modifier(modifier) else {
            diagnostics.error(
                format!("{path}[{index}]"),
                format!("unknown modifier {modifier:?}"),
                Some("use Alt, Super, Control, Shift, CapsLock, or Mod1..Mod5".into()),
            );
            continue;
        };
        if canonical == "CapsLock" {
            diagnostics.warning(
                format!("{path}[{index}]"),
                format!("modifier {modifier:?} is ignored when matching shortcuts"),
                Some("lock state is normalized away; remove this modifier".into()),
            );
            continue;
        }
        if seen.contains(&canonical) {
            diagnostics.warning(
                format!("{path}[{index}]"),
                format!("modifier {modifier:?} is duplicated"),
                None,
            );
        } else {
            seen.push(canonical);
        }
    }
}

fn validate_spawn_argument(
    diagnostics: &mut ConfigDiagnostics,
    path: &str,
    argument: &ArgumentConfig,
) -> bool {
    match argument {
        ArgumentConfig::String(command) => match crate::command_line::split_command_line(command) {
            Ok(command)
                if command
                    .first()
                    .is_some_and(|program| !program.trim().is_empty()) =>
            {
                true
            }
            Ok(_) => {
                diagnostics.error(
                    path,
                    "spawn requires a non-empty command program",
                    Some("example: \"alacritty --title 'work shell'\"".into()),
                );
                false
            }
            Err(error) => {
                diagnostics.error(
                    path,
                    format!("spawn command cannot be parsed: {error}"),
                    Some("balance quotes and escapes; no shell is involved".into()),
                );
                false
            }
        },
        ArgumentConfig::StringVec(command)
            if command
                .first()
                .is_some_and(|program| !program.trim().is_empty()) =>
        {
            true
        }
        _ => {
            diagnostics.error(
                path,
                "spawn requires a non-empty command string or array",
                Some("examples: \"alacritty\" or [\"alacritty\", \"--option\"]".into()),
            );
            false
        }
    }
}

fn validate_finite_argument(
    diagnostics: &mut ConfigDiagnostics,
    path: &str,
    argument: &ArgumentConfig,
) -> bool {
    let ArgumentConfig::Float(value) = argument else {
        return true;
    };
    if value.is_finite() {
        return true;
    }
    diagnostics.error(
        path,
        format!("floating-point argument {value} is not finite"),
        Some("use a finite numeric value".into()),
    );
    false
}

fn validate_binding(
    config: &Config,
    diagnostics: &mut ConfigDiagnostics,
    path: &str,
    binding: &KeyConfig,
) -> bool {
    validate_modifiers(diagnostics, &format!("{path}.modifier"), &binding.modifier);
    let key_is_known = config.parse_keysym(&binding.key).is_some();
    if !key_is_known {
        diagnostics.warning(
            format!("{path}.key"),
            format!(
                "unknown key {:?}; this binding will be ignored",
                binding.key
            ),
            None,
        );
    }
    let function_is_known = config.parse_function(&binding.function).is_some();
    if !function_is_known {
        diagnostics.warning(
            format!("{path}.function"),
            format!(
                "unknown function {:?}; this binding will be ignored",
                binding.function
            ),
            None,
        );
    }
    let argument_path = format!("{path}.argument");
    let argument_is_finite =
        validate_finite_argument(diagnostics, &argument_path, &binding.argument);
    let argument_is_valid = argument_is_finite
        && (binding.function != "spawn"
            || validate_spawn_argument(diagnostics, &argument_path, &binding.argument));

    // These are the same gates used by `Config::convert_key_config`.
    // Invalid entries never reach the runtime binding list and therefore
    // cannot shadow a generated tag shortcut.
    key_is_known && function_is_known && argument_is_valid
}

fn validate_time(diagnostics: &mut ConfigDiagnostics, path: &str, value: &str) {
    let valid = value.split_once(':').is_some_and(|(hour, minute)| {
        hour.parse::<u8>().is_ok_and(|hour| hour < 24)
            && minute.parse::<u8>().is_ok_and(|minute| minute < 60)
            && hour.len() == 2
            && minute.len() == 2
    });
    if !valid {
        diagnostics.warning(
            path,
            format!("invalid time {value:?}"),
            Some("use 24-hour HH:MM format".into()),
        );
    }
}

fn window_rule_has_matcher(rule: &RuleConfig) -> bool {
    !rule.class.is_empty() || !rule.instance.is_empty() || !rule.name.is_empty()
}

/// Whether every value admitted by `later` is also admitted by `earlier` for
/// one of the runtime rule matcher's substring-constrained fields.
///
/// An empty earlier pattern is a wildcard. Otherwise the later pattern must
/// contain it: any attribute containing the later pattern then necessarily
/// contains the earlier one as well. A non-empty earlier pattern cannot cover
/// a later wildcard because the later rule also admits attributes without it.
fn window_rule_field_covers(earlier: &str, later: &str) -> bool {
    earlier.is_empty() || (!later.is_empty() && later.contains(earlier))
}

/// Prove that the first-match runtime semantics make `later` unreachable.
/// All three fields are conjunctive, so coverage must hold independently for
/// class, instance, and name. All-empty rules are excluded because the runtime
/// matcher explicitly rejects them instead of treating them as match-all.
fn window_rule_covers(earlier: &RuleConfig, later: &RuleConfig) -> bool {
    window_rule_has_matcher(earlier)
        && window_rule_has_matcher(later)
        && window_rule_field_covers(&earlier.class, &later.class)
        && window_rule_field_covers(&earlier.instance, &later.instance)
        && window_rule_field_covers(&earlier.name, &later.name)
}

impl Config {
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn diagnostics(&self) -> ConfigDiagnostics {
        let mut diagnostics = ConfigDiagnostics::default();
        let behavior = &self.inner.behavior;
        let layout = &self.inner.layout;

        if !(1..=31).contains(&layout.tags_length) {
            diagnostics.error(
                "layout.tags_length",
                format!(
                    "{} cannot be represented by JWM's 32-bit tag mask",
                    layout.tags_length
                ),
                Some("use a value from 1 through 31".into()),
            );
        }
        require_f32_range(&mut diagnostics, "layout.m_fact", layout.m_fact, 0.05, 0.95);

        // Per-tag layouts are usually written by JWM, but the file is a file:
        // a hand-written entry that names a layout that does not exist, or a
        // tag that does not exist, would otherwise be dropped in silence.
        for (index, entry) in layout.tags.iter().enumerate() {
            let path = format!("layout.tags[{index}]");
            if entry.tag > layout.tags_length {
                diagnostics.warning(
                    format!("{path}.tag"),
                    format!(
                        "tag {} is outside the {} configured tags",
                        entry.tag, layout.tags_length
                    ),
                    Some("tags are numbered from 1; 0 is the all-tags view".into()),
                );
            }
            for (field, name) in [("layout", &entry.layout), ("alt", &entry.alt)] {
                if !name.is_empty() && LayoutEnum::from_name(name).is_none() {
                    diagnostics.warning(
                        format!("{path}.{field}"),
                        format!("unknown layout {name:?}"),
                        Some(format!(
                            "available: {}",
                            LayoutEnum::all()
                                .iter()
                                .map(|layout| layout.0)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )),
                    );
                }
            }
            if let Some(m_fact) = entry.m_fact {
                require_f32_range(
                    &mut diagnostics,
                    &format!("{path}.m_fact"),
                    m_fact,
                    0.05,
                    0.95,
                );
            }
        }
        if self.inner.appearance.border_px > i32::MAX as u32 {
            diagnostics.error(
                "appearance.border_px",
                "value cannot be represented by the signed geometry pipeline",
                Some(format!("use at most {}", i32::MAX)),
            );
        } else if self.inner.appearance.border_px > 64 {
            diagnostics.warning(
                "appearance.border_px",
                format!(
                    "{}px is unusually large and may leave no client area",
                    self.inner.appearance.border_px
                ),
                None,
            );
        }
        if self.inner.appearance.gap_px > i32::MAX as u32 {
            diagnostics.error(
                "appearance.gap_px",
                "value cannot be represented by the signed geometry pipeline",
                Some(format!("use at most {}", i32::MAX)),
            );
        } else if self.inner.appearance.gap_px > 512 {
            diagnostics.warning(
                "appearance.gap_px",
                format!(
                    "{}px is unusually large and may leave no usable workspace",
                    self.inner.appearance.gap_px
                ),
                None,
            );
        }
        if self.inner.appearance.cursor_size > super::MAX_CURSOR_SIZE {
            diagnostics.warning(
                "appearance.cursor_size",
                format!(
                    "{}px exceeds the safe backend limit and will be clamped to {}px",
                    self.inner.appearance.cursor_size,
                    super::MAX_CURSOR_SIZE
                ),
                Some("use a value in the 24-128 range (0 = follow the environment)".into()),
            );
        }
        if self.inner.status_bar.show_bar && self.inner.appearance.status_bar_height <= 0 {
            diagnostics.error(
                "appearance.status_bar_height",
                format!(
                    "{} must be greater than zero",
                    self.inner.appearance.status_bar_height
                ),
                None,
            );
        }
        if self.inner.appearance.status_bar_padding < 0 {
            diagnostics.warning(
                "appearance.status_bar_padding",
                format!(
                    "{} is negative and may place the bar outside the output",
                    self.inner.appearance.status_bar_padding
                ),
                None,
            );
        }
        if layout.n_master == 0 {
            diagnostics.warning(
                "layout.n_master",
                "zero disables the master area",
                Some("use 1 for the conventional master/stack layout".into()),
            );
        }

        for (path, value, min, max) in [
            ("behavior.active_opacity", behavior.active_opacity, 0.0, 1.0),
            (
                "behavior.inactive_opacity",
                behavior.inactive_opacity,
                0.0,
                1.0,
            ),
            (
                "behavior.blur_temporal_mix_ratio",
                behavior.blur_temporal_mix_ratio,
                0.0,
                1.0,
            ),
            ("behavior.fade_in_step", behavior.fade_in_step, 0.0, 1.0),
            ("behavior.fade_out_step", behavior.fade_out_step, 0.0, 1.0),
            ("behavior.corner_radius", behavior.corner_radius, 0.0, 64.0),
            (
                "behavior.border_glow_radius",
                behavior.border_glow_radius,
                0.0,
                crate::backend::compositor_common::window_glow::MAX_WINDOW_GLOW_RADIUS,
            ),
            (
                "behavior.border_glow_intensity",
                behavior.border_glow_intensity,
                0.0,
                crate::backend::compositor_common::window_glow::MAX_WINDOW_GLOW_INTENSITY,
            ),
            (
                "behavior.motion_trail_opacity",
                behavior.motion_trail_opacity,
                0.0,
                1.0,
            ),
            ("behavior.inactive_dim", behavior.inactive_dim, 0.0, 1.0),
            (
                "behavior.shadow_inactive_opacity",
                behavior.shadow_inactive_opacity,
                0.0,
                1.0,
            ),
            (
                "behavior.inactive_desaturate",
                behavior.inactive_desaturate,
                0.0,
                1.0,
            ),
            (
                "behavior.border_gradient_angle",
                behavior.border_gradient_angle,
                -360.0,
                360.0,
            ),
            (
                "behavior.border_gradient_speed",
                behavior.border_gradient_speed,
                -720.0,
                720.0,
            ),
        ] {
            validate_f32_range(&mut diagnostics, path, value, min, max);
        }
        if behavior.fading {
            for (path, value) in [
                ("behavior.fade_in_step", behavior.fade_in_step),
                ("behavior.fade_out_step", behavior.fade_out_step),
            ] {
                if value.is_finite() && value <= 0.0 {
                    diagnostics.error(
                        path,
                        "fade step must be greater than zero while fading is enabled",
                        Some("disable behavior.fading or use a positive step".into()),
                    );
                }
            }
        }

        for (path, value) in [
            ("behavior.shadow_radius", behavior.shadow_radius),
            ("behavior.border_width", behavior.border_width),
            ("behavior.border_glow_radius", behavior.border_glow_radius),
            (
                "behavior.border_glow_intensity",
                behavior.border_glow_intensity,
            ),
            (
                "behavior.annotation_line_width",
                behavior.annotation_line_width,
            ),
        ] {
            validate_nonnegative_f32(&mut diagnostics, path, value);
        }
        if behavior.hdr_enabled {
            validate_positive_f32(
                &mut diagnostics,
                "behavior.hdr_peak_nits",
                behavior.hdr_peak_nits,
            );
        } else {
            validate_nonnegative_f32(
                &mut diagnostics,
                "behavior.hdr_peak_nits",
                behavior.hdr_peak_nits,
            );
        }
        if behavior.magnifier_enabled {
            validate_positive_f32(
                &mut diagnostics,
                "behavior.magnifier_radius",
                behavior.magnifier_radius,
            );
            validate_positive_f32(
                &mut diagnostics,
                "behavior.magnifier_zoom",
                behavior.magnifier_zoom,
            );
        } else {
            validate_nonnegative_f32(
                &mut diagnostics,
                "behavior.magnifier_radius",
                behavior.magnifier_radius,
            );
            validate_nonnegative_f32(
                &mut diagnostics,
                "behavior.magnifier_zoom",
                behavior.magnifier_zoom,
            );
        }

        // Time-based effects must never receive zero, negative, or non-finite
        // durations.  Those values otherwise lead to divisions by zero or an
        // invalid `Duration::from_secs_f32` in a render loop.
        for (path, value, enabled) in [
            (
                "behavior.particle_lifetime",
                behavior.particle_lifetime,
                behavior.particle_effects,
            ),
            (
                "behavior.ripple_duration",
                behavior.ripple_duration,
                behavior.ripple_on_open,
            ),
        ] {
            if enabled {
                validate_positive_f32(&mut diagnostics, path, value);
            } else {
                validate_nonnegative_f32(&mut diagnostics, path, value);
            }
            if value.is_finite() && value > 30.0 {
                diagnostics.warning(
                    path,
                    format!("{value}s exceeds the supported maximum 30s and will be clamped"),
                    Some("use at most 30 seconds".into()),
                );
            }
        }
        for (path, value, enabled) in [
            (
                "behavior.genie_duration_ms",
                behavior.genie_duration_ms,
                behavior.genie_minimize,
            ),
            (
                "behavior.focus_highlight_duration_ms",
                behavior.focus_highlight_duration_ms,
                behavior.focus_highlight,
            ),
            (
                "behavior.wallpaper_crossfade_duration_ms",
                behavior.wallpaper_crossfade_duration_ms,
                behavior.wallpaper_crossfade,
            ),
        ] {
            if enabled && value == 0 {
                diagnostics.error(path, "duration must be greater than zero", None);
            } else if value > 30_000 {
                diagnostics.warning(
                    path,
                    format!("{value}ms exceeds the supported maximum 30000ms and will be clamped"),
                    Some("use at most 30000 milliseconds".into()),
                );
            }
        }

        for (path, value, enabled, min, max) in [
            (
                "behavior.tilt_perspective",
                behavior.tilt_perspective,
                behavior.window_tilt,
                100.0,
                10_000.0,
            ),
            (
                "behavior.tilt_speed",
                behavior.tilt_speed,
                behavior.window_tilt,
                0.1,
                100.0,
            ),
            (
                "behavior.wobbly_stiffness",
                behavior.wobbly_stiffness,
                behavior.wobbly_windows,
                0.1,
                10_000.0,
            ),
            (
                "behavior.wobbly_damping",
                behavior.wobbly_damping,
                behavior.wobbly_windows,
                0.1,
                1_000.0,
            ),
            (
                "behavior.wobbly_restore_stiffness",
                behavior.wobbly_restore_stiffness,
                behavior.wobbly_windows,
                0.1,
                10_000.0,
            ),
        ] {
            if enabled {
                validate_positive_f32(&mut diagnostics, path, value);
            } else {
                validate_nonnegative_f32(&mut diagnostics, path, value);
            }
            validate_f32_range(&mut diagnostics, path, value, min, max);
        }
        for (path, value, min, max) in [
            ("behavior.tilt_amount", behavior.tilt_amount, 0.0, 0.35),
            (
                "behavior.ripple_amplitude",
                behavior.ripple_amplitude,
                0.0,
                0.1,
            ),
            (
                "behavior.particle_gravity",
                behavior.particle_gravity,
                -10_000.0,
                10_000.0,
            ),
            (
                "behavior.window_animation_scale",
                behavior.window_animation_scale,
                0.1,
                2.0,
            ),
            (
                "behavior.tab_bar_height",
                behavior.tab_bar_height,
                1.0,
                256.0,
            ),
        ] {
            validate_f32_range(&mut diagnostics, path, value, min, max);
        }

        for (path, value, supported_max) in [
            ("behavior.tilt_grid", behavior.tilt_grid, 64),
            (
                "behavior.wobbly_grid_size",
                behavior.wobbly_grid_size,
                crate::backend::compositor_common::effects::MAX_WOBBLY_SUBDIVISIONS,
            ),
            (
                "behavior.motion_trail_frames",
                behavior.motion_trail_frames,
                64,
            ),
            ("behavior.particle_count", behavior.particle_count, 4096),
        ] {
            if value > supported_max {
                diagnostics.warning(
                    path,
                    format!(
                        "{value} exceeds the supported maximum {supported_max} and will be clamped"
                    ),
                    Some(format!("use at most {supported_max}")),
                );
            }
        }
        if behavior.tilt_grid == 0 {
            diagnostics.warning(
                "behavior.tilt_grid",
                "zero subdivisions will be clamped to one",
                Some("use at least 1".into()),
            );
        }
        if behavior.wobbly_grid_size == 0 {
            diagnostics.warning(
                "behavior.wobbly_grid_size",
                "zero subdivisions will be clamped to one",
                Some("use at least 1".into()),
            );
        }
        if !behavior.gesture_swipe.is_empty() {
            validate_positive_f64(
                &mut diagnostics,
                "behavior.gesture_swipe_threshold",
                behavior.gesture_swipe_threshold,
            );
        }

        for (path, color) in [
            ("behavior.shadow_color", behavior.shadow_color),
            (
                "behavior.border_color_focused",
                behavior.border_color_focused,
            ),
            (
                "behavior.border_color_unfocused",
                behavior.border_color_unfocused,
            ),
            ("behavior.border_glow_color", behavior.border_glow_color),
            (
                "behavior.border_gradient_color_a",
                behavior.border_gradient_color_a,
            ),
            (
                "behavior.border_gradient_color_b",
                behavior.border_gradient_color_b,
            ),
            ("behavior.edge_glow_color", behavior.edge_glow_color),
            ("behavior.attention_color", behavior.attention_color),
            ("behavior.pip_border_color", behavior.pip_border_color),
            ("behavior.snap_preview_color", behavior.snap_preview_color),
            (
                "behavior.focus_highlight_color",
                behavior.focus_highlight_color,
            ),
            ("behavior.annotation_color", behavior.annotation_color),
        ] {
            validate_rgba(&mut diagnostics, path, color);
        }

        if behavior.blur_strength > 5 {
            diagnostics.warning(
                "behavior.blur_strength",
                format!("{} exceeds the supported maximum 5", behavior.blur_strength),
                None,
            );
        }
        if behavior.recording_fps == 0 || behavior.recording_fps > 240 {
            diagnostics.warning(
                "behavior.recording_fps",
                format!(
                    "{} is outside [1, 240] and will be clamped while recording",
                    behavior.recording_fps
                ),
                Some("use a value from 1 through 240".into()),
            );
        }
        if behavior.recording_max_height != 0 && behavior.recording_max_height < 64 {
            diagnostics.warning(
                "behavior.recording_max_height",
                format!(
                    "{} is too small to encode and will be ignored; recording will use the captured height",
                    behavior.recording_max_height
                ),
                Some("use 0 for native resolution, or at least 64".into()),
            );
        }
        if behavior.recording_quality > 51 {
            diagnostics.warning(
                "behavior.recording_quality",
                format!(
                    "{} is outside the encoder QP range [0, 51]",
                    behavior.recording_quality
                ),
                None,
            );
        }
        if behavior.audio_recording_sample_rate == 0 {
            diagnostics.warning(
                "behavior.audio_recording_sample_rate",
                "sample rate will be clamped when audio recording starts",
                Some("48000 is a widely supported value".into()),
            );
        }
        if !matches!(behavior.audio_recording_channels, 1 | 2) {
            diagnostics.warning(
                "behavior.audio_recording_channels",
                format!(
                    "{} is unsupported and will be clamped when audio recording starts",
                    behavior.audio_recording_channels
                ),
                Some("use 1 or 2".into()),
            );
        }
        if behavior.vrr_enabled
            && (behavior.vrr_min_fps == 0 || behavior.vrr_max_fps < behavior.vrr_min_fps)
        {
            diagnostics.error(
                "behavior.vrr_min_fps",
                format!(
                    "invalid VRR range {}..{}",
                    behavior.vrr_min_fps, behavior.vrr_max_fps
                ),
                Some("minimum must be positive and no greater than maximum".into()),
            );
        }

        validate_choice_ignore_ascii_case(
            &mut diagnostics,
            "appearance.ui_theme",
            &self.inner.appearance.ui_theme,
            &[
                "material",
                "glass",
                "glass-dark",
                "aurora",
                "nord",
                "tokyo-night",
                "paper",
            ],
        );
        validate_choice(
            &mut diagnostics,
            "behavior.vsync_method",
            &behavior.vsync_method,
            &["global", "oml_sync_control", "present"],
        );
        validate_choice(
            &mut diagnostics,
            "behavior.tone_mapping_method",
            &behavior.tone_mapping_method,
            &["none", "reinhard", "aces"],
        );
        validate_choice(
            &mut diagnostics,
            "behavior.transition_mode",
            &behavior.transition_mode,
            &[
                "none",
                "slide",
                "cube",
                "fade",
                "flip",
                "zoom",
                "stack",
                "blinds",
                "coverflow",
                "helix",
                "portal",
                "book",
            ],
        );
        validate_choice_ignore_ascii_case(
            &mut diagnostics,
            "behavior.new_client_position",
            &behavior.new_client_position,
            &["master", "tail", "after_focused"],
        );
        validate_choice_ignore_ascii_case(
            &mut diagnostics,
            "behavior.client_moveresize",
            &behavior.client_moveresize,
            &["always", "floating-only", "never"],
        );
        validate_choice_ignore_ascii_case(
            &mut diagnostics,
            "behavior.wallpaper_mode",
            &behavior.wallpaper_mode,
            &["fill", "fit", "stretch", "center"],
        );
        validate_choice(
            &mut diagnostics,
            "behavior.colorblind_mode",
            &behavior.colorblind_mode,
            &["", "deuteranopia", "protanopia", "tritanopia"],
        );
        validate_choice(
            &mut diagnostics,
            "behavior.recording_encoder",
            &behavior.recording_encoder,
            &["auto", "nvenc", "vaapi", "software"],
        );
        validate_choice(
            &mut diagnostics,
            "behavior.audio_recording_backend",
            &behavior.audio_recording_backend,
            &["auto", "direct", "ffmpeg"],
        );
        validate_choice(
            &mut diagnostics,
            "behavior.audio_recording_format",
            &behavior.audio_recording_format,
            &["wav", "flac", "opus", "mp3"],
        );
        validate_choice(
            &mut diagnostics,
            "animation.easing",
            &self.inner.animation.easing,
            &[
                "linear",
                "ease-in",
                "ease-out",
                "ease-in-out",
                "bounce",
                "elastic",
            ],
        );
        validate_choice(
            &mut diagnostics,
            "animation.speed",
            &self.inner.animation.speed,
            &["slow", "normal", "fast", "instant"],
        );
        validate_time(
            &mut diagnostics,
            "behavior.night_light_start",
            &behavior.night_light_start,
        );
        validate_time(
            &mut diagnostics,
            "behavior.night_light_end",
            &behavior.night_light_end,
        );
        for (field, command) in [
            ("suspend_command", &behavior.suspend_command),
            ("hibernate_command", &behavior.hibernate_command),
            ("reboot_command", &behavior.reboot_command),
            ("shutdown_command", &behavior.shutdown_command),
        ] {
            let path = format!("behavior.{field}");
            match crate::command_line::split_command_line(command) {
                Ok(argv) if argv.first().is_none_or(|program| program.trim().is_empty()) => {
                    diagnostics.warning(
                        path,
                        "session action has no command program and cannot run",
                        Some(
                            "configure a direct argv command; shell operators are not evaluated"
                                .into(),
                        ),
                    );
                }
                Err(error) => diagnostics.warning(
                    path,
                    format!("session command cannot be parsed: {error}"),
                    Some(
                        "balance quotes and escapes; the command is parsed without a shell".into(),
                    ),
                ),
                Ok(_) => {}
            }
        }

        if self.inner.status_bar.show_bar && self.inner.status_bar.name.trim().is_empty() {
            diagnostics.warning(
                "status_bar.name",
                "status bar is enabled but its executable name is empty",
                Some("set status_bar.show_bar=false or provide an executable".into()),
            );
        }

        validate_modifiers(
            &mut diagnostics,
            "keybindings.modkey",
            std::slice::from_ref(&self.inner.keybindings.modkey),
        );
        let mut runtime_chords: HashMap<String, (usize, &str)> = HashMap::new();
        for (index, binding) in self.inner.keybindings.keys.iter().enumerate() {
            let path = format!("keybindings.keys[{index}]");
            let reaches_runtime = validate_binding(self, &mut diagnostics, &path, binding);
            let chord = canonical_chord(&binding.modifier, &binding.key);
            if reaches_runtime {
                if let Some((previous_index, previous_function)) = runtime_chords.get(&chord) {
                    diagnostics.warning(
                        path,
                        format!(
                            "shortcut {chord} is already assigned to {previous_function:?} at keybindings.keys[{previous_index}]; {:?} is unreachable",
                            binding.function
                        ),
                        Some("assign a unique modifier/key combination".into()),
                    );
                } else {
                    runtime_chords.insert(chord, (index, &binding.function));
                }
            }
        }

        // `Config::get_keys` appends four bindings for each of keys 1..9
        // after the configured bindings. Dispatch selects the first match, so
        // a configured collision silently makes the generated tag action
        // unreachable. Compare using the same lock-free modifier identity as
        // runtime dispatch, including aliases such as Mod1/Alt.
        let generated_tag_variants: &[(&[&str], &str)] = &[
            (&[], "view"),
            (&["Control"], "toggleview"),
            (&["Shift"], "tag"),
            (&["Control", "Shift"], "toggletag"),
        ];
        for tag_index in 0..self.tags_length().min(9) {
            let key = (tag_index + 1).to_string();
            for (extra_modifiers, generated_function) in generated_tag_variants {
                let mut modifiers = vec![self.inner.keybindings.modkey.clone()];
                modifiers.extend(
                    extra_modifiers
                        .iter()
                        .map(|modifier| (*modifier).to_string()),
                );
                let chord = canonical_chord(&modifiers, &key);
                if let Some((index, configured_function)) = runtime_chords.get(&chord) {
                    diagnostics.warning(
                        format!("keybindings.keys[{index}]"),
                        format!(
                            "shortcut {chord} assigned to {configured_function:?} shadows JWM's generated {generated_function:?} shortcut for tag {}; the generated action is unreachable",
                            tag_index + 1
                        ),
                        Some(
                            "move this binding or change keybindings.modkey so tag shortcuts remain reachable"
                                .into(),
                        ),
                    );
                }
            }
        }

        let chord_config = &self.inner.keybindings.chord;
        if !chord_config.leader_key.is_empty() {
            validate_modifiers(
                &mut diagnostics,
                "keybindings.chord.leader_modifier",
                &chord_config.leader_modifier,
            );
            if self.parse_keysym(&chord_config.leader_key).is_none() {
                diagnostics.warning(
                    "keybindings.chord.leader_key",
                    format!("unknown key {:?}", chord_config.leader_key),
                    None,
                );
            }
            if chord_config.timeout_ms < 100 {
                diagnostics.warning(
                    "keybindings.chord.timeout_ms",
                    format!(
                        "{}ms is too short for reliable input",
                        chord_config.timeout_ms
                    ),
                    Some("use at least 100ms".into()),
                );
            }
            let leader = canonical_chord(&chord_config.leader_modifier, &chord_config.leader_key);
            let leader_uses_lock = chord_config
                .leader_modifier
                .iter()
                .any(|modifier| canonical_modifier(modifier) == Some("CapsLock"));
            // Unlike normal/sequence bindings, the compiled leader mask is
            // compared directly with the cleaned event mask. A leader that
            // names lock state can never arm, so it cannot shadow the
            // lock-free configured shortcut either.
            if !leader_uses_lock && let Some((index, function)) = runtime_chords.get(&leader) {
                diagnostics.warning(
                    "keybindings.chord.leader_key",
                    format!(
                        "chord leader {leader} shadows {function:?} at keybindings.keys[{index}]"
                    ),
                    None,
                );
            }
            let mut sequence_keys: HashMap<String, usize> = HashMap::new();
            for (index, binding) in chord_config.bindings.iter().enumerate() {
                let path = format!("keybindings.chord.bindings[{index}]");
                let reaches_runtime = validate_binding(self, &mut diagnostics, &path, binding);
                let chord = canonical_chord(&binding.modifier, &binding.key);
                if reaches_runtime {
                    if let Some(previous) = sequence_keys.get(&chord) {
                        diagnostics.warning(
                            path,
                            format!(
                                "sequence key {chord} duplicates keybindings.chord.bindings[{previous}]"
                            ),
                            None,
                        );
                    } else {
                        sequence_keys.insert(chord, index);
                    }
                }
            }
        }

        for (index, button) in self.inner.mouse_bindings.buttons.iter().enumerate() {
            let path = format!("mouse_bindings.buttons[{index}]");
            validate_modifiers(
                &mut diagnostics,
                &format!("{path}.modifier"),
                &button.modifier,
            );
            if !matches!(button.click_type.as_str(), "ClkClientWin" | "ClkRootWin") {
                diagnostics.warning(
                    format!("{path}.click_type"),
                    format!("unknown click target {:?}", button.click_type),
                    None,
                );
            }
            if !(1..=5).contains(&button.button) {
                diagnostics.warning(
                    format!("{path}.button"),
                    format!("unsupported mouse button {}", button.button),
                    Some("use a button number from 1 through 5".into()),
                );
            }
            if self.parse_function(&button.function).is_none() {
                diagnostics.warning(
                    format!("{path}.function"),
                    format!("unknown function {:?}", button.function),
                    None,
                );
            }
            let argument_is_finite = validate_finite_argument(
                &mut diagnostics,
                &format!("{path}.argument"),
                &button.argument,
            );
            if button.function == "spawn" && argument_is_finite {
                validate_spawn_argument(
                    &mut diagnostics,
                    &format!("{path}.argument"),
                    &button.argument,
                );
            }
        }

        let tag_count = layout.tags_length;
        let tag_mask = (1..=31)
            .contains(&tag_count)
            .then(|| (1usize << tag_count) - 1);
        for (index, rule) in self.inner.rules.iter().enumerate() {
            if !window_rule_has_matcher(rule) {
                diagnostics.warning(
                    format!("rules[{index}]"),
                    "class, instance, and name are all empty, so this rule can never match",
                    Some("set at least one of class, instance, or name".into()),
                );
            }
            if tag_mask.is_some_and(|mask| rule.tags & !mask != 0) {
                diagnostics.warning(
                    format!("rules[{index}].tags"),
                    format!(
                        "mask 0x{:x} references tags beyond tags_length={tag_count}",
                        rule.tags
                    ),
                    None,
                );
            }
            if rule.monitor < -1 {
                diagnostics.warning(
                    format!("rules[{index}].monitor"),
                    format!("{} must be -1 or a non-negative index", rule.monitor),
                    None,
                );
            }
        }

        // Runtime applies the first matching window rule and stops. Report a
        // later rule only when substring-constraint containment proves that an
        // earlier rule accepts every window the later rule could accept.
        for (later_index, later) in self.inner.rules.iter().enumerate() {
            if !window_rule_has_matcher(later) {
                continue;
            }
            if let Some((earlier_index, _)) = self.inner.rules[..later_index]
                .iter()
                .enumerate()
                .find(|(_, earlier)| window_rule_covers(earlier, later))
            {
                diagnostics.warning(
                    format!("rules[{later_index}]"),
                    format!(
                        "rule is unreachable because rules[{earlier_index}] matches every window this rule can match and is evaluated first"
                    ),
                    Some(
                        "remove the duplicate, move this rule earlier, or narrow the earlier rule"
                            .into(),
                    ),
                );
            }
        }

        for (index, wallpaper) in behavior.wallpaper_tags.iter().enumerate() {
            if wallpaper.tag as usize >= tag_count {
                diagnostics.warning(
                    format!("behavior.wallpaper_tags[{index}].tag"),
                    format!("{} is outside tags_length={tag_count}", wallpaper.tag),
                    None,
                );
            }
            if wallpaper.monitor < -1 {
                diagnostics.warning(
                    format!("behavior.wallpaper_tags[{index}].monitor"),
                    format!("{} must be -1 or a non-negative index", wallpaper.monitor),
                    None,
                );
            }
            if !wallpaper.mode.is_empty() {
                validate_choice_ignore_ascii_case(
                    &mut diagnostics,
                    &format!("behavior.wallpaper_tags[{index}].mode"),
                    &wallpaper.mode,
                    &["fill", "fit", "stretch", "center"],
                );
            }
        }
        for (index, wallpaper) in behavior.wallpaper_monitors.iter().enumerate() {
            if !wallpaper.mode.is_empty() {
                validate_choice_ignore_ascii_case(
                    &mut diagnostics,
                    &format!("behavior.wallpaper_monitors[{index}].mode"),
                    &wallpaper.mode,
                    &["fill", "fit", "stretch", "center"],
                );
            }
        }

        for (index, rule) in behavior.scrolling_column_width_rules.iter().enumerate() {
            let path = format!("behavior.scrolling_column_width_rules[{index}]");
            let Some((factor, pattern)) = rule.split_once(':') else {
                diagnostics.warning(
                    path,
                    format!("rule {rule:?} must use factor:pattern syntax"),
                    None,
                );
                continue;
            };
            if !factor
                .trim()
                .parse::<f32>()
                .is_ok_and(|factor| factor.is_finite() && factor > 0.0)
            {
                diagnostics.warning(
                    path.clone(),
                    format!("rule {rule:?} has an invalid width factor"),
                    None,
                );
            }
            if pattern.trim().is_empty() {
                diagnostics.warning(path, format!("rule {rule:?} has an empty pattern"), None);
            }
        }

        for (index, gesture) in behavior.gesture_swipe.iter().enumerate() {
            let path = format!("behavior.gesture_swipe[{index}]");
            if !(3..=5).contains(&gesture.fingers) {
                diagnostics.warning(
                    format!("{path}.fingers"),
                    format!("{} is outside the supported range [3, 5]", gesture.fingers),
                    None,
                );
            }
            validate_choice_ignore_ascii_case(
                &mut diagnostics,
                &format!("{path}.direction"),
                &gesture.direction,
                &["left", "right", "up", "down"],
            );
            if gesture.function.trim().is_empty() {
                diagnostics.warning(
                    format!("{path}.function"),
                    "gesture function is empty and will be ignored",
                    None,
                );
            } else if !crate::ipc::is_known_command(&gesture.function) {
                diagnostics.warning(
                    format!("{path}.function"),
                    format!(
                        "unknown IPC command {:?}; this gesture will be ignored",
                        gesture.function
                    ),
                    None,
                );
            }
        }

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(modifiers: &[&str], key: &str, function: &str) -> KeyConfig {
        KeyConfig {
            modifier: modifiers
                .iter()
                .map(|modifier| (*modifier).to_string())
                .collect(),
            key: key.to_string(),
            function: function.to_string(),
            argument: ArgumentConfig::Int(0),
        }
    }

    fn window_rule(class: &str, instance: &str, name: &str) -> RuleConfig {
        RuleConfig {
            class: class.into(),
            instance: instance.into(),
            name: name.into(),
            tags: 1,
            is_floating: false,
            monitor: -1,
        }
    }

    fn has_unreachable_rule_diagnostic(diagnostics: &ConfigDiagnostics, index: usize) -> bool {
        diagnostics.issues().iter().any(|issue| {
            issue.path == format!("rules[{index}]") && issue.message.contains("unreachable")
        })
    }

    #[test]
    fn all_empty_window_rule_reports_that_runtime_will_never_match_it() {
        let mut config = Config::default();
        config.inner.rules = vec![window_rule("", "", "")];

        let diagnostics = config.diagnostics();
        let issue = diagnostics
            .issues()
            .iter()
            .find(|issue| issue.path == "rules[0]")
            .expect("missing all-empty window rule diagnostic");
        assert_eq!(issue.level, ConfigDiagnosticLevel::Warning);
        assert!(issue.message.contains("class, instance, and name"));
        assert!(issue.message.contains("never match"));
        assert!(!has_unreachable_rule_diagnostic(&diagnostics, 0));
    }

    #[test]
    fn duplicate_window_rule_is_unreachable_after_the_first_match() {
        let mut config = Config::default();
        config.inner.rules = vec![
            window_rule("Firefox", "Navigator", "Private"),
            window_rule("Firefox", "Navigator", "Private"),
        ];

        let diagnostics = config.diagnostics();
        let issue = diagnostics
            .issues()
            .iter()
            .find(|issue| issue.path == "rules[1]" && issue.message.contains("rules[0]"))
            .expect("missing duplicate rule shadow diagnostic");
        assert!(issue.message.contains("unreachable"));
        assert!(issue.message.contains("every window"));
    }

    #[test]
    fn broader_window_rule_provably_shadows_a_later_narrower_rule() {
        let mut config = Config::default();
        config.inner.rules = vec![
            window_rule("Fire", "", ""),
            window_rule("Firefox", "Navigator", "Private Browsing"),
        ];

        let diagnostics = config.diagnostics();
        assert!(has_unreachable_rule_diagnostic(&diagnostics, 1));
    }

    #[test]
    fn narrower_or_orthogonal_earlier_rule_does_not_report_shadowing() {
        let mut config = Config::default();
        config.inner.rules = vec![
            // Narrow before broad: windows matching "Fire" need not contain
            // "Firefox", so the later rule remains reachable.
            window_rule("Firefox", "", ""),
            window_rule("Fire", "", ""),
            // A required earlier name cannot cover a later name wildcard even
            // though the class constraint is broader.
            window_rule("Chrom", "", "Dialog"),
            window_rule("Chrome", "", ""),
        ];

        let diagnostics = config.diagnostics();
        assert!(!has_unreachable_rule_diagnostic(&diagnostics, 1));
        assert!(!has_unreachable_rule_diagnostic(&diagnostics, 3));
    }

    #[test]
    fn lock_modifiers_are_reported_as_ignored_and_collide_with_the_lock_free_chord() {
        for lock_modifier in ["CapsLock", "Lock"] {
            let mut config = Config::default();
            config.inner.keybindings.keys = vec![
                binding(&[], "x", "quit"),
                binding(&[lock_modifier], "x", "restart"),
            ];

            let diagnostics = config.diagnostics();
            assert!(diagnostics.issues().iter().any(|issue| {
                issue.path == "keybindings.keys[1].modifier[0]"
                    && issue.message.contains("ignored when matching shortcuts")
            }));
            assert!(diagnostics.issues().iter().any(|issue| {
                issue.path == "keybindings.keys[1]" && issue.message.contains("unreachable")
            }));
        }
    }

    #[test]
    fn configured_binding_reports_generated_tag_shortcut_it_shadows() {
        let mut config = Config::default();
        config.inner.keybindings.keys = vec![binding(&["Alt"], "1", "quit")];

        let diagnostics = config.diagnostics();
        let collision = diagnostics
            .issues()
            .iter()
            .find(|issue| issue.message.contains("generated \"view\" shortcut"))
            .expect("missing generated tag shortcut collision");
        assert_eq!(collision.path, "keybindings.keys[0]");
        assert!(collision.message.contains("Alt+1"));
        assert!(collision.message.contains("tag 1"));
        assert!(collision.message.contains("unreachable"));
    }

    #[test]
    fn invalid_bindings_do_not_participate_in_runtime_shadow_diagnostics() {
        let malformed = |key: &str| KeyConfig {
            modifier: vec!["Alt".into()],
            key: key.into(),
            function: "spawn".into(),
            argument: ArgumentConfig::String("terminal 'unfinished".into()),
        };
        let mut config = Config::default();
        config.inner.keybindings.keys = vec![
            malformed("F8"),
            binding(&["Alt"], "F8", "quit"),
            malformed("F9"),
        ];
        config.inner.keybindings.chord.leader_modifier = vec!["Alt".into()];
        config.inner.keybindings.chord.leader_key = "F9".into();
        config.inner.keybindings.chord.bindings =
            vec![malformed("b"), binding(&["Alt"], "b", "restart")];

        let diagnostics = config.diagnostics();
        assert_eq!(diagnostics.error_count(), 3);
        assert!(!diagnostics.issues().iter().any(|issue| {
            issue.path == "keybindings.keys[1]" && issue.message.contains("unreachable")
        }));
        assert!(!diagnostics.issues().iter().any(|issue| {
            issue.path == "keybindings.chord.leader_key" && issue.message.contains("shadows")
        }));
        assert!(
            !diagnostics
                .issues()
                .iter()
                .any(|issue| issue.message.contains("sequence key")
                    && issue.message.contains("duplicates"))
        );
    }

    #[test]
    fn non_finite_shortcut_arguments_are_configuration_errors() {
        let mut config = Config::default();
        let mut key = binding(&["Alt"], "F8", "setcfact");
        key.argument = ArgumentConfig::Float(f32::NAN);
        config.inner.keybindings.keys = vec![key];
        config.inner.mouse_bindings.buttons[0].argument = ArgumentConfig::Float(f32::INFINITY);

        let diagnostics = config.diagnostics();

        for path in [
            "keybindings.keys[0].argument",
            "mouse_bindings.buttons[0].argument",
        ] {
            assert!(diagnostics.issues().iter().any(|issue| {
                issue.path == path
                    && issue.level == ConfigDiagnosticLevel::Error
                    && issue.message.contains("not finite")
            }));
        }
    }

    #[test]
    fn malformed_session_commands_are_reported_before_the_menu_uses_them() {
        let mut config = Config::default();
        config.inner.behavior.suspend_command = "locker --message 'unfinished".into();
        config.inner.behavior.reboot_command = "   ".into();
        config.inner.behavior.hibernate_command = "\"\" --argument".into();
        config.inner.behavior.shutdown_command = "locker --message 'safe words'".into();

        let diagnostics = config.diagnostics();
        assert!(diagnostics.issues().iter().any(|issue| {
            issue.path == "behavior.suspend_command" && issue.message.contains("cannot be parsed")
        }));
        assert!(diagnostics.issues().iter().any(|issue| {
            issue.path == "behavior.reboot_command" && issue.message.contains("no command")
        }));
        assert!(diagnostics.issues().iter().any(|issue| {
            issue.path == "behavior.hibernate_command" && issue.message.contains("no command")
        }));
        assert!(
            !diagnostics
                .issues()
                .iter()
                .any(|issue| issue.path == "behavior.shutdown_command")
        );
    }
}
