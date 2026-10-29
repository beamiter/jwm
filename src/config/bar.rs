//! Comment-preserving persistence for the native bar controls.

use super::{Config, ConfigError, StatusBarMonitorConfig};
use std::path::Path;

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    ))
}

impl Config {
    /// Save the mode transition as one atomic, comment-preserving edit. A
    /// first explicit table also carries legacy timing values forward.
    pub(crate) fn persist_desktop_shell_mode(
        &self,
        desired: &xbar_core::desktop_shell::DesktopShellConfig,
    ) -> Result<std::time::SystemTime, ConfigError> {
        self.persist_desktop_shell_mode_to(Self::resolve_load_path(), desired)
    }

    fn persist_desktop_shell_mode_to(
        &self,
        path: impl AsRef<Path>,
        desired: &xbar_core::desktop_shell::DesktopShellConfig,
    ) -> Result<std::time::SystemTime, ConfigError> {
        let value = serde_json::to_value(desired).map_err(|error| invalid(error.to_string()))?;
        let selected = self
            .with_validated_value("desktop_shell", &value)
            .map_err(invalid)?;
        let path = path.as_ref();
        let existing = match super::read_config_text(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(selected.save_to_file_with_outcome(path)?.revision?);
            }
            Err(error) => return Err(error.into()),
        };
        let table: toml::Table = toml::from_str(&existing)?;
        let is_migration = !table.contains_key("desktop_shell");
        let mut edited = existing;
        for (field, value) in value
            .as_object()
            .expect("desktop shell serializes as a table")
        {
            if is_migration || matches!(field.as_str(), "enabled" | "left" | "right" | "bottom") {
                edited = edit_setting(&edited, "desktop_shell", field, value).map_err(invalid)?;
            }
        }
        Ok(super::atomic_write_with_outcome(path, edited.as_bytes())?.revision?)
    }

    pub(crate) fn persist_bar_setting(
        &self,
        key: &str,
        value: &serde_json::Value,
        changed_monitor: Option<i32>,
    ) -> Result<std::time::SystemTime, ConfigError> {
        self.persist_bar_setting_to(Self::resolve_load_path(), key, value, changed_monitor)
    }

    fn persist_bar_setting_to(
        &self,
        path: impl AsRef<Path>,
        key: &str,
        value: &serde_json::Value,
        changed_monitor: Option<i32>,
    ) -> Result<std::time::SystemTime, ConfigError> {
        let (section, field) = match key {
            "status_bar.name" => ("status_bar", "name"),
            "status_bar.layout" => ("status_bar", "layout"),
            "desktop_shell.enabled" => ("desktop_shell", "enabled"),
            "desktop_shell.left" => ("desktop_shell", "left"),
            "desktop_shell.right" => ("desktop_shell", "right"),
            "desktop_shell.bottom" => ("desktop_shell", "bottom"),
            "desktop_shell.reveal_delay_ms" => ("desktop_shell", "reveal_delay_ms"),
            "desktop_shell.hide_delay_ms" => ("desktop_shell", "hide_delay_ms"),
            "desktop_shell.trigger_pixels" => ("desktop_shell", "trigger_pixels"),
            "status_bar.position" => ("status_bar", "position"),
            "status_bar.monitors" => ("status_bar", "monitors"),
            "appearance.status_bar_height" => ("appearance", "status_bar_height"),
            "appearance.status_bar_padding" => ("appearance", "status_bar_padding"),
            _ => return Err(invalid("the bar controls cannot save this setting")),
        };
        let mut selected = self.with_validated_value(key, value).map_err(invalid)?;
        let path = path.as_ref();
        let existing = match super::read_config_text(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(selected.save_to_file_with_outcome(path)?.revision?);
            }
            Err(error) => return Err(error.into()),
        };
        let mut value = value.clone();
        if field == "monitors"
            && let Some(monitor) = changed_monitor
        {
            // The native UI edits one output. Merge it into the latest disk
            // revision so a concurrent edit to another output survives.
            let disk: toml::Table = toml::from_str(&existing)?;
            let mut placements: Vec<StatusBarMonitorConfig> = disk
                .get("status_bar")
                .and_then(|table| table.get("monitors"))
                .cloned()
                .map(toml::Value::try_into)
                .transpose()?
                .unwrap_or_default();
            let wanted = selected
                .inner
                .status_bar
                .monitors
                .iter()
                .find(|entry| entry.monitor == monitor)
                .cloned();
            placements.retain(|entry| entry.monitor != monitor);
            placements.extend(wanted);
            placements.sort_by_key(|entry| entry.monitor);
            value = serde_json::json!(placements);
            selected
                .set_values(&[(key.into(), value.clone())])
                .map_err(invalid)?;
        }
        let edited = edit_setting(&existing, section, field, &value).map_err(invalid)?;
        Ok(super::atomic_write_with_outcome(path, edited.as_bytes())?.revision?)
    }
}

