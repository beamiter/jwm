//! Appearance preview ownership. A newer publication invalidates rollback and
//! saving; owned keys never restore an unrelated setting or frontend override.
use crate::config::Config;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) const OWNED_KEYS: &[&str] = &[
    "appearance.ui_theme",
    "appearance.system_ui_font",
    "appearance.ui_surface_opacity",
    "appearance.ui_density",
    "appearance.gap_px",
    "appearance.status_bar_height",
    "appearance.status_bar_padding",
    "animation.enabled",
    "behavior.blur_enabled",
    "behavior.blur_strength",
    "behavior.shadow_enabled",
    "behavior.border_glow_enabled",
];
pub(crate) const PRESETS: &[(&str, &str)] = &[
    (
        "@density:compact",
        "Density · Compact (28 px bar / 4 px gaps)",
    ),
    (
        "@density:comfortable",
        "Density · Comfortable (36 px bar / 8 px gaps)",
    ),
    (
        "@density:spacious",
        "Density · Spacious (44 px bar / 12 px gaps)",
    ),
    ("@font:11", "Font · Sans 11"),
    ("@font:14", "Font · Sans 14"),
    ("@font:18", "Font · Sans 18 · large text"),
    ("@surface:0.85", "Surface opacity · 85% · text stays opaque"),
    ("@surface:0.95", "Surface opacity · 95% · text stays opaque"),
    ("@surface:1", "Surface opacity · 100% · authored coverage"),
    ("@motion:reduced", "Motion · Reduced"),
    ("@motion:normal", "Motion · Normal"),
    (
        "@effects:low",
        "Effects · Low cost (Paper / no blur / shadow / glow)",
    ),
    (
        "@effects:balanced",
        "Effects · Balanced (blur 2 / shadow / glow)",
    ),
    ("@effects:rich", "Effects · Rich (blur 4 / shadow / glow)"),
    (
        "@save",
        "Save preview · global defaults · preserve frontend overrides",
    ),
    ("@keep", "Keep preview for this session"),
    ("@cancel", "Cancel preview and return"),
];

pub(crate) fn changes_for_choice(choice: &str) -> Result<Vec<(String, Value)>, String> {
    let changes: Vec<(&str, Value)> = match choice {
        theme if crate::config::KNOWN_UI_THEMES.contains(&theme) => {
            vec![("appearance.ui_theme", json!(theme))]
        }
        "@density:compact" => vec![
            ("appearance.ui_density", json!(0.8)),
            ("appearance.status_bar_height", json!(28)),
            ("appearance.status_bar_padding", json!(2)),
            ("appearance.gap_px", json!(4)),
        ],
        "@density:comfortable" => vec![
            ("appearance.ui_density", json!(1.0)),
            ("appearance.status_bar_height", json!(36)),
            ("appearance.status_bar_padding", json!(4)),
            ("appearance.gap_px", json!(8)),
        ],
        "@density:spacious" => vec![
            ("appearance.ui_density", json!(1.2)),
            ("appearance.status_bar_height", json!(44)),
            ("appearance.status_bar_padding", json!(6)),
            ("appearance.gap_px", json!(12)),
        ],
        "@font:11" => vec![("appearance.system_ui_font", json!("Sans 11"))],
        "@font:14" => vec![("appearance.system_ui_font", json!("Sans 14"))],
        "@font:18" => vec![("appearance.system_ui_font", json!("Sans 18"))],
        "@surface:0.85" => vec![("appearance.ui_surface_opacity", json!(0.85))],
        "@surface:0.95" => vec![("appearance.ui_surface_opacity", json!(0.95))],
        "@surface:1" => vec![("appearance.ui_surface_opacity", json!(1.0))],
        "@motion:reduced" => vec![("animation.enabled", json!(false))],
        "@motion:normal" => vec![("animation.enabled", json!(true))],
        "@effects:low" => vec![
            ("appearance.ui_theme", json!("paper")),
            ("behavior.blur_enabled", json!(false)),
            ("behavior.blur_strength", json!(0)),
            ("behavior.shadow_enabled", json!(false)),
            ("behavior.border_glow_enabled", json!(false)),
        ],
        "@effects:balanced" | "@effects:rich" => vec![
            ("behavior.blur_enabled", json!(true)),
            (
                "behavior.blur_strength",
                json!(if choice == "@effects:rich" { 4 } else { 2 }),
            ),
            ("behavior.shadow_enabled", json!(true)),
            ("behavior.border_glow_enabled", json!(true)),
        ],
        _ => return Err("No appearance preset is selected".into()),
    };
    Ok(changes
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect())
}

