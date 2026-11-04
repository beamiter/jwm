//! Owned appearance persistence: edit only preview keys in fresh TOML, keeping
//! comments, frontend overrides and unrelated concurrent disk edits intact.
use super::{
    Config, ConfigError, atomic_write_with_sequence_and_guard, read_config_text,
    resolve_write_destination, verify_config_snapshot,
};
use serde_json::Value;
use std::path::Path;

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    ))
}
fn lookup<'a>(document: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let (section, field) = key.split_once('.')?;
    let table = document.get(section)?;
    table.get(field).or_else(|| {
        (key == "appearance.system_ui_font")
            .then(|| table.get("dmenu_font"))
            .flatten()
    })
}
impl Config {
    pub(crate) fn appearance_persistence_label(&self) -> &'static str {
        // Ordinary unit-test UI helpers must not inspect the user's files.
        #[cfg(test)]
        {
            "Session · saved state unavailable"
        }
        #[cfg(not(test))]
        {
            self.appearance_persistence_label_at(Self::resolve_load_path())
        }
    }
    pub(crate) fn appearance_persistence_label_at(&self, path: impl AsRef<Path>) -> &'static str {
        let Ok(text) = read_config_text(path.as_ref()) else {
            return "Session · saved state unavailable";
        };
        let Ok(inner) = toml::from_str::<super::TomlConfig>(&text) else {
            return "Session · saved state unavailable";
        };
        let disk = Self {
            inner,
            ..self.clone()
        };
        if disk.diagnostics().has_errors() {
            return "Session · saved state unavailable";
        }
        if crate::jwm::features::theme_preview::OWNED_KEYS
            .iter()
            .all(|key| self.value_at(key) == disk.value_at(key))
        {
            "Saved · global defaults"
        } else {
            "Session overrides · not saved"
        }
    }
    pub(crate) fn appearance_disk_baseline() -> Result<Option<String>, ConfigError> {
        match read_config_text(&Self::resolve_load_path()) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub(crate) fn persist_appearance_changes(
        &self,
        changes: &[(String, Value)],
        baseline: Option<&str>,
    ) -> Result<std::time::SystemTime, ConfigError> {
        self.persist_appearance_changes_to(Self::resolve_load_path(), changes, baseline)
    }
    pub(crate) fn persist_appearance_changes_to(
        &self,
        path: impl AsRef<Path>,
        changes: &[(String, Value)],
        baseline: Option<&str>,
    ) -> Result<std::time::SystemTime, ConfigError> {
        if changes.is_empty() || changes.len() > 32 {
            return Err(invalid(
                "No bounded appearance preview is available to save",
            ));
        }
        let mut validated = self.clone();
        validated.set_values(changes).map_err(invalid)?;
        for (key, _) in changes {
            if !crate::jwm::features::theme_preview::OWNED_KEYS.contains(&key.as_str()) {
                return Err(invalid(format!("Appearance settings do not own {key}")));
            }
        }
        let path = path.as_ref();
        let existing = match read_config_text(path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let old = baseline
            .map(toml::from_str::<toml::Table>)
            .transpose()?
            .unwrap_or_default();
        let fresh = existing
            .as_deref()
            .map(toml::from_str::<toml::Table>)
            .transpose()?
            .unwrap_or_default();
        for (key, _) in changes {
            if lookup(&old, key) != lookup(&fresh, key) {
                return Err(invalid(format!(
                    "{key} was changed in the configuration file while previewing; the newer file was retained"
                )));
            }
        }
        let destination = resolve_write_destination(path)?;
        let Some(existing) = existing else {
            let text = Self::add_option_comments(&toml::to_string_pretty(&validated.inner)?);
            return Ok(atomic_write_with_sequence_and_guard(
                path,
                text.as_bytes(),
                || super::CONFIG_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                || {
                    if resolve_write_destination(path)? != destination {
                        return Err(std::io::Error::other(
                            "Configuration symlink changed during save",
                        ));
                    }
                    match std::fs::symlink_metadata(&destination) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(e) => Err(e),
                        Ok(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            "A configuration was created while preparing this save",
                        )),
                    }
                },
                true,
            )?
            .revision?);
        };
        let metadata = std::fs::symlink_metadata(&destination)?;
        verify_config_snapshot(&destination, &metadata, existing.as_bytes())?;
        let mut document = existing
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| invalid(e.to_string()))?;
        for (key, value) in changes {
            let (section, field) = key
                .split_once('.')
                .ok_or_else(|| invalid("Invalid appearance setting path"))?;
            let scalar = toml::Value::try_from(value).map_err(|e| invalid(e.to_string()))?;
            let mut item = format!("value = {scalar}\n")
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| invalid(e.to_string()))?
                .remove("value")
                .ok_or_else(|| invalid("Appearance value could not be represented"))?;
            if document.get(section).is_none() {
                document.insert(section, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            if document[section].as_table_like().is_none() {
                return Err(invalid(format!(
                    "{section} is not a table; the configuration file was left unchanged"
                )));
            }
            let legacy_font =
                key == "appearance.system_ui_font" && document[section].get("dmenu_font").is_some();
            if legacy_font && document[section].get(field).is_some() {
                return Err(invalid(
                    "Both appearance.system_ui_font and its legacy dmenu_font alias exist; resolve the duplicate before saving",
                ));
            }
            let decor_source = if legacy_font { "dmenu_font" } else { field };
            let legacy_key_decor = legacy_font
                .then(|| {
                    document[section]
                        .as_table_like()
                        .and_then(|table| table.key("dmenu_font"))
                        .map(|key| key.leaf_decor().clone())
                })
                .flatten();
            if let Some(decor) = document[section]
                .get(decor_source)
                .and_then(toml_edit::Item::as_value)
                .map(|v| v.decor().clone())
                && let Some(value) = item.as_value_mut()
            {
                *value.decor_mut() = decor;
            }
            if legacy_font {
                document[section]
                    .as_table_like_mut()
                    .ok_or_else(|| invalid("Appearance table is not editable"))?
                    .remove("dmenu_font");
            }
            document[section][field] = item;
            if let Some(decor) = legacy_key_decor
                && let Some(mut key) = document[section]
                    .as_table_like_mut()
                    .and_then(|table| table.key_mut(field))
            {
                *key.leaf_decor_mut() = decor;
            }
        }
        let text = document.to_string();
        let after: toml::Table = toml::from_str(&text)?;
        let mut before_other = fresh;
        let mut after_other = after;
        for (key, value) in changes {
            let (section, field) = key.split_once('.').unwrap();
            let expected = toml::Value::try_from(value).map_err(|e| invalid(e.to_string()))?;
            if lookup(&after_other, key) != Some(&expected) {
                return Err(invalid(format!("Appearance write did not set {key}")));
            }
            for table in [&mut before_other, &mut after_other] {
                if let Some(section) = table.get_mut(section).and_then(toml::Value::as_table_mut) {
                    section.remove(field);
                    if key == "appearance.system_ui_font" {
                        section.remove("dmenu_font");
                    }
                }
            }
        }
        for table in [&mut before_other, &mut after_other] {
            table.retain(|_, value| !value.as_table().is_some_and(|t| t.is_empty()));
        }
        if before_other != after_other {
            return Err(invalid(
                "Appearance save would change an unowned setting; file left unchanged",
            ));
        }
        // The existing writer rechecks both bytes/inode and the symlink
        // destination after staging and fsync, immediately before publication.
        Ok(atomic_write_with_sequence_and_guard(
            path,
            text.as_bytes(),
            || super::CONFIG_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            || {
                if resolve_write_destination(path)? != destination {
                    return Err(std::io::Error::other(
                        "Configuration symlink changed during save",
                    ));
                }
                verify_config_snapshot(&destination, &metadata, existing.as_bytes())
            },
            false,
        )?
        .revision?)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn appearance_save_preserves_comments_overrides_and_unrelated_concurrent_edits() {
        let dir = std::env::temp_dir().join(format!(
            "jwm-appearance-save-{}-{}",
            std::process::id(),
            crate::config::CONFIG_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.toml");
        let baseline = "# personal settings\n[appearance] # appearance controls\nui_theme = 'nord' # keep theme note\nsystem_ui_font = 'Sans 11' # keep font note\n[status_bar.frontends.xilem]\nfont = 'Custom 17' # explicit frontend override\n[layout]\nn_master = 1\n";
        std::fs::write(&path, baseline.replace("n_master = 1", "n_master = 3")).unwrap();
        let changes = vec![
            ("appearance.ui_theme".into(), json!("paper")),
            ("appearance.system_ui_font".into(), json!("Sans 18")),
            ("animation.enabled".into(), json!(false)),
        ];
        Config::default()
            .persist_appearance_changes_to(&path, &changes, Some(baseline))
            .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("# keep theme note")
                && saved.contains("# keep font note")
                && saved.contains("font = 'Custom 17' # explicit frontend override")
                && saved.contains("n_master = 3")
        );
        let parsed: toml::Table = toml::from_str(&saved).unwrap();
        assert_eq!(
            parsed["appearance"]["system_ui_font"].as_str(),
            Some("Sans 18")
        );
        assert_eq!(parsed["animation"]["enabled"].as_bool(), Some(false));
        let changed = baseline.replace("ui_theme = 'nord'", "ui_theme = 'glass'");
        std::fs::write(&path, &changed).unwrap();
        assert!(
            Config::default()
                .persist_appearance_changes_to(&path, &changes, Some(baseline))
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod legacy_font_tests {
    use super::*;
    #[test]
    fn saving_font_migrates_legacy_alias_without_leaving_duplicate_fields() {
        let dir = std::env::temp_dir().join(format!(
            "jwm-appearance-alias-{}-{}",
            std::process::id(),
            crate::config::CONFIG_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.toml");
        let text = "[appearance]\n# leading font comment\ndmenu_font = 'Custom 12' # preserve legacy note\n";
        std::fs::write(&path, text).unwrap();
        Config::default()
            .persist_appearance_changes_to(
                &path,
                &[(
                    "appearance.system_ui_font".into(),
                    serde_json::json!("Sans 18"),
                )],
                Some(text),
            )
            .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("dmenu_font"));
        assert!(
            saved.contains("system_ui_font")
                && saved.contains("# preserve legacy note")
                && saved.contains("# leading font comment")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod malformed_section_tests {
    use super::*;
    #[test]
    fn malformed_table_shape_is_refused_without_panicking_or_replacing_file() {
        let dir = std::env::temp_dir().join(format!(
            "jwm-appearance-shape-{}-{}",
            std::process::id(),
            crate::config::CONFIG_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.toml");
        let text = "appearance = 'invalid-table'\n";
        std::fs::write(&path, text).unwrap();
        assert!(
            Config::default()
                .persist_appearance_changes_to(
                    &path,
                    &[("appearance.ui_theme".into(), serde_json::json!("paper"))],
                    Some(text)
                )
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod persistence_label_tests {
    use super::*;
    #[test]
    fn menu_saved_label_requires_a_valid_matching_disk_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "jwm-appearance-status-{}-{}",
            std::process::id(),
            crate::config::CONFIG_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.toml");
        let config = Config::default();
        assert!(
            config
                .appearance_persistence_label_at(&path)
                .contains("unavailable")
        );
        config.save_to_file(&path).unwrap();
        assert!(
            config
                .appearance_persistence_label_at(&path)
                .starts_with("Saved")
        );
        let mut live = config.clone();
        live.set_value("appearance.system_ui_font", &serde_json::json!("Sans 18"))
            .unwrap();
        assert!(
            live.appearance_persistence_label_at(&path)
                .starts_with("Session overrides")
        );
        std::fs::write(&path, "[invalid").unwrap();
        assert!(
            config
                .appearance_persistence_label_at(&path)
                .contains("unavailable")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
