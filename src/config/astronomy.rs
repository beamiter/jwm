//! Bounded, backend-neutral celestial wallpaper configuration.
//!
//! Output keys are physical connector names, never enumeration indices. Saved
//! assignments for disconnected outputs remain available for the next hotplug.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const MAX_ASTRONOMY_OUTPUTS: usize = 128;

/// Canonical scene tokens shared by the picker, hot setters and renderer.
pub(crate) const ASTRONOMY_SCENES: [(&str, &str); 12] = [
    ("earth", "Earth"),
    ("moon", "Moon"),
    ("sun", "Sun"),
    ("mercury", "Mercury"),
    ("venus", "Venus"),
    ("mars", "Mars"),
    ("jupiter", "Jupiter"),
    ("saturn", "Saturn"),
    ("uranus", "Uranus"),
    ("neptune", "Neptune"),
    ("solar-system", "Solar System 3D"),
    ("galaxy", "Milky Way panorama"),
];

pub(crate) const ASTRONOMY_LAYOUTS: [&str; 5] =
    ["span", "mirror", "independent", "per-output", "earth-moon"];

/// One output's scene in `independent` layout. A nonempty shared group spans
/// its connected members; an empty group owns one canvas and interaction state.
/// Every member of a group must name the same scene.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AstronomyOutputConfig {
    pub scene: String,
    #[serde(default)]
    pub group: String,
}

impl AstronomyOutputConfig {
    pub fn independent(scene: impl Into<String>) -> Self {
        Self {
            scene: scene.into(),
            group: String::new(),
        }
    }
}

pub(crate) fn is_scene(scene: &str) -> bool {
    ASTRONOMY_SCENES.iter().any(|(key, _)| *key == scene)
}

pub(crate) fn scene_label(scene: &str) -> &str {
    ASTRONOMY_SCENES
        .iter()
        .find_map(|(key, label)| (*key == scene).then_some(*label))
        .unwrap_or(scene)
}

pub(crate) fn outputs_error(
    outputs: &BTreeMap<String, AstronomyOutputConfig>,
) -> Option<&'static str> {
    if outputs.len() > MAX_ASTRONOMY_OUTPUTS {
        return Some("at most 128 connector assignments are allowed");
    }
    let mut groups = BTreeMap::<&str, &str>::new();
    for (connector, output) in outputs {
        if connector.trim().is_empty()
            || connector.len() > 256
            || connector.chars().any(char::is_control)
        {
            return Some("connector names must contain 1..=256 bytes and no control characters");
        }
        if !is_scene(&output.scene) {
            return Some("scene must name a supported celestial body, solar-system, or galaxy");
        }
        if output.group.len() > 128
            || output.group.chars().any(char::is_control)
            || (!output.group.is_empty() && output.group.trim().is_empty())
        {
            return Some(
                "group names must be empty or contain 1..=128 bytes and no control characters",
            );
        }
        if !output.group.is_empty()
            && let Some(scene) = groups.insert(&output.group, &output.scene)
            && scene != output.scene
        {
            return Some("outputs in the same shared group must use the same scene");
        }
    }
    None
}

/// Select a body for one independent output or its explicitly shared scene.
/// Unrelated outputs, including disconnected ones, are left byte-for-byte equal.
pub(crate) fn set_output_scene(
    outputs: &mut BTreeMap<String, AstronomyOutputConfig>,
    connector: &str,
    scene: &str,
) {
    let group = outputs
        .get(connector)
        .map(|output| output.group.clone())
        .unwrap_or_default();
    if group.is_empty() {
        outputs.insert(connector.into(), AstronomyOutputConfig::independent(scene));
    } else {
        for output in outputs.values_mut().filter(|output| output.group == group) {
            output.scene = scene.into();
        }
    }
}

