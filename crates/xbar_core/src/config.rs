//! Optional TOML configuration shared by bar frontends.
//!
//! Every field is optional and overrides the crate default, so an empty or
//! missing file yields exactly the built-in appearance. Invalid values are
//! errors rather than silent fallbacks: a bar should refuse to start with a
//! config the user believes is active but is not.

use std::fmt;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ThemeMode;
use crate::model::MAX_MODEL_TAGS;
use crate::presentation::PresentationConfig;

/// Default font when neither the config file nor `XBAR_FONT` specifies one.
pub const DEFAULT_FONT: &str = "monospace 11";

/// Configuration is hand-written and should never need an allocation large
/// enough to disrupt every bar frontend during startup.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_FONT_BYTES: usize = 1024;
const MAX_ICON_FONT_BYTES: usize = 255;
const MAX_TAG_LABEL_BYTES: usize = 256;
const MAX_WALLPAPER_PATH_BYTES: usize = 4 * 1024;
const MAX_WALLPAPER_MODE_BYTES: usize = 32;

/// Resolved bar configuration ready for a frontend.
#[derive(Debug, Clone, PartialEq)]
pub struct BarConfig {
    /// Pango font description string.
    pub font: String,
    pub theme: ThemeMode,
    /// Optional background alpha multiplier for renderers that support it.
    pub background_opacity: Option<f64>,
    pub presentation: PresentationConfig,
    pub glass: GlassConfig,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            font: DEFAULT_FONT.to_owned(),
            theme: ThemeMode::Dark,
            background_opacity: None,
            presentation: PresentationConfig::default(),
            glass: GlassConfig::default(),
        }
    }
}

/// Frosted-glass backdrop settings.
///
/// Every field is an override: what is unset keeps
/// [`GlassParams::default`](crate::glass::GlassParams::default), so this type
/// never becomes a second place where the recipe is defined.
///
/// `wallpaper` is the one field a user normally has to set, and it should name
/// the same file the compositor draws — under JWM that is `behavior.wallpaper`
/// from its own config.  Leaving it unset asks the frontend to fall back to
/// whatever its platform offers, which on X11 means the root pixmap.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlassConfig {
    pub wallpaper: Option<PathBuf>,
    /// `fill`, `fit`, `stretch`, or `center`; must match the compositor's own
    /// mode or the backdrop will not line up with its surroundings.
    pub wallpaper_mode: Option<String>,
    pub downscale: Option<u32>,
    pub blur_radius: Option<u32>,
    pub blur_passes: Option<u32>,
    pub saturation: Option<f32>,
    pub pad: Option<u32>,
}

impl GlassConfig {
    /// Apply these overrides to the default recipe.
    #[cfg(feature = "glass")]
    #[must_use]
    pub fn params(&self) -> crate::glass::GlassParams {
        let mut params = crate::glass::GlassParams::default();
        if let Some(downscale) = self.downscale {
            params.downscale = downscale;
        }
        if let Some(radius) = self.blur_radius {
            params.blur_radius = radius;
        }
        if let Some(passes) = self.blur_passes {
            params.blur_passes = passes;
        }
        if let Some(saturation) = self.saturation {
            params.saturation = saturation;
        }
        if let Some(pad) = self.pad {
            params.pad = pad;
        }
        params.sanitized()
    }

    /// The configured layout mode, defaulting the way a compositor defaults it.
    #[cfg(feature = "glass-wallpaper")]
    #[must_use]
    pub fn mode(&self) -> crate::glass::wallpaper::WallpaperMode {
        self.wallpaper_mode.as_deref().map_or_else(
            Default::default,
            crate::glass::wallpaper::WallpaperMode::parse,
        )
    }

    /// The file-backed wallpaper source this configuration asks for, if any.
    ///
    /// `background` fills whatever the wallpaper does not cover under `fit` and
    /// `center`, and should be the frontend's own opaque fallback color.
    #[cfg(feature = "glass-wallpaper")]
    #[must_use]
    pub fn file_source(
        &self,
        screen_width: u32,
        screen_height: u32,
        background: [u8; 3],
    ) -> Option<crate::glass::wallpaper::WallpaperFile> {
        let path = self.wallpaper.as_ref()?;
        Some(
            crate::glass::wallpaper::WallpaperFile::new(
                path,
                self.mode(),
                screen_width,
                screen_height,
            )
            .with_background(background),
        )
    }

