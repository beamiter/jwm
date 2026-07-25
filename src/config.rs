//! Optional TOML configuration shared by bar frontends.
//!
//! Every field is optional and overrides the crate default, so an empty or
//! missing file yields exactly the built-in appearance. Invalid values are
//! errors rather than silent fallbacks: a bar should refuse to start with a
//! config the user believes is active but is not.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ThemeMode;
use crate::presentation::PresentationConfig;

/// Default font when neither the config file nor `XBAR_FONT` specifies one.
pub const DEFAULT_FONT: &str = "monospace 11";

/// Resolved bar configuration ready for a frontend.
#[derive(Debug, Clone, PartialEq)]
pub struct BarConfig {
    /// Pango font description string.
    pub font: String,
    pub theme: ThemeMode,
    /// Optional background alpha multiplier for renderers that support it.
    pub background_opacity: Option<f64>,
    pub presentation: PresentationConfig,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            font: DEFAULT_FONT.to_owned(),
            theme: ThemeMode::Dark,
            background_opacity: None,
            presentation: PresentationConfig::default(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: Option<PathBuf>,
        source: toml::de::Error,
    },
    InvalidValue {
        field: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::Parse {
                path: Some(path),
                source,
            } => {
                write!(f, "failed to parse {}: {source}", path.display())
            }
            Self::Parse { path: None, source } => write!(f, "failed to parse config: {source}"),
            Self::InvalidValue { field, reason } => {
                write!(f, "invalid config value for `{field}`: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::InvalidValue { .. } => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    font: Option<String>,
    theme: Option<FileTheme>,
    background_opacity: Option<f64>,
    #[serde(default)]
    presentation: FilePresentation,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FileTheme {
    Dark,
    Light,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePresentation {
    bar_height: Option<f32>,
    horizontal_padding: Option<f32>,
    vertical_padding: Option<f32>,
    item_gap: Option<f32>,
    pill_horizontal_padding: Option<f32>,
    corner_radius: Option<f32>,
    font_size: Option<f32>,
    left_fraction: Option<f32>,
    tag_labels: Option<Vec<String>>,
}

impl BarConfig {
    /// Parse a TOML document, overriding defaults field by field.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Self::from_file_config(
            toml::from_str(text).map_err(|source| ConfigError::Parse { path: None, source })?,
        )
    }

    /// Read and parse `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let parsed = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: Some(path.to_owned()),
            source,
        })?;
        Self::from_file_config(parsed)
    }

    /// Load the conventional user configuration.
    ///
    /// Resolution order: `$XBAR_CONFIG` (must exist when set), else
    /// `$XDG_CONFIG_HOME/xbar/config.toml`, else `$HOME/.config/xbar/config.toml`.
    /// A missing conventional file yields the defaults. Afterwards a non-empty
    /// `XBAR_FONT` environment variable overrides the font, preserving the
    /// pre-config workflow of every native bar.
    pub fn load_default() -> Result<Self, ConfigError> {
        let mut config = match Self::default_path() {
            Some((path, required)) if path.is_file() || required => Self::load(&path)?,
            _ => Self::default(),
        };
        if let Ok(font) = std::env::var("XBAR_FONT")
            && !font.is_empty()
        {
            config.font = font;
        }
        Ok(config)
    }

    /// Model configuration seeded with this file's theme.
    #[must_use]
    pub fn model_config(&self) -> crate::ModelConfig {
        crate::ModelConfig {
            initial_theme: self.theme,
            ..crate::ModelConfig::default()
        }
    }