pub(crate) struct ThemePreview {
    originals: BTreeMap<String, Value>,
    applied: Arc<Config>,
    disk_baseline: Option<String>,
}
impl std::fmt::Debug for ThemePreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThemePreview")
            .field("owned_keys", &self.originals.keys())
            .field("applied_theme", &self.applied.ui_theme())
            .finish_non_exhaustive()
    }
}
impl ThemePreview {
    #[cfg(test)]
    pub(crate) fn new(original: String, applied: Arc<Config>) -> Self {
        Self {
            originals: BTreeMap::from([("appearance.ui_theme".into(), json!(original))]),
            applied,
            disk_baseline: None,
        }
    }
    pub(crate) fn stage(
        previous: Option<Self>,
        current: Arc<Config>,
        changes: &[(String, Value)],
        disk_baseline: Option<String>,
    ) -> Result<(Arc<Config>, Self), String> {
        let mut preview=match previous{
            Some(preview)if preview.owns(&current)=>preview,
            Some(_)=>return Err("Appearance changed elsewhere; close this preview and reopen settings before editing".into()),
            None=>Self{originals:BTreeMap::new(),applied:current.clone(),disk_baseline},
        };
        for (key, _) in changes {
            if !OWNED_KEYS.contains(&key.as_str()) {
                return Err(format!("Appearance preview does not own {key}"));
            }
            if !preview.originals.contains_key(key) {
                preview.originals.insert(
                    key.clone(),
                    current
                        .value_at(key)
                        .ok_or_else(|| format!("Unknown appearance key {key}"))?,
                );
            }
        }
        let mut candidate = (*current).clone();
        candidate.set_values(changes)?;
        let applied = Arc::new(candidate);
        preview.applied = applied.clone();
        Ok((applied, preview))
    }
    pub(crate) fn owns(&self, current: &Arc<Config>) -> bool {
        Arc::ptr_eq(&self.applied, current)
    }
    pub(crate) fn rollback_changes(&self, current: &Arc<Config>) -> Option<Vec<(String, Value)>> {
        self.owns(current).then(|| {
            self.originals
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
    }
    pub(crate) fn staged_changes(
        &self,
        current: &Arc<Config>,
    ) -> Result<Vec<(String, Value)>, String> {
        if !self.owns(current) {
            return Err("Appearance changed elsewhere; newer settings were retained".into());
        }
        self.originals
            .keys()
            .map(|key| {
                current
                    .value_at(key)
                    .map(|v| (key.clone(), v))
                    .ok_or_else(|| format!("Appearance key disappeared: {key}"))
            })
            .collect()
    }
    pub(crate) fn disk_baseline(&self) -> Option<&str> {
        self.disk_baseline.as_deref()
    }
    #[cfg(test)]
    pub(crate) fn rollback_theme(&self, current: &Arc<Config>) -> Option<&str> {
        self.owns(current)
            .then(|| {
                self.originals
                    .get("appearance.ui_theme")
                    .and_then(Value::as_str)
            })
            .flatten()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancel_restores_only_the_revision_owned_by_the_preview() {
        let applied = Arc::new(Config::default());
        let preview = ThemePreview::new("paper".into(), applied.clone());
        assert_eq!(preview.rollback_theme(&applied), Some("paper"));
        let newer = Arc::new((*applied).clone());
        assert_eq!(preview.rollback_changes(&newer), None);
    }
    #[test]
    fn successive_presets_restore_owned_fields_without_touching_frontend_defaults() {
        let baseline = Arc::new(Config::default());
        let original_gap = baseline.gap_px();
        let font = baseline.system_ui_font().to_owned();
        let (first, preview) = ThemePreview::stage(
            None,
            baseline.clone(),
            &changes_for_choice("@density:compact").unwrap(),
            None,
        )
        .unwrap();
        let (second, preview) = ThemePreview::stage(
            Some(preview),
            first,
            &changes_for_choice("@font:18").unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(second.system_ui_font(), "Sans 18");
        assert_eq!(second.gap_px(), 4);
        let mut reverted = (*second).clone();
        reverted
            .set_values(&preview.rollback_changes(&second).unwrap())
            .unwrap();
        assert_eq!(reverted.system_ui_font(), font);
        assert_eq!(reverted.gap_px(), original_gap);
        let mut other = (*second).clone();
        other.set_value("appearance.gap_px", &json!(31)).unwrap();
        assert!(preview.rollback_changes(&Arc::new(other)).is_none());
    }
    #[test]
    fn invalid_or_unowned_stage_cannot_publish_partial_values() {
        let baseline = Arc::new(Config::default());
        assert!(
            ThemePreview::stage(
                None,
                baseline.clone(),
                &[
                    ("appearance.gap_px".into(), json!(3)),
                    ("behavior.wallpaper".into(), json!("/tmp/nope"))
                ],
                None
            )
            .is_err()
        );
        assert_eq!(baseline.gap_px(), Config::default().gap_px());
    }
}

#[cfg(test)]
mod preset_validation_tests {
    use super::*;
    #[test]
    fn every_concrete_appearance_preset_is_an_atomic_valid_config_batch() {
        for (key, _) in PRESETS {
            if ["@save", "@keep", "@cancel"].contains(key) {
                continue;
            }
            let mut config = Config::default();
            config
                .set_values(&changes_for_choice(key).unwrap())
                .unwrap_or_else(|e| panic!("{key}: {e}"));
        }
        let mut low = Config::default();
        low.set_values(&changes_for_choice("@effects:low").unwrap())
            .unwrap();
        assert_eq!(low.ui_theme(), "paper");
        assert!(!low.behavior().blur_enabled);
    }
}