    /// Source and cache in one call, for a frontend that uploads the strip
    /// into its own texture.
    ///
    /// `None` means no wallpaper was configured, and a frontend without a
    /// platform fallback should then simply render without glass.
    #[cfg(feature = "glass-wallpaper")]
    #[must_use]
    pub fn file_strip(
        &self,
        screen_width: u32,
        screen_height: u32,
        background: [u8; 3],
    ) -> Option<crate::glass::GlassStrip<crate::glass::wallpaper::WallpaperFile>> {
        let source = self.file_source(screen_width, screen_height, background)?;
        Some(crate::glass::GlassStrip::new(source, self.params()).with_fallback(background))
    }

    /// The whole Cairo-side backdrop in one call: source, cache, and surface.
    ///
    /// `None` means no wallpaper was configured, and a frontend without a
    /// platform fallback should then simply render without glass.
    #[cfg(all(feature = "glass-wallpaper", feature = "render-cairo"))]
    #[must_use]
    pub fn file_backdrop(
        &self,
        screen_width: u32,
        screen_height: u32,
        background: [u8; 3],
    ) -> Option<crate::glass::GlassBackdrop<crate::glass::wallpaper::WallpaperFile>> {
        let source = self.file_source(screen_width, screen_height, background)?;
        Some(crate::glass::GlassBackdrop::new(source, self.params()).with_fallback(background))
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
    #[serde(default)]
    glass: FileGlass,
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
    dock_item_size: Option<f32>,
    dock_item_aspect_ratio: Option<f32>,
    dock_item_gap: Option<f32>,
    dock_shelf_padding: Option<f32>,
    dock_corner_radius: Option<f32>,
    dock_hover_scale: Option<f32>,
    dock_influence_radius: Option<f32>,
    dock_separator_width: Option<f32>,
    left_fraction: Option<f32>,
    tag_labels: Option<Vec<String>>,
    icon_font: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileGlass {
    wallpaper: Option<String>,
    wallpaper_mode: Option<String>,
    downscale: Option<u32>,
    blur_radius: Option<u32>,
    blur_passes: Option<u32>,
    saturation: Option<f32>,
    pad: Option<u32>,
}

impl BarConfig {
    /// Parse a TOML document, overriding defaults field by field.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Self::from_file_config(
            toml::from_str(text).map_err(|source| ConfigError::Parse { path: None, source })?,
        )
    }