    fn default_path() -> Option<(PathBuf, bool)> {
        if let Ok(path) = std::env::var("XBAR_CONFIG")
            && !path.is_empty()
        {
            return Some((PathBuf::from(path), true));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some((base.join("xbar").join("config.toml"), false))
    }

    fn from_file_config(file: FileConfig) -> Result<Self, ConfigError> {
        let mut config = Self::default();
        if let Some(font) = file.font {
            if font.trim().is_empty() {
                return Err(ConfigError::InvalidValue {
                    field: "font",
                    reason: "font must not be empty",
                });
            }
            config.font = font;
        }
        if let Some(theme) = file.theme {
            config.theme = match theme {
                FileTheme::Dark => ThemeMode::Dark,
                FileTheme::Light => ThemeMode::Light,
            };
        }
        if let Some(opacity) = file.background_opacity {
            if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
                return Err(ConfigError::InvalidValue {
                    field: "background_opacity",
                    reason: "must be between 0 and 1",
                });
            }
            config.background_opacity = Some(opacity);
        }

        let presentation = &mut config.presentation;
        apply_positive(
            "presentation.bar_height",
            file.presentation.bar_height,
            &mut presentation.bar_height,
        )?;
        apply_non_negative(
            "presentation.horizontal_padding",
            file.presentation.horizontal_padding,
            &mut presentation.horizontal_padding,
        )?;
        apply_non_negative(
            "presentation.vertical_padding",
            file.presentation.vertical_padding,
            &mut presentation.vertical_padding,
        )?;
        apply_non_negative(
            "presentation.item_gap",
            file.presentation.item_gap,
            &mut presentation.item_gap,
        )?;
        apply_non_negative(
            "presentation.pill_horizontal_padding",
            file.presentation.pill_horizontal_padding,
            &mut presentation.pill_horizontal_padding,
        )?;
        apply_non_negative(
            "presentation.corner_radius",
            file.presentation.corner_radius,
            &mut presentation.corner_radius,
        )?;
        apply_positive(
            "presentation.font_size",
            file.presentation.font_size,
            &mut presentation.font_size,
        )?;
        if let Some(fraction) = file.presentation.left_fraction {
            if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
                return Err(ConfigError::InvalidValue {
                    field: "presentation.left_fraction",
                    reason: "must be between 0 and 1",
                });
            }
            presentation.left_fraction = fraction;
        }
        if let Some(labels) = file.presentation.tag_labels {
            if labels.is_empty() || labels.iter().any(|label| label.trim().is_empty()) {
                return Err(ConfigError::InvalidValue {
                    field: "presentation.tag_labels",
                    reason: "labels must be a non-empty list of non-empty strings",
                });
            }
            presentation.tag_labels = labels;
        }
        Ok(config)
    }
}

fn apply_positive(
    field: &'static str,
    value: Option<f32>,
    target: &mut f32,
) -> Result<(), ConfigError> {
    if let Some(value) = value {
        if !value.is_finite() || value <= 0.0 {
            return Err(ConfigError::InvalidValue {
                field,
                reason: "must be a finite value greater than zero",
            });
        }
        *target = value;
    }
    Ok(())
}

fn apply_non_negative(
    field: &'static str,
    value: Option<f32>,
    target: &mut f32,
) -> Result<(), ConfigError> {
    if let Some(value) = value {
        if !value.is_finite() || value < 0.0 {
            return Err(ConfigError::InvalidValue {
                field,
                reason: "must be a finite non-negative value",
            });
        }
        *target = value;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_is_exactly_the_default() {
        assert_eq!(BarConfig::from_toml("").unwrap(), BarConfig::default());
    }

    #[test]
    fn overrides_apply_field_by_field() {
        let config = BarConfig::from_toml(
            r#"
font = "JetBrainsMono Nerd Font 12"
theme = "light"
background_opacity = 0.85

[presentation]
bar_height = 42.0
font_size = 14.5
tag_labels = ["a", "b", "c"]
"#,
        )
        .unwrap();

        assert_eq!(config.font, "JetBrainsMono Nerd Font 12");
        assert_eq!(config.theme, ThemeMode::Light);
        assert_eq!(config.background_opacity, Some(0.85));
        assert_eq!(config.presentation.bar_height, 42.0);
        assert_eq!(config.presentation.font_size, 14.5);
        assert_eq!(config.presentation.tag_labels, vec!["a", "b", "c"]);
        // Untouched fields keep their defaults.
        assert_eq!(
            config.presentation.item_gap,
            PresentationConfig::default().item_gap
        );
    }

    #[test]
    fn invalid_values_and_unknown_fields_are_rejected() {
        assert!(matches!(
            BarConfig::from_toml("[presentation]\nbar_height = 0.0"),
            Err(ConfigError::InvalidValue {
                field: "presentation.bar_height",
                ..
            })
        ));
        assert!(matches!(
            BarConfig::from_toml("background_opacity = 1.5"),
            Err(ConfigError::InvalidValue {
                field: "background_opacity",
                ..
            })
        ));
        assert!(matches!(
            BarConfig::from_toml("[presentation]\ntag_labels = []"),
            Err(ConfigError::InvalidValue {
                field: "presentation.tag_labels",
                ..
            })
        ));
        assert!(matches!(
            BarConfig::from_toml("unknown_key = 1"),
            Err(ConfigError::Parse { .. })
        ));
        assert!(matches!(
            BarConfig::from_toml("theme = \"solarized\""),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn load_reports_missing_files_with_their_path() {
        let error = BarConfig::load(Path::new("/definitely/missing/xbar.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Read { .. }));
        assert!(error.to_string().contains("/definitely/missing/xbar.toml"));
    }
}