/// Edit a plain table or array-of-tables representation, then prove the edit
/// changes exactly one key. Unmodelled TOML shapes fail before publication.
fn edit_setting(
    existing: &str,
    section: &str,
    field: &str,
    value: &serde_json::Value,
) -> Result<String, String> {
    let before: toml::Table = toml::from_str(existing).map_err(|error| error.to_string())?;
    let wanted = toml::Value::try_from(value).map_err(|error| error.to_string())?;
    let assignment = format!("{field} = {wanted}\n");
    let mut result = String::with_capacity(existing.len() + assignment.len() + 32);
    let mut inside = false;
    let mut skipping_array = false;
    let mut inserted = false;
    let mut pending_value = String::new();
    for line in existing.lines() {
        let trimmed = line.trim();
        if !pending_value.is_empty() {
            pending_value.push_str(line);
            pending_value.push('\n');
            if toml::from_str::<toml::Table>(&pending_value).is_ok() {
                pending_value.clear();
            }
            continue;
        }
        if trimmed.starts_with('[') {
            skipping_array = super::is_toml_table_header(trimmed, true, &[section, field]);
            inside = super::is_toml_table_header(trimmed, false, &[section]);
            if skipping_array {
                continue;
            }
            result.push_str(line);
            result.push('\n');
            if inside {
                result.push_str(&assignment);
                inserted = true;
            }
            continue;
        }
        if skipping_array {
            continue;
        }
        if inside
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim().trim_matches(['\'', '"']) == field)
        {
            if toml::from_str::<toml::Table>(line).is_err() {
                pending_value.push_str(line);
                pending_value.push('\n');
            }
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    if !inserted {
        result.push_str(&format!("\n[{section}]\n{assignment}"));
    }
    let after: toml::Table = toml::from_str(&result).map_err(|error| error.to_string())?;
    if after.get(section).and_then(|table| table.get(field)) != Some(&wanted) {
        return Err(format!(
            "{section}.{field} would not contain the selected value"
        ));
    }
    if Config::without_owned_config_key(before, section, field)
        != Config::without_owned_config_key(after, section, field)
    {
        return Err("bar settings cannot be saved without changing another setting".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_mode_persistence_is_atomic_and_keeps_unrelated_configuration() {
        use xbar_core::desktop_shell::DesktopShellConfig;
        use xbar_core::edge_shell::RevealMode;
        let directory = std::env::temp_dir().join(format!(
            "jwm-shell-mode-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("config_x11.toml");
        let config = Config::default()
            .with_validated_value(
                "desktop_shell",
                &serde_json::json!({"reveal_delay_ms":123,"hide_delay_ms":456}),
            )
            .unwrap();
        config.save_to_file(&path).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!("# user's desktop\n{original}\n# trailing comment\n"),
        )
        .unwrap();
        let desired = DesktopShellConfig {
            enabled: true,
            left: RevealMode::Always,
            right: RevealMode::Disabled,
            bottom: RevealMode::Always,
            reveal_delay_ms: 123,
            hide_delay_ms: 456,
            ..Default::default()
        };
        config
            .persist_desktop_shell_mode_to(&path, &desired)
            .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# user's desktop"));
        assert!(saved.contains("# trailing comment"));
        let loaded = Config::load_from_file(&path).unwrap();
        assert_eq!(loaded.desktop_shell_config(), desired);
        assert_eq!(loaded.status_bar_position(), config.status_bar_position());
        let hovered = desired.toggled_mode();
        loaded
            .persist_desktop_shell_mode_to(&path, &hovered)
            .unwrap();
        assert_eq!(
            Config::load_from_file(&path)
                .unwrap()
                .desktop_shell_config(),
            hovered
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn shell_mode_persistence_promotes_legacy_ownership_and_disabled_edges() {
        use xbar_core::desktop_shell::{DesktopShellConfig, DesktopShellSource};
        use xbar_core::edge_shell::RevealMode;
        let path = std::env::temp_dir().join(format!(
            "jwm-shell-mode-legacy-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = Config::default()
            .with_validated_value("status_bar.layout", &serde_json::json!("four-edge"))
            .unwrap();
        config.save_to_file(&path).unwrap();
        assert_eq!(
            config.desktop_shell_source(),
            DesktopShellSource::LegacyFourEdge
        );
        let desired = DesktopShellConfig {
            right: RevealMode::Disabled,
            ..config.desktop_shell_config()
        }
        .toggled_mode();
        config
            .persist_desktop_shell_mode_to(&path, &desired)
            .unwrap();
        let loaded = Config::load_from_file(&path).unwrap();
        assert_eq!(
            loaded.desktop_shell_source(),
            DesktopShellSource::DesktopShell
        );
        assert_eq!(loaded.desktop_shell_config(), desired);
        assert!(desired.enabled);
        assert_eq!(desired.right, RevealMode::Disabled);
        assert_eq!(desired.left, RevealMode::Hover);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bar_persistence_preserves_unrelated_comments_and_supports_monitor_tables() {
        let source = "# my desktop\n[appearance] # keep\nstatus_bar_height = 42\ngap_px = 7 # keep\n[status_bar]\nname = 'tao_glow_bar' # keep\nposition = 'top'\n[[status_bar.monitors]]\nmonitor = 1\nposition = 'left'\n[layout]\nn_master = 2\n";
        let placements =
            serde_json::json!([{"monitor":1,"position":"right"},{"monitor":2,"position":"bottom"}]);
        let edited = edit_setting(source, "status_bar", "monitors", &placements).unwrap();
        for kept in [
            "# my desktop",
            "[appearance] # keep",
            "gap_px = 7 # keep",
            "name = 'tao_glow_bar' # keep",
            "n_master = 2",
        ] {
            assert!(edited.contains(kept));
        }
        assert_eq!(
            edit_setting(&edited, "status_bar", "monitors", &placements).unwrap(),
            edited
        );
        let resized = edit_setting(
            &edited,
            "appearance",
            "status_bar_height",
            &serde_json::json!(48),
        )
        .unwrap();
        let table: toml::Table = toml::from_str(&resized).unwrap();
        assert_eq!(
            table["appearance"]["status_bar_height"].as_integer(),
            Some(48)
        );
        assert_eq!(table["status_bar"]["monitors"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn ambiguous_multiline_or_inline_config_is_never_corrupted() {
        for source in [
            "status_bar = { position = 'top', name = 'bar' }\n",
            "[status_bar]\nname = '''\n[status_bar]\nposition = 'left'\n'''\n",
        ] {
            assert!(
                edit_setting(
                    source,
                    "status_bar",
                    "position",
                    &serde_json::json!("right")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn saving_one_bar_monitor_preserves_a_newer_placement_on_another() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "jwm-bar-persistence-{}-{unique}.toml",
            std::process::id()
        ));
        let source = "# external edit\n[status_bar]\nposition = 'bottom'\n[[status_bar.monitors]]\nmonitor = 2\nposition = 'right'\n";
        std::fs::write(&path, source).unwrap();
        // This process has not reloaded monitor 2's newest disk placement.
        // Saving monitor 7 must merge only that output into the fresh file.
        Config::default()
            .persist_bar_setting_to(
                &path,
                "status_bar.monitors",
                &serde_json::json!([
                    {"monitor":2,"position":"top"},
                    {"monitor":7,"position":"left"}
                ]),
                Some(7),
            )
            .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        let table: toml::Table = toml::from_str(&saved).unwrap();
        let placements: Vec<StatusBarMonitorConfig> =
            table["status_bar"]["monitors"].clone().try_into().unwrap();
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].monitor, 2);
        assert_eq!(placements[0].position, xbar_core::placement::BarEdge::Right);
        assert_eq!(placements[1].monitor, 7);
        assert_eq!(placements[1].position, xbar_core::placement::BarEdge::Left);
        assert_eq!(table["status_bar"]["position"].as_str(), Some("bottom"));
        assert!(saved.contains("# external edit"));
        std::fs::remove_file(path).unwrap();
    }
}