    /// Read and parse a UTF-8 regular file no larger than one mebibyte.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = read_config_text(path).map_err(|source| ConfigError::Read {
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
    /// pre-config workflow of every native bar, and `XBAR_ICON_FONT` does the
    /// same for the family that backs private-use icon glyphs.
    pub fn load_default() -> Result<Self, ConfigError> {
        let mut config = match Self::default_path() {
            Some((path, required)) if path.is_file() || required => Self::load(&path)?,
            _ => Self::default(),
        };
        if let Ok(font) = std::env::var("XBAR_FONT")
            && !font.is_empty()
        {
            validate_text_bytes("XBAR_FONT", &font, MAX_FONT_BYTES)?;
            config.font = font;
        }
        if let Ok(icon_font) = std::env::var("XBAR_ICON_FONT")
            && !icon_font.trim().is_empty()
        {
            validate_text_bytes("XBAR_ICON_FONT", &icon_font, MAX_ICON_FONT_BYTES)?;
            config.presentation.icon_font = Some(icon_font);
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
            validate_text_bytes("font", &font, MAX_FONT_BYTES)?;
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
        apply_positive(
            "presentation.dock_item_size",
            file.presentation.dock_item_size,
            &mut presentation.dock_item_size,
        )?;
        if let Some(aspect_ratio) = file.presentation.dock_item_aspect_ratio {
            if !aspect_ratio.is_finite() || aspect_ratio < 1.0 {
                return Err(ConfigError::InvalidValue {
                    field: "presentation.dock_item_aspect_ratio",
                    reason: "must be a finite value greater than or equal to one",
                });
            }
            presentation.dock_item_aspect_ratio = aspect_ratio;
        }
        apply_non_negative(
            "presentation.dock_item_gap",
            file.presentation.dock_item_gap,
            &mut presentation.dock_item_gap,
        )?;
        apply_non_negative(
            "presentation.dock_shelf_padding",
            file.presentation.dock_shelf_padding,
            &mut presentation.dock_shelf_padding,
        )?;
        apply_non_negative(
            "presentation.dock_corner_radius",
            file.presentation.dock_corner_radius,
            &mut presentation.dock_corner_radius,
        )?;
        if let Some(scale) = file.presentation.dock_hover_scale {
            if !scale.is_finite() || scale < 1.0 {
                return Err(ConfigError::InvalidValue {
                    field: "presentation.dock_hover_scale",
                    reason: "must be a finite value greater than or equal to one",
                });
            }
            presentation.dock_hover_scale = scale;
        }
        apply_positive(
            "presentation.dock_influence_radius",
            file.presentation.dock_influence_radius,
            &mut presentation.dock_influence_radius,
        )?;
        apply_non_negative(
            "presentation.dock_separator_width",
            file.presentation.dock_separator_width,
            &mut presentation.dock_separator_width,
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
        if let Some(mut labels) = file.presentation.tag_labels {
            if labels.is_empty() {
                return Err(ConfigError::InvalidValue {
                    field: "presentation.tag_labels",
                    reason: "labels must be a non-empty list of non-empty strings",
                });
            }
            // TagId is a u32 bit position, so labels after the model's final
            // representable tag can never be projected by any frontend. Drop
            // them before validation so unreachable junk cannot reject an
            // otherwise valid configuration.
            labels.truncate(MAX_MODEL_TAGS);
            if labels.iter().any(|label| label.trim().is_empty()) {
                return Err(ConfigError::InvalidValue {
                    field: "presentation.tag_labels",
                    reason: "labels must be a non-empty list of non-empty strings",
                });
            }
            if let Some(label) = labels
                .iter()
                .find(|label| label.len() > MAX_TAG_LABEL_BYTES)
            {
                validate_text_bytes("presentation.tag_labels", label, MAX_TAG_LABEL_BYTES)?;
            }
            presentation.tag_labels = labels;
        }
        if let Some(icon_font) = file.presentation.icon_font {
            if icon_font.trim().is_empty() {
                return Err(ConfigError::InvalidValue {
                    field: "presentation.icon_font",
                    reason: "icon_font must name a font family",
                });
            }
            validate_text_bytes("presentation.icon_font", &icon_font, MAX_ICON_FONT_BYTES)?;
            presentation.icon_font = Some(icon_font);
        }

        config.glass = glass_from_file(file.glass)?;
        Ok(config)
    }
}

fn read_config_text(path: &Path) -> io::Result<String> {
    // Check before opening so an accidentally configured FIFO or device cannot
    // stall a frontend at startup. Symlinks to ordinary config files continue
    // to work because `metadata` follows the link.
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration path is not a regular file",
        ));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(config_too_large());
    }

    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration path is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(config_too_large());
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn config_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("configuration exceeds the {MAX_CONFIG_BYTES}-byte limit"),
    )
}

/// Validate the `[glass]` section.
///
/// Only the values that would be *meaningless* rather than merely extreme are
/// rejected here; ranges are the recipe's business and it clamps them.
fn glass_from_file(file: FileGlass) -> Result<GlassConfig, ConfigError> {
    let wallpaper = match file.wallpaper {
        Some(path) if path.trim().is_empty() => {
            return Err(ConfigError::InvalidValue {
                field: "glass.wallpaper",
                reason: "path must not be empty",
            });
        }
        Some(path) => {
            validate_text_bytes("glass.wallpaper", &path, MAX_WALLPAPER_PATH_BYTES)?;
            Some(PathBuf::from(path))
        }
        None => None,
    };
    if let Some(mode) = &file.wallpaper_mode {
        validate_text_bytes("glass.wallpaper_mode", mode, MAX_WALLPAPER_MODE_BYTES)?;
        if !matches!(
            mode.trim().to_ascii_lowercase().as_str(),
            "fill" | "fit" | "stretch" | "center"
        ) {
            return Err(ConfigError::InvalidValue {
                field: "glass.wallpaper_mode",
                reason: "must be one of fill, fit, stretch, center",
            });
        }
    }
    if file.downscale == Some(0) {
        return Err(ConfigError::InvalidValue {
            field: "glass.downscale",
            reason: "must be at least 1",
        });
    }
    if let Some(saturation) = file.saturation
        && (!saturation.is_finite() || saturation < 0.0)
    {
        return Err(ConfigError::InvalidValue {
            field: "glass.saturation",
            reason: "must be a finite non-negative value",
        });
    }

    Ok(GlassConfig {
        wallpaper,
        wallpaper_mode: file.wallpaper_mode,
        downscale: file.downscale,
        blur_radius: file.blur_radius,
        blur_passes: file.blur_passes,
        saturation: file.saturation,
        pad: file.pad,
    })
}

