//! Shared horizontal desktop-header styling, independent of a frontend toolkit.
use crate::placement::BarEdge;
use crate::presentation::{PresentationConfig, PresentationLabels};
use crate::tag_contrast::TagContrastMode;

impl PresentationConfig {
    /// Select the desktop header on horizontal bars, regardless of sidebar mode.
    /// Stock labels and dimensions acquire the common profile; custom values
    /// remain intact. Call before automatic stock Nerd Font substitution.
    /// Returns whether the frontend should use its horizontal capsule surface.
    pub fn apply_desktop_header_profile(&mut self) -> bool {
        if !matches!(self.edge, BarEdge::Top | BarEdge::Bottom) {
            return false;
        }
        let stock = Self::default();
        self.desktop_header = true;
        self.visibility.minimized_windows = false;
        self.visibility.media = false;
        self.visibility.brightness = false;
        self.visibility.theme = false;
        self.visibility.screenshot = false;
        if self.tag_contrast == TagContrastMode::Auto {
            self.tag_contrast = TagContrastMode::Glass;
        }
        if self.tag_labels == stock.tag_labels {
            self.tag_labels = (1..=crate::model::MAX_MODEL_TAGS)
                .map(|number| number.to_string())
                .collect();
        }
        if self.labels == stock.labels && self.icon_set.is_none() {
            self.labels = PresentationLabels {
                clock: String::new(),
                screenshot: "▣".into(),
                theme_dark: "◐".into(),
                theme_light: "☼".into(),
                monitor: "▣".into(),
                cpu: "CPU".into(),
                memory: "MEM".into(),
                audio: "♪".into(),
                muted: "×".into(),
                brightness: "☼".into(),
                battery: "BAT".into(),
                charging: "+".into(),
                network: "NET".into(),
                network_offline: "×".into(),
                media_playing: "▶".into(),
                media_paused: "Ⅱ".into(),
                shell_routes: ["▦", "⌕", "♧", "▤", "▣", "▱", "◐"].map(str::to_owned),
            };
        }
        for (value, default, profile) in [
            (&mut self.font_size, stock.font_size, 12.0),
            (&mut self.horizontal_padding, stock.horizontal_padding, 12.0),
            (&mut self.vertical_padding, stock.vertical_padding, 7.0),
            (&mut self.item_gap, stock.item_gap, 8.0),
            (
                &mut self.pill_horizontal_padding,
                stock.pill_horizontal_padding,
                12.0,
            ),
            (&mut self.corner_radius, stock.corner_radius, 16.0),
        ] {
            if *value == default {
                *value = profile;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn horizontal_profile_is_independent_and_idempotent() {
        let mut config = PresentationConfig::default();
        assert!(config.apply_desktop_header_profile());
        assert!(config.desktop_header);
        assert_eq!(
            (
                config.font_size,
                config.horizontal_padding,
                config.vertical_padding,
                config.item_gap,
                config.pill_horizontal_padding,
                config.corner_radius
            ),
            (12.0, 12.0, 7.0, 8.0, 12.0, 16.0)
        );
        assert_eq!(config.tag_labels[0], "1");
        assert_eq!(
            config.tag_labels.last().unwrap(),
            &crate::model::MAX_MODEL_TAGS.to_string()
        );
        let applied = config.clone();
        assert!(config.apply_desktop_header_profile());
        assert_eq!(config, applied);
    }
    #[test]
    fn custom_labels_dimensions_and_vertical_policy_survive() {
        let mut config = PresentationConfig::default();
        config.font_size = 18.0;
        config.item_gap = 17.0;
        config.tag_labels = vec!["Work".into()];
        config.labels.cpu = "Processor".into();
        let custom = config.clone();
        assert!(config.apply_desktop_header_profile());
        assert_eq!(config.font_size, custom.font_size);
        assert_eq!(config.item_gap, custom.item_gap);
        assert_eq!(config.tag_labels, custom.tag_labels);
        assert_eq!(config.labels, custom.labels);
        for edge in [BarEdge::Left, BarEdge::Right] {
            let mut vertical = custom.clone();
            vertical.edge = edge;
            let saved = vertical.clone();
            assert!(!vertical.apply_desktop_header_profile());
            assert_eq!(vertical, saved);
        }
    }
}