/// Move one output into another output's shared canvas. Joining an existing
/// group reuses it; joining a standalone output creates a collision-free id.
pub(crate) fn join_output_scene(
    outputs: &mut BTreeMap<String, AstronomyOutputConfig>,
    connector: &str,
    reference: &str,
    fallback_scene: &str,
) {
    if connector == reference {
        return;
    }
    let reference_config = outputs
        .get(reference)
        .cloned()
        .unwrap_or_else(|| AstronomyOutputConfig::independent(fallback_scene));
    let group = if reference_config.group.is_empty() {
        let mut serial = 1;
        loop {
            let candidate = format!("shared-{serial}");
            if !outputs.values().any(|output| output.group == candidate) {
                break candidate;
            }
            serial += 1;
        }
    } else {
        reference_config.group
    };
    let shared = AstronomyOutputConfig {
        scene: reference_config.scene,
        group,
    };
    outputs.insert(reference.into(), shared.clone());
    outputs.insert(connector.into(), shared);
}

/// The celestial picker owns this closed set of configuration keys. Image
/// choices retain the existing session/daily semantics.
pub(super) const PERSISTED_KEYS: [&str; 11] = [
    "wallpaper_effect",
    "wallpaper_astronomy_scene",
    "wallpaper_astronomy_layout",
    "wallpaper_astronomy_outputs",
    "wallpaper_astronomy_presentation",
    "wallpaper_solar_orbit_mode",
    "wallpaper_solar_time_scale",
    "wallpaper_solar_time_offset_days",
    "wallpaper_earth_view",
    "wallpaper_moon_view",
    "wallpaper_solar_spin_speed",
];

