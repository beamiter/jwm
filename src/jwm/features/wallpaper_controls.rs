//! Session-only static wallpaper controls, shared by the native picker backends.

use crate::config::BehaviorConfig;

const PREFIX: &str = "jwm-wallpaper:image:";
pub(crate) const REFRESH: &str = "jwm-wallpaper:image:refresh";
pub(crate) const ROTATE_GALLERY: &str = "jwm-wallpaper:image:rotate-gallery";

pub(crate) fn is_choice(key: &str) -> bool {
    key.starts_with(PREFIX)
}

pub(crate) fn rows(behavior: &BehaviorConfig) -> Vec<(String, String)> {
    let mut rows = vec![(REFRESH.into(), "\u{f021} Refresh image library · F5".into())];
    for (mode, label) in [
        ("fill", "Fill · crop to screen"),
        ("fit", "Fit · show whole image"),
        ("stretch", "Stretch · match screen"),
        ("center", "Center · native pixels"),
    ] {
        let marker = if behavior.wallpaper_mode == mode {
            "\u{f00c}"
        } else {
            " "
        };
        rows.push((
            format!("{PREFIX}mode:{mode}"),
            format!("{marker} Global image sizing: {label}"),
        ));
    }
    rows.push((
        format!("{PREFIX}crossfade"),
        format!(
            "Global image transitions: {}",
            if behavior.wallpaper_crossfade {
                "On"
            } else {
                "Off"
            }
        ),
    ));
    rows.push((
        format!("{PREFIX}colors"),
        format!(
            "Wallpaper accent colors: {}",
            if behavior.wallpaper_colors {
                "On"
            } else {
                "Off"
            }
        ),
    ));
    rows.push((
        ROTATE_GALLERY.into(),
        "Rotate this image library daily · all screens".into(),
    ));
    if behavior.wallpaper_rotation == "daily" {
        rows.push((
            format!("{PREFIX}stop-rotation"),
            "Stop daily rotation · restore static wallpapers".into(),
        ));
    }
    rows
}

/// Build one validated transaction. A filtered search never supplies the
/// collection: callers pass all image paths from the immutable library.
pub(crate) fn changes(
    behavior: &BehaviorConfig,
    choice: &str,
    paths: &[String],
) -> Result<Vec<(String, serde_json::Value)>, String> {
    use serde_json::json;
    let action = choice.strip_prefix(PREFIX).ok_or("Unknown image control")?;
    let changes = match action {
        "mode:fill" | "mode:fit" | "mode:stretch" | "mode:center" => {
            vec![("behavior.wallpaper_mode".into(), json!(&action[5..]))]
        }
        "colors" => vec![(
            "behavior.wallpaper_colors".into(),
            json!(!behavior.wallpaper_colors),
        )],
        "crossfade" => {
            let mut changes = vec![(
                "behavior.wallpaper_crossfade".into(),
                json!(!behavior.wallpaper_crossfade),
            )];
            // Zero is valid while disabled; enabling must restore a usable duration.
            if !behavior.wallpaper_crossfade && behavior.wallpaper_crossfade_duration_ms == 0 {
                changes.push((
                    "behavior.wallpaper_crossfade_duration_ms".into(),
                    json!(500),
                ));
            }
            changes
        }
        "rotate-gallery" => {
            if paths.is_empty() {
                return Err("No images to rotate. Add images to the library and press F5.".into());
            }
            vec![
                ("behavior.wallpaper_effect".into(), json!("off")),
                // Clear today's manual pick before starting this explicit new collection.
                ("behavior.wallpaper_rotation".into(), json!("off")),
                ("behavior.wallpaper_rotation_paths".into(), json!(paths)),
                ("behavior.wallpaper_rotation".into(), json!("daily")),
            ]
        }
        "stop-rotation" => vec![("behavior.wallpaper_rotation".into(), json!("off"))],
        _ => return Err("Unknown image control".into()),
    };
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    #[test]
    fn static_wallpaper_controls_validate_and_leave_live_scenes_intact() {
        let mut config = Config::default();
        config
            .set_value("behavior.wallpaper_effect", &json!("astronomy"))
            .unwrap();
        for mode in ["fit", "stretch", "center", "fill"] {
            config
                .set_values(
                    &changes(config.behavior(), &format!("{PREFIX}mode:{mode}"), &[]).unwrap(),
                )
                .unwrap();
            assert_eq!(config.behavior().wallpaper_mode, mode);
            assert_eq!(config.behavior().wallpaper_effect, "astronomy");
        }
        assert!(changes(config.behavior(), &format!("{PREFIX}mode:invalid"), &[]).is_err());
        assert!(changes(config.behavior(), ROTATE_GALLERY, &[]).is_err());
    }

    #[test]
    fn gallery_rotation_replaces_manual_pick_and_preserves_saved_static_path() {
        let mut config = Config::default();
        config
            .set_values(&[
                ("behavior.wallpaper".into(), json!("/static.png")),
                ("behavior.wallpaper_rotation".into(), json!("daily")),
                (
                    "behavior.wallpaper_rotation_paths".into(),
                    json!(["/old.png"]),
                ),
            ])
            .unwrap();
        let mut config = config.with_picker_wallpaper("/manual.png", 100).unwrap();
        let paths = vec!["/a.png".into(), "/b.png".into()];
        config
            .set_values(&changes(config.behavior(), ROTATE_GALLERY, &paths).unwrap())
            .unwrap();
        assert_eq!(config.behavior().wallpaper_rotation, "daily");
        assert_eq!(config.behavior().wallpaper_rotation_paths, paths);
        assert!(config.behavior().wallpaper_daily_pick.is_none());
        assert_eq!(config.behavior().wallpaper, "/static.png");
        config
            .set_values(
                &changes(config.behavior(), &format!("{PREFIX}stop-rotation"), &[]).unwrap(),
            )
            .unwrap();
        assert_eq!(config.behavior().wallpaper_rotation, "off");
        assert_eq!(config.behavior().wallpaper, "/static.png");
        assert_eq!(config.behavior().wallpaper_rotation_paths, paths);
    }

    #[test]
    fn crossfade_can_be_reenabled_after_a_zero_duration_and_invalid_batches_are_atomic() {
        let mut config = Config::default();
        config
            .set_values(&[
                ("behavior.wallpaper_crossfade".into(), json!(false)),
                ("behavior.wallpaper_crossfade_duration_ms".into(), json!(0)),
            ])
            .unwrap();
        config
            .set_values(&changes(config.behavior(), &format!("{PREFIX}crossfade"), &[]).unwrap())
            .unwrap();
        assert!(config.behavior().wallpaper_crossfade);
        assert_eq!(config.behavior().wallpaper_crossfade_duration_ms, 500);
        for invalid in [json!(-1), json!(30_001), json!(1.5), json!("500")] {
            assert!(
                config
                    .set_values(&[
                        ("behavior.wallpaper_crossfade".into(), json!(false)),
                        ("behavior.wallpaper_crossfade_duration_ms".into(), invalid),
                    ])
                    .is_err()
            );
            assert!(config.behavior().wallpaper_crossfade);
            assert_eq!(config.behavior().wallpaper_crossfade_duration_ms, 500);
        }
    }
}