fn validate_text_bytes(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ConfigError> {
    if value.len() > max_bytes {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "text exceeds the per-field byte limit",
        });
    }
    Ok(())
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

    fn test_path(name: &str) -> PathBuf {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "xbar-config-test-{name}-{}-{sequence}",
            std::process::id()
        ))
    }

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
dock_item_size = 20.0
dock_item_aspect_ratio = 1.6
dock_hover_scale = 1.7
dock_influence_radius = 60.0
tag_labels = ["a", "b", "c"]
"#,
        )
        .unwrap();

        assert_eq!(config.font, "JetBrainsMono Nerd Font 12");
        assert_eq!(config.theme, ThemeMode::Light);
        assert_eq!(config.background_opacity, Some(0.85));
        assert_eq!(config.presentation.bar_height, 42.0);
        assert_eq!(config.presentation.font_size, 14.5);
        assert_eq!(config.presentation.dock_item_size, 20.0);
        assert_eq!(config.presentation.dock_item_aspect_ratio, 1.6);
        assert_eq!(config.presentation.dock_hover_scale, 1.7);
        assert_eq!(config.presentation.dock_influence_radius, 60.0);
        assert_eq!(config.presentation.tag_labels, vec!["a", "b", "c"]);
        // Untouched fields keep their defaults.
        assert_eq!(
            config.presentation.item_gap,
            PresentationConfig::default().item_gap
        );
    }

    #[test]
    fn tag_labels_beyond_the_model_capacity_are_not_retained() {
        let labels = (0..MAX_MODEL_TAGS + 4)
            .map(|index| format!(r#""tag-{index}""#))
            .collect::<Vec<_>>()
            .join(",");
        let config =
            BarConfig::from_toml(&format!("[presentation]\ntag_labels = [{labels}]")).unwrap();

        assert_eq!(config.presentation.tag_labels.len(), MAX_MODEL_TAGS);
        let last = format!("tag-{}", MAX_MODEL_TAGS - 1);
        assert_eq!(
            config.presentation.tag_labels.last().map(String::as_str),
            Some(last.as_str())
        );

        let mut labels = vec!["\"ok\"".to_owned(); MAX_MODEL_TAGS];
        let unreachable = "x".repeat(MAX_TAG_LABEL_BYTES + 1);
        labels.push(format!("{unreachable:?}"));
        let config = BarConfig::from_toml(&format!(
            "[presentation]\ntag_labels = [{}]",
            labels.join(",")
        ))
        .unwrap();
        assert_eq!(config.presentation.tag_labels.len(), MAX_MODEL_TAGS);
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
            BarConfig::from_toml("[presentation]\ndock_item_aspect_ratio = 0.75"),
            Err(ConfigError::InvalidValue {
                field: "presentation.dock_item_aspect_ratio",
                ..
            })
        ));
        assert!(matches!(
            BarConfig::from_toml("[presentation]\ndock_hover_scale = 0.99"),
            Err(ConfigError::InvalidValue {
                field: "presentation.dock_hover_scale",
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
    fn oversized_text_fields_are_rejected_before_retention() {
        let font = "f".repeat(MAX_FONT_BYTES + 1);
        assert!(matches!(
            BarConfig::from_toml(&format!("font = {font:?}")),
            Err(ConfigError::InvalidValue { field: "font", .. })
        ));

        let tag = "t".repeat(MAX_TAG_LABEL_BYTES + 1);
        assert!(matches!(
            BarConfig::from_toml(&format!("[presentation]\ntag_labels = [{tag:?}]")),
            Err(ConfigError::InvalidValue {
                field: "presentation.tag_labels",
                ..
            })
        ));

        let icon_font = "i".repeat(MAX_ICON_FONT_BYTES + 1);
        assert!(matches!(
            BarConfig::from_toml(&format!("[presentation]\nicon_font = {icon_font:?}")),
            Err(ConfigError::InvalidValue {
                field: "presentation.icon_font",
                ..
            })
        ));

        let wallpaper = "w".repeat(MAX_WALLPAPER_PATH_BYTES + 1);
        assert!(matches!(
            BarConfig::from_toml(&format!("[glass]\nwallpaper = {wallpaper:?}")),
            Err(ConfigError::InvalidValue {
                field: "glass.wallpaper",
                ..
            })
        ));

        let mode = "m".repeat(MAX_WALLPAPER_MODE_BYTES + 1);
        assert!(matches!(
            BarConfig::from_toml(&format!("[glass]\nwallpaper_mode = {mode:?}")),
            Err(ConfigError::InvalidValue {
                field: "glass.wallpaper_mode",
                ..
            })
        ));
    }

    #[test]
    fn glass_overrides_only_what_the_file_names() {
        let config = BarConfig::from_toml(
            r#"
[glass]
wallpaper = "/home/user/.config/jwm/wallpaper.jpg"
wallpaper_mode = "Fit"
blur_radius = 10
"#,
        )
        .unwrap();

        assert_eq!(
            config.glass.wallpaper.as_deref(),
            Some(Path::new("/home/user/.config/jwm/wallpaper.jpg"))
        );
        assert_eq!(config.glass.blur_radius, Some(10));
        assert_eq!(config.glass.saturation, None);

        #[cfg(feature = "glass")]
        {
            let params = config.glass.params();
            let default = crate::glass::GlassParams::default();
            assert_eq!(params.blur_radius, 10);
            assert_eq!(params.saturation, default.saturation);
            assert_eq!(params.downscale, default.downscale);
        }
        #[cfg(feature = "glass-wallpaper")]
        assert_eq!(
            config.glass.mode(),
            crate::glass::wallpaper::WallpaperMode::Fit
        );
    }

    #[test]
    fn glass_rejects_values_that_could_not_mean_anything() {
        assert!(matches!(
            BarConfig::from_toml("[glass]\nwallpaper = \"\""),
            Err(ConfigError::InvalidValue {
                field: "glass.wallpaper",
                ..
            })
        ));
        assert!(matches!(
            BarConfig::from_toml("[glass]\nwallpaper_mode = \"tile\""),
            Err(ConfigError::InvalidValue {
                field: "glass.wallpaper_mode",
                ..
            })
        ));
        assert!(matches!(
            BarConfig::from_toml("[glass]\ndownscale = 0"),
            Err(ConfigError::InvalidValue {
                field: "glass.downscale",
                ..
            })
        ));
        assert!(matches!(
            BarConfig::from_toml("[glass]\nsaturation = -1.0"),
            Err(ConfigError::InvalidValue {
                field: "glass.saturation",
                ..
            })
        ));
        assert!(matches!(
            BarConfig::from_toml("[glass]\nblur = 3"),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn load_reports_missing_files_with_their_path() {
        let error = BarConfig::load(Path::new("/definitely/missing/xbar.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Read { .. }));
        assert!(error.to_string().contains("/definitely/missing/xbar.toml"));
    }

    #[test]
    fn load_rejects_oversized_files_before_parsing() {
        let path = test_path("oversized");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CONFIG_BYTES + 1).unwrap();

        let error = BarConfig::load(&path).unwrap_err();
        let ConfigError::Read { source, .. } = error else {
            panic!("oversized input should be a read error");
        };
        assert_eq!(source.kind(), io::ErrorKind::InvalidData);
        assert!(source.to_string().contains("byte limit"));

        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_non_regular_sources_without_opening_them() {
        use std::os::unix::net::UnixListener;

        let path = test_path("socket");
        let listener = UnixListener::bind(&path).unwrap();

        let error = BarConfig::load(&path).unwrap_err();
        let ConfigError::Read { source, .. } = error else {
            panic!("a socket should be a read error");
        };
        assert_eq!(source.kind(), io::ErrorKind::InvalidInput);

        drop(listener);
        std::fs::remove_file(path).unwrap();
    }
}