fn inline_value(value: &serde_json::Value) -> Result<String, String> {
    match value {
        serde_json::Value::String(text) => Ok(super::toml_string_literal(text)),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        serde_json::Value::Object(object) => {
            let fields = object
                .iter()
                .map(|(key, value)| {
                    inline_value(value)
                        .map(|value| format!("{} = {value}", super::toml_string_literal(key)))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("{{ {} }}", fields.join(", ")))
        }
        _ => Err("celestial settings must be strings, numbers, or scene objects".into()),
    }
}

fn header_path(line: &str) -> Option<Vec<String>> {
    if !line.trim_start().starts_with('[') {
        return None;
    }
    // Let TOML handle quoted connector names, whitespace and header comments.
    let mut table = toml::from_str::<toml::Table>(&format!("{line}\n__jwm_marker = 1\n")).ok()?;
    let mut path = Vec::new();
    loop {
        if table.len() != 1 {
            return None;
        }
        let (key, value) = table.into_iter().next()?;
        if key == "__jwm_marker" {
            return Some(path);
        }
        path.push(key);
        table = match value {
            toml::Value::Table(table) => table,
            toml::Value::Array(mut values) if values.len() == 1 => match values.pop()? {
                toml::Value::Table(table) => table,
                _ => return None,
            },
            _ => return None,
        };
    }
}

/// Preserve unrelated text and prove that the parsed edit changes only owned
/// keys before writing it. Unmodelled dotted/inline table shapes are refused
/// without altering the file; multiline strings cannot trick the verifier.
pub(super) fn edit_values(
    existing: &str,
    changes: &BTreeMap<String, serde_json::Value>,
) -> Result<String, String> {
    let before = toml::from_str::<toml::Table>(existing).map_err(|error| error.to_string())?;
    let assignments = changes
        .iter()
        .map(|(key, value)| {
            if !PERSISTED_KEYS.contains(&key.as_str()) {
                return Err(format!("the celestial picker does not own behavior.{key}"));
            }
            inline_value(value).map(|value| (key.clone(), format!("{key} = {value}\n")))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut text = String::with_capacity(existing.len() + 512);
    let mut section = Vec::<String>::new();
    let mut skip_table = false;
    let mut inserted = false;
    let mut pending_value = String::new();
    for line in existing.lines() {
        if !pending_value.is_empty() {
            pending_value.push_str(line);
            pending_value.push('\n');
            if toml::from_str::<toml::Table>(&pending_value).is_ok() {
                pending_value.clear();
            }
            continue;
        }
        if let Some(path) = header_path(line) {
            section = path;
            skip_table = section.len() >= 2
                && section[0] == "behavior"
                && assignments.contains_key(&section[1]);
            if skip_table {
                continue;
            }
            text.push_str(line);
            text.push('\n');
            if section == ["behavior"] {
                for assignment in assignments.values() {
                    text.push_str(assignment);
                }
                inserted = true;
            }
            continue;
        }
        if skip_table {
            continue;
        }
        if section == ["behavior"]
            && let Some((key, _)) = line.trim().split_once('=')
            && assignments.contains_key(key.trim().trim_matches(['\'', '"']))
        {
            if toml::from_str::<toml::Table>(line).is_err() {
                pending_value.push_str(line);
                pending_value.push('\n');
            }
            continue;
        }
        text.push_str(line);
        text.push('\n');
    }
    if !inserted {
        text.push_str("\n[behavior]\n");
        for assignment in assignments.values() {
            text.push_str(assignment);
        }
    }
    let after = toml::from_str::<toml::Table>(&text).map_err(|error| error.to_string())?;
    for (key, value) in changes {
        let actual = after.get("behavior").and_then(|section| section.get(key));
        let wanted = toml::Value::try_from(value).map_err(|error| error.to_string())?;
        if actual != Some(&wanted) {
            return Err(format!(
                "behavior.{key} would not contain the selected value"
            ));
        }
    }
    let strip_owned = |mut table: toml::Table| {
        if let Some(toml::Value::Table(behavior)) = table.get_mut("behavior") {
            for key in changes.keys() {
                behavior.remove(key);
            }
            if behavior.is_empty() {
                table.remove("behavior");
            }
        }
        table
    };
    if strip_owned(before) != strip_owned(after) {
        return Err("the edit would change another configuration value".into());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn celestial_schema_roundtrips_and_legacy_files_receive_safe_defaults() {
        use crate::config::{Config, TomlConfig};
        let config = Config::default();
        let mut old = toml::Value::try_from(&config.inner).unwrap();
        let behavior = old.get_mut("behavior").unwrap().as_table_mut().unwrap();
        for key in [
            "wallpaper_astronomy_scene",
            "wallpaper_astronomy_outputs",
            "wallpaper_astronomy_presentation",
            "wallpaper_solar_orbit_mode",
            "wallpaper_solar_time_scale",
            "wallpaper_solar_time_offset_days",
        ] {
            behavior.remove(key);
        }
        let legacy: TomlConfig = old.try_into().unwrap();
        assert_eq!(legacy.behavior.wallpaper_astronomy_scene, "earth");
        assert_eq!(legacy.behavior.wallpaper_astronomy_layout, "span");
        assert_eq!(
            legacy.behavior.wallpaper_astronomy_presentation,
            "scientific"
        );
        assert_eq!(legacy.behavior.wallpaper_solar_orbit_mode, "ephemeris");
        assert_eq!(legacy.behavior.wallpaper_solar_time_scale, 1.0);
        assert_eq!(legacy.behavior.wallpaper_solar_time_offset_days, 0.0);
        assert!(legacy.behavior.wallpaper_astronomy_outputs.is_empty());
        let mut configured = config;
        for layout in ASTRONOMY_LAYOUTS {
            configured
                .set_values(&[
                    (
                        "behavior.wallpaper_astronomy_layout".into(),
                        serde_json::json!(layout),
                    ),
                    (
                        "behavior.wallpaper_astronomy_scene".into(),
                        serde_json::json!("jupiter"),
                    ),
                    (
                        "behavior.wallpaper_astronomy_outputs".into(),
                        serde_json::json!({
                            "DP-1": { "scene": "earth", "group": "desk" },
                            "DP-2": { "scene": "earth", "group": "desk" },
                            "HDMI-1": { "scene": "solar-system" }
                        }),
                    ),
                ])
                .unwrap();
            let text = toml::to_string(&configured.inner).unwrap();
            let restored: TomlConfig = toml::from_str(&text).unwrap();
            assert_eq!(restored.behavior.wallpaper_astronomy_layout, layout);
            assert_eq!(restored.behavior.wallpaper_astronomy_scene, "jupiter");
            assert_eq!(
                restored.behavior.wallpaper_astronomy_outputs["DP-1"].group,
                "desk"
            );
            assert!(
                restored.behavior.wallpaper_astronomy_outputs["HDMI-1"]
                    .group
                    .is_empty()
            );
        }
    }

    #[test]
    fn celestial_invalid_group_and_clock_batches_are_atomic() {
        use crate::config::Config;
        let mut config = Config::default();
        let before = toml::to_string(&config.inner).unwrap();
        for (key, value) in [
            ("wallpaper_astronomy_scene", serde_json::json!("pluto")),
            ("wallpaper_solar_time_scale", serde_json::json!(-1.0)),
            ("wallpaper_solar_time_scale", serde_json::json!(1_000_001.0)),
            (
                "wallpaper_solar_time_offset_days",
                serde_json::json!(8501.0),
            ),
            (
                "wallpaper_astronomy_outputs",
                serde_json::json!({
                    "DP-1": {"scene": "earth", "group": "desk"},
                    "DP-2": {"scene": "mars", "group": "desk"}
                }),
            ),
            (
                "wallpaper_astronomy_outputs",
                serde_json::json!({"DP-1": {"theme": "earth"}}),
            ),
        ] {
            assert!(
                config
                    .set_values(&[
                        (
                            "behavior.wallpaper_astronomy_layout".into(),
                            serde_json::json!("independent")
                        ),
                        (format!("behavior.{key}"), value),
                    ])
                    .is_err()
            );
            assert_eq!(toml::to_string(&config.inner).unwrap(), before);
        }
    }

    #[test]
    fn celestial_persistence_preserves_comments_and_unrelated_values() {
        let source = "# keep this\n[behavior] # keep header\nwallpaper = '/my/image.png' # keep fallback\nwallpaper_astronomy_scene = 'earth'\nwallpaper_astronomy_layout = 'span'\n[behavior.wallpaper_astronomy_outputs.'DP-1']\nscene = 'moon'\n[appearance]\nui_theme = 'nord' # keep appearance\n";
        let changes = BTreeMap::from([
            (
                "wallpaper_astronomy_scene".into(),
                serde_json::json!("saturn"),
            ),
            (
                "wallpaper_astronomy_layout".into(),
                serde_json::json!("independent"),
            ),
            (
                "wallpaper_astronomy_outputs".into(),
                serde_json::json!({"DP-1": {"scene": "moon"}, "HDMI-1": {"scene": "galaxy"}}),
            ),
        ]);
        let edited = edit_values(source, &changes).unwrap();
        assert!(edited.starts_with("# keep this\n"));
        assert!(edited.contains("wallpaper = '/my/image.png' # keep fallback"));
        assert!(edited.contains("ui_theme = 'nord' # keep appearance"));
        assert_eq!(edit_values(&edited, &changes).unwrap(), edited);
        let parsed: toml::Table = toml::from_str(&edited).unwrap();
        assert_eq!(
            parsed["behavior"]["wallpaper_astronomy_outputs"]["DP-1"]["scene"].as_str(),
            Some("moon")
        );
        assert_eq!(
            parsed["behavior"]["wallpaper_astronomy_outputs"]["HDMI-1"]["scene"].as_str(),
            Some("galaxy")
        );
        let ambiguous = "[behavior]\nsuspend_command = '''\n[behavior]\nwallpaper_astronomy_scene = 'moon'\n'''\n";
        assert!(edit_values(ambiguous, &changes).is_err());
    }

    #[test]
    fn celestial_view_and_spin_controls_persist_and_reload_without_changing_other_settings() {
        use crate::config::Config;
        use crate::jwm::features::wallpaper::{WallpaperTarget, astronomy_changes};
        use serde_json::json;

        let directory = std::env::temp_dir().join(format!(
            "jwm-celestial-controls-save-{}-{}",
            std::process::id(),
            super::super::CONFIG_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let mut selected = Config::default();
        selected
            .set_values(&[
                ("behavior.wallpaper".into(), json!("/pictures/static.png")),
                ("behavior.wallpaper_mode".into(), json!("fit")),
                ("appearance.ui_theme".into(), json!("nord")),
                ("behavior.wallpaper_earth_view".into(), json!("fixed")),
                ("behavior.wallpaper_moon_view".into(), json!("rotating")),
                ("behavior.wallpaper_solar_spin_speed".into(), json!(42.0)),
                (
                    "behavior.wallpaper_astronomy_outputs".into(),
                    json!({"DP-1": {"scene": "moon"}, "HDMI-1": {"scene": "saturn"}}),
                ),
            ])
            .unwrap();
        selected.save_to_file(&path).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("# Keep my other settings\n{original}")).unwrap();
        let default_spin = Config::default().behavior().wallpaper_solar_spin_speed;
        for (choice, field, expected) in [
            ("view:earth", "wallpaper_earth_view", json!("rotating")),
            ("view:moon", "wallpaper_moon_view", json!("fixed")),
            ("spin:pause", "wallpaper_solar_spin_speed", json!(0.0)),
            ("spin:realtime", "wallpaper_solar_spin_speed", json!(1.0)),
            (
                "spin:default",
                "wallpaper_solar_spin_speed",
                json!(default_spin),
            ),
            ("view:earth", "wallpaper_earth_view", json!("fixed")),
            ("view:moon", "wallpaper_moon_view", json!("rotating")),
        ] {
            let before = std::fs::read_to_string(&path).unwrap();
            let mut expected_file: toml::Table = toml::from_str(&before).unwrap();
            expected_file
                .get_mut("behavior")
                .unwrap()
                .as_table_mut()
                .unwrap()
                .insert(field.into(), toml::Value::try_from(&expected).unwrap());
            let previous_outputs = selected.behavior().wallpaper_astronomy_outputs.clone();
            let changes = astronomy_changes(
                selected.behavior(),
                &WallpaperTarget::All,
                &["DP-1".into(), "HDMI-1".into()],
                &format!("jwm-wallpaper:{choice}"),
            )
            .unwrap();
            selected.set_values(&changes).unwrap();
            selected
                .persist_wallpaper_changes_to(&path, &changes, &previous_outputs)
                .unwrap();
            let saved = std::fs::read_to_string(&path).unwrap();
            assert!(saved.starts_with("# Keep my other settings\n"));
            assert_eq!(
                toml::from_str::<toml::Table>(&saved).unwrap(),
                expected_file
            );
            let reloaded = Config::load_from_file(&path).unwrap();
            assert_eq!(
                serde_json::to_value(reloaded.behavior()).unwrap()[field],
                expected,
                "{choice} did not survive reload"
            );
            assert_eq!(
                reloaded.behavior().wallpaper_astronomy_outputs,
                previous_outputs
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn celestial_persistence_merges_only_changed_connectors_from_fresh_disk() {
        use crate::config::Config;
        let directory = std::env::temp_dir().join(format!(
            "jwm-celestial-save-{}-{}",
            std::process::id(),
            super::super::CONFIG_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let mut previous = Config::default();
        previous
            .set_values(&[(
                "behavior.wallpaper_astronomy_outputs".into(),
                serde_json::json!({
                    "DP-1": {"scene":"earth"}, "HDMI-1": {"scene":"saturn"}
                }),
            )])
            .unwrap();
        let mut disk = previous.clone();
        disk.set_values(&[(
            "behavior.wallpaper_astronomy_outputs".into(),
            serde_json::json!({
                "DP-1": {"scene":"earth"}, "HDMI-1": {"scene":"neptune"}, "DP-4": {"scene":"moon"}
            }),
        )])
        .unwrap();
        disk.save_to_file(&path).unwrap();
        let mut selected = previous.clone();
        let changes = vec![(
            "behavior.wallpaper_astronomy_outputs".into(),
            serde_json::json!({
                "DP-1": {"scene":"mars"}, "HDMI-1": {"scene":"saturn"}
            }),
        )];
        selected.set_values(&changes).unwrap();
        selected
            .persist_wallpaper_changes_to(
                &path,
                &changes,
                &previous.behavior().wallpaper_astronomy_outputs,
            )
            .unwrap();
        let restored = Config::load_from_file(&path).unwrap();
        assert_eq!(
            restored.behavior().wallpaper_astronomy_outputs["DP-1"].scene,
            "mars"
        );
        assert_eq!(
            restored.behavior().wallpaper_astronomy_outputs["HDMI-1"].scene,
            "neptune"
        );
        assert_eq!(
            restored.behavior().wallpaper_astronomy_outputs["DP-4"].scene,
            "moon"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn changing_one_connector_preserves_other_and_disconnected_outputs() {
        let mut outputs = BTreeMap::from([
            ("DP-1".into(), AstronomyOutputConfig::independent("earth")),
            (
                "HDMI-1".into(),
                AstronomyOutputConfig::independent("saturn"),
            ),
            (
                "DP-3".into(),
                AstronomyOutputConfig::independent("solar-system"),
            ),
        ]);
        let previous = outputs.clone();
        set_output_scene(&mut outputs, "DP-1", "mars");
        assert_eq!(outputs["DP-1"].scene, "mars");
        assert_eq!(outputs["HDMI-1"], previous["HDMI-1"]);
        assert_eq!(outputs["DP-3"], previous["DP-3"]);
    }

    #[test]
    fn joining_and_leaving_groups_does_not_erase_another_scene() {
        let mut outputs = BTreeMap::from([
            ("A".into(), AstronomyOutputConfig::independent("earth")),
            ("B".into(), AstronomyOutputConfig::independent("moon")),
            ("C".into(), AstronomyOutputConfig::independent("saturn")),
            ("D".into(), AstronomyOutputConfig::independent("galaxy")),
        ]);
        join_output_scene(&mut outputs, "B", "A", "earth");
        join_output_scene(&mut outputs, "C", "D", "earth");
        assert_ne!(outputs["A"].group, outputs["D"].group);
        set_output_scene(&mut outputs, "B", "jupiter");
        assert_eq!(outputs["A"].scene, "jupiter");
        assert_eq!(outputs["B"].scene, "jupiter");
        assert_eq!(outputs["C"].scene, "galaxy");
        join_output_scene(&mut outputs, "B", "D", "earth");
        assert_eq!(outputs["A"].scene, "jupiter");
        assert_eq!(outputs["B"], outputs["D"]);
        assert!(outputs_error(&outputs).is_none());
    }

    #[test]
    fn invalid_groups_and_oversized_or_unknown_assignments_are_rejected() {
        let mut outputs = BTreeMap::from([
            (
                "DP-1".into(),
                AstronomyOutputConfig {
                    scene: "earth".into(),
                    group: "desk".into(),
                },
            ),
            (
                "DP-2".into(),
                AstronomyOutputConfig {
                    scene: "moon".into(),
                    group: "desk".into(),
                },
            ),
        ]);
        assert!(outputs_error(&outputs).is_some());
        outputs.get_mut("DP-2").unwrap().scene = "earth".into();
        assert!(outputs_error(&outputs).is_none());
        outputs.get_mut("DP-2").unwrap().scene = "invented-world".into();
        assert!(outputs_error(&outputs).is_some());
        outputs = (0..=MAX_ASTRONOMY_OUTPUTS)
            .map(|index| {
                (
                    format!("DP-{index}"),
                    AstronomyOutputConfig::independent("earth"),
                )
            })
            .collect();
        assert!(outputs_error(&outputs).is_some());
    }
}
