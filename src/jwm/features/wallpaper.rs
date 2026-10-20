//! Wallpaper picker.
//!
//! JWM already renders wallpapers from `behavior.wallpaper`; this module only
//! finds the candidates and applies the choice through the same configuration
//! path a `set_config` would take, so there is one code path that changes the
//! wallpaper rather than two.
//!
//! Directory scanning and name formatting are pure so the extension filter,
//! the ordering, and the "which directory" decision are unit tested without
//! touching the user's pictures.

use std::path::{Path, PathBuf};

/// Picker-only identity. Never pass it to an image decoder or store it as a path.
pub(crate) const ASTRONOMY_CHOICE: &str = "jwm-effect:astronomy";
pub(crate) const SOLAR_SYSTEM_CHOICE: &str = "jwm-effect:solar-system";
pub(crate) const SCENE_CHOICES: [(&str, &str, &str); 12] = [
    (ASTRONOMY_CHOICE, "earth", "Astronomy Earth"),
    (
        SOLAR_SYSTEM_CHOICE,
        "solar-system",
        "Solar System 3D · drag to orbit",
    ),
    ("jwm-effect:moon", "moon", "Moon · lunar surface"),
    ("jwm-effect:sun", "sun", "Sun · stellar surface"),
    ("jwm-effect:mercury", "mercury", "Mercury · cratered world"),
    ("jwm-effect:venus", "venus", "Venus · cloud-covered world"),
    ("jwm-effect:mars", "mars", "Mars · red planet"),
    ("jwm-effect:jupiter", "jupiter", "Jupiter · gas giant"),
    ("jwm-effect:saturn", "saturn", "Saturn · rings"),
    ("jwm-effect:uranus", "uranus", "Uranus · ice giant"),
    ("jwm-effect:neptune", "neptune", "Neptune · ice giant"),
    (
        "jwm-effect:galaxy",
        "galaxy",
        "Milky Way panorama · 360° sky",
    ),
];

const CONTROL_PREFIX: &str = "jwm-wallpaper:";
const TARGET_PREFIX: &str = "jwm-wallpaper:target:";

pub(crate) fn is_effect_choice(choice: &str) -> bool {
    choice_scene(choice).is_some()
}

pub(crate) fn is_control_choice(choice: &str) -> bool {
    choice.starts_with(CONTROL_PREFIX)
}

pub(crate) fn choice_scene(choice: &str) -> Option<&'static str> {
    SCENE_CHOICES
        .iter()
        .find_map(|(key, scene, _)| (*key == choice).then_some(*scene))
}

fn scene_choice(scene: &str) -> &'static str {
    SCENE_CHOICES
        .iter()
        .find_map(|(key, name, _)| (*name == scene).then_some(*key))
        .unwrap_or(ASTRONOMY_CHOICE)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum WallpaperTarget {
    #[default]
    All,
    Output(String),
}

impl WallpaperTarget {
    pub(crate) fn from_key(key: &str) -> Option<Self> {
        let target = key.strip_prefix(TARGET_PREFIX)?;
        if target == "all" {
            Some(Self::All)
        } else {
            target
                .strip_prefix("output:")
                .map(|name| Self::Output(name.into()))
        }
    }

    fn key(&self) -> String {
        match self {
            Self::All => format!("{TARGET_PREFIX}all"),
            Self::Output(name) => format!("{TARGET_PREFIX}output:{name}"),
        }
    }

    pub(crate) fn next(&self, connectors: &[String]) -> Self {
        let next = match self {
            Self::All => connectors.first(),
            Self::Output(name) => connectors
                .iter()
                .position(|item| item == name)
                .and_then(|index| connectors.get(index + 1)),
        };
        next.map_or(Self::All, |name| Self::Output(name.clone()))
    }

    pub(crate) fn is_connected(&self, connectors: &[String]) -> bool {
        match self {
            Self::All => true,
            Self::Output(name) => connectors.contains(name),
        }
    }
}

pub(crate) fn global_scene(behavior: &crate::config::BehaviorConfig) -> &str {
    if behavior.wallpaper_effect == "solar-system" {
        "solar-system"
    } else {
        &behavior.wallpaper_astronomy_scene
    }
}

fn output_scene<'a>(
    behavior: &'a crate::config::BehaviorConfig,
    connector: &str,
    connectors: &[String],
) -> &'a str {
    match behavior.wallpaper_astronomy_layout.as_str() {
        "independent" => behavior
            .wallpaper_astronomy_outputs
            .get(connector)
            .map_or_else(|| global_scene(behavior), |output| output.scene.as_str()),
        "earth-moon" if behavior.wallpaper_effect != "solar-system" => {
            if let Some(body) = behavior.wallpaper_astronomy_bodies.get(connector) {
                return body;
            }
            // The original renderer alternates sorted connector names. A
            // one-output edit freezes this old assignment for every survivor.
            let mut sorted: Vec<_> = connectors.iter().collect();
            sorted.sort();
            if sorted
                .iter()
                .position(|name| name.as_str() == connector)
                .unwrap_or(0)
                % 2
                == 0
            {
                "earth"
            } else {
                "moon"
            }
        }
        "per-output" if behavior.wallpaper_effect != "solar-system" => "earth",
        _ => global_scene(behavior),
    }
}

pub(crate) fn target_scene<'a>(
    behavior: &'a crate::config::BehaviorConfig,
    target: &WallpaperTarget,
    connectors: &[String],
) -> &'a str {
    match target {
        WallpaperTarget::All => global_scene(behavior),
        WallpaperTarget::Output(name) => output_scene(behavior, name, connectors),
    }
}

pub(crate) fn astronomy_controls(
    behavior: &crate::config::BehaviorConfig,
    target: &WallpaperTarget,
    connectors: &[String],
) -> Vec<(String, String)> {
    let target_label = match target {
        WallpaperTarget::All => "All screens".into(),
        WallpaperTarget::Output(name) => {
            let absent = if connectors.contains(name) {
                ""
            } else {
                " · disconnected"
            };
            format!("{name}{absent}")
        }
    };
    let mut controls = vec![(
        target.key(),
        format!("\u{f108} Apply to: {target_label} · change"),
    )];
    for (layout, label) in [
        ("span", "Span · one scene across all screens"),
        ("mirror", "Mirror · one shared view on every screen"),
        ("independent", "Independent · a scene or group per screen"),
    ] {
        let marker = if behavior.wallpaper_astronomy_layout == layout {
            "\u{f00c}"
        } else {
            " "
        };
        controls.push((
            format!("{CONTROL_PREFIX}layout:{layout}"),
            format!("\u{f108} {marker} {label}"),
        ));
    }
    if let WallpaperTarget::Output(name) = target {
        if let Some(output) = behavior.wallpaper_astronomy_outputs.get(name)
            && !output.group.is_empty()
            && behavior.wallpaper_astronomy_layout == "independent"
        {
            controls.push((
                format!("{CONTROL_PREFIX}detach"),
                format!(
                    "\u{f127} Leave group {} · make {name} independent",
                    output.group
                ),
            ));
        }
        for other in connectors.iter().filter(|other| *other != name) {
            let scene = output_scene(behavior, other, connectors);
            controls.push((
                format!("{CONTROL_PREFIX}share:{other}"),
                format!(
                    "\u{f0c1} Share with {other} · {}",
                    crate::config::astronomy::scene_label(scene)
                ),
            ));
        }
    }
    let mode = if behavior.wallpaper_astronomy_presentation == "scientific" {
        "Scientific"
    } else {
        "Illustrative"
    };
    controls.push((
        format!("{CONTROL_PREFIX}presentation"),
        format!("\u{f0eb} Global lighting: {mode} · change"),
    ));
    let orbit = if behavior.wallpaper_solar_orbit_mode == "ephemeris" {
        "JPL approximation"
    } else {
        "Illustrative orbit tour"
    };
    controls.push((
        format!("{CONTROL_PREFIX}orbit"),
        format!("\u{f135} Global solar orbits: {orbit} · change"),
    ));
    controls.push((
        format!("{CONTROL_PREFIX}detail:clock"),
        format!(
            "Global date-derived clock: {:.0}× · {:+.0} days",
            behavior.wallpaper_solar_time_scale, behavior.wallpaper_solar_time_offset_days
        ),
    ));
    for (action, label) in [
        ("now", "Global date-derived clock: Now · UTC and 1×"),
        ("pause", "Global date-derived clock: Pause orbits"),
        ("realtime", "Global date-derived clock: 1× · UTC + offset"),
        ("fast", "Global date-derived clock: 1 day per second"),
        ("day-back", "Global date-derived clock: offset −1 day"),
        ("day-forward", "Global date-derived clock: offset +1 day"),
        ("month-back", "Global date-derived clock: offset −30 days"),
        (
            "month-forward",
            "Global date-derived clock: offset +30 days",
        ),
    ] {
        controls.push((
            format!("{CONTROL_PREFIX}time:{action}"),
            format!("\u{f017} {label}"),
        ));
    }
    controls.push((
        format!("{CONTROL_PREFIX}detail:spin"),
        format!(
            "Solar spin: {:.3}× · independent of orbital date",
            behavior.wallpaper_solar_spin_speed
        ),
    ));
    controls.push((
        format!("{CONTROL_PREFIX}info"),
        format!(
            "\u{f05a} About {} · facts, materials and sources",
            crate::config::astronomy::scene_label(target_scene(behavior, target, connectors)),
        ),
    ));
    for (index, line) in scene_information(target_scene(behavior, target, connectors))
        .into_iter()
        .enumerate()
    {
        controls.push((format!("{CONTROL_PREFIX}detail:{index}"), line));
    }
    controls
}

pub(crate) fn scene_information(scene: &str) -> Vec<String> {
    use crate::backend::compositor_common::celestial_catalog::body_by_id;
    let mut lines = Vec::new();
    let mut wrap = |text: String| {
        const WIDTH: usize = 60;
        let mut line = String::new();
        for mut word in text.split_whitespace() {
            // Long source URLs also occupy bounded rows; the entire URL
            // remains readable across consecutive lines instead of clipping.
            while let Some((boundary, _)) = word.char_indices().nth(WIDTH) {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                lines.push(word[..boundary].into());
                word = &word[boundary..];
            }
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > WIDTH {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            lines.push(line);
        }
    };
    if let Some(body) = body_by_id(scene) {
        wrap(format!("{} · {}", body.name, body.name_zh));
        wrap(format!(
            "Mean radius: {:.1} km · axial obliquity: {:.3}°",
            body.radius_km, body.obliquity_degrees
        ));
        wrap(format!(
            "Sidereal rotation: {:.4} hours{}",
            body.rotation_hours.abs(),
            if body.rotation_hours < 0.0 {
                " · retrograde"
            } else {
                ""
            }
        ));
        if let Some(period) = body.orbital_period_days {
            let parent = body
                .orbit_parent
                .and_then(body_by_id)
                .map_or("its parent", |parent| parent.name);
            wrap(format!("Sidereal orbit: {period:.4} days around {parent}"));
        }
        wrap(body.description.into());
        wrap(format!("Material: {}", body.material_label));
        wrap(body.material_note.into());
        wrap(format!("NASA reference: {}", body.source_url));
        if body.physical_source_url != body.source_url {
            wrap(format!("Physical quantities: {}", body.physical_source_url));
        }
        if let Some(source) = body.texture_source_url {
            wrap(format!("Texture provenance: {source}"));
        }
    } else if scene == "solar-system" {
        wrap("Solar System · 太阳系 · Sun and eight planets".into());
        wrap("Ephemeris mode: JPL approximate positions; valid only within its documented date range.".into());
        wrap("Both presentation modes compress orbital distances and enlarge bodies. The view is not to scale.".into());
        wrap(
            "The solar orbital clock is separate from Earth/Moon's real UTC day/night cycle."
                .into(),
        );
        wrap("Orbital date at 1× follows UTC; surface spin uses its own multiplier, default 718.035×. Set wallpaper_solar_spin_speed to 1 for real-time sidereal spin.".into());
        wrap("Source: https://ssd.jpl.nasa.gov/planets/approx_pos.html".into());
    } else {
        wrap("Milky Way panorama · 银河全景".into());
        wrap("A navigable 360° celestial environment. Decorative stars and nebulae are illustrative, not a complete astrometric catalogue.".into());
    }
    wrap("Controls: drag a body to rotate; drag the sky to look around; wheel to zoom. Solar-system bodies are selectable.".into());
    lines
}

fn independent_output_snapshot(
    behavior: &crate::config::BehaviorConfig,
    connectors: &[String],
    preserve_saved: bool,
) -> std::collections::BTreeMap<String, crate::config::AstronomyOutputConfig> {
    let mut outputs = behavior.wallpaper_astronomy_outputs.clone();
    for name in connectors {
        if behavior.wallpaper_astronomy_layout == "independent" || preserve_saved {
            outputs.entry(name.clone()).or_insert_with(|| {
                crate::config::AstronomyOutputConfig::independent(output_scene(
                    behavior, name, connectors,
                ))
            });
        } else {
            outputs.insert(
                name.clone(),
                crate::config::AstronomyOutputConfig::independent(output_scene(
                    behavior, name, connectors,
                )),
            );
        }
    }
    outputs
}

/// Produce a bounded atomic config transaction from a picker action. The
/// current visible scene is captured before entering independent mode, so a
/// choice for one output cannot silently replace another output's backdrop.
pub(crate) fn astronomy_changes(
    behavior: &crate::config::BehaviorConfig,
    target: &WallpaperTarget,
    connectors: &[String],
    choice: &str,
) -> Result<Vec<(String, serde_json::Value)>, String> {
    use crate::config::astronomy::{join_output_scene, set_output_scene};
    if !target.is_connected(connectors) {
        return Err("The selected display is disconnected. Choose another target; its saved scene is retained.".into());
    }
    let mut changes = Vec::new();
    let mut set = |field: &str, value: serde_json::Value| {
        changes.push((format!("behavior.{field}"), value));
    };
    if let Some(scene) = choice_scene(choice) {
        match target {
            WallpaperTarget::All => {
                set("wallpaper_astronomy_scene", serde_json::json!(scene));
                let mut outputs = behavior.wallpaper_astronomy_outputs.clone();
                for output in outputs.values_mut() {
                    output.scene = scene.into();
                }
                if !outputs.is_empty() {
                    set("wallpaper_astronomy_outputs", serde_json::json!(outputs));
                }
                if matches!(
                    behavior.wallpaper_astronomy_layout.as_str(),
                    "earth-moon" | "per-output"
                ) {
                    set("wallpaper_astronomy_layout", serde_json::json!("mirror"));
                }
                // New selections use the unified scene route. The legacy
                // solar-system effect deliberately spans the desktop and
                // would otherwise override a requested Mirror layout.
                set("wallpaper_effect", serde_json::json!("astronomy"));
            }
            WallpaperTarget::Output(name) => {
                let mut outputs = independent_output_snapshot(behavior, connectors, false);
                set_output_scene(&mut outputs, name, scene);
                set(
                    "wallpaper_astronomy_scene",
                    serde_json::json!(global_scene(behavior)),
                );
                set("wallpaper_astronomy_outputs", serde_json::json!(outputs));
                set(
                    "wallpaper_astronomy_layout",
                    serde_json::json!("independent"),
                );
                set("wallpaper_effect", serde_json::json!("astronomy"));
            }
        }
    } else if let Some(layout) = choice.strip_prefix("jwm-wallpaper:layout:") {
        if !["span", "mirror", "independent"].contains(&layout) {
            return Err("Unknown celestial layout".into());
        }
        if layout == "independent" {
            set(
                "wallpaper_astronomy_outputs",
                serde_json::json!(independent_output_snapshot(behavior, connectors, true)),
            );
        }
        set(
            "wallpaper_astronomy_scene",
            serde_json::json!(global_scene(behavior)),
        );
        set("wallpaper_astronomy_layout", serde_json::json!(layout));
        set("wallpaper_effect", serde_json::json!("astronomy"));
    } else if let Some(reference) = choice.strip_prefix("jwm-wallpaper:share:") {
        let WallpaperTarget::Output(name) = target else {
            return Err("Choose a display before sharing its scene".into());
        };
        if !connectors.iter().any(|connector| connector == reference) {
            return Err(
                "The display to share with is disconnected; no assignments changed.".into(),
            );
        }
        let mut outputs = independent_output_snapshot(behavior, connectors, false);
        join_output_scene(&mut outputs, name, reference, global_scene(behavior));
        set(
            "wallpaper_astronomy_scene",
            serde_json::json!(global_scene(behavior)),
        );
        set("wallpaper_astronomy_outputs", serde_json::json!(outputs));
        set(
            "wallpaper_astronomy_layout",
            serde_json::json!("independent"),
        );
        set("wallpaper_effect", serde_json::json!("astronomy"));
    } else {
        match choice {
            "jwm-wallpaper:detach" => {
                let WallpaperTarget::Output(name) = target else {
                    return Err("Choose a display to detach".into());
                };
                let mut outputs = independent_output_snapshot(behavior, connectors, false);
                if let Some(output) = outputs.get_mut(name) {
                    output.group.clear();
                }
                set("wallpaper_astronomy_outputs", serde_json::json!(outputs));
                set(
                    "wallpaper_astronomy_layout",
                    serde_json::json!("independent"),
                );
            }
            "jwm-wallpaper:presentation" => set(
                "wallpaper_astronomy_presentation",
                serde_json::json!(
                    if behavior.wallpaper_astronomy_presentation == "scientific" {
                        "illustrative"
                    } else {
                        "scientific"
                    }
                ),
            ),
            "jwm-wallpaper:orbit" => set(
                "wallpaper_solar_orbit_mode",
                serde_json::json!(if behavior.wallpaper_solar_orbit_mode == "ephemeris" {
                    "illustrative"
                } else {
                    "ephemeris"
                }),
            ),
            "jwm-wallpaper:time:now" => {
                set("wallpaper_solar_time_scale", serde_json::json!(1.0));
                set("wallpaper_solar_time_offset_days", serde_json::json!(0.0));
            }
            "jwm-wallpaper:time:pause" => set("wallpaper_solar_time_scale", serde_json::json!(0.0)),
            "jwm-wallpaper:time:realtime" => {
                set("wallpaper_solar_time_scale", serde_json::json!(1.0))
            }
            "jwm-wallpaper:time:fast" => {
                set("wallpaper_solar_time_scale", serde_json::json!(86400.0))
            }
            "jwm-wallpaper:time:day-back"
            | "jwm-wallpaper:time:day-forward"
            | "jwm-wallpaper:time:month-back"
            | "jwm-wallpaper:time:month-forward" => {
                let magnitude = if choice.contains("month-") { 30.0 } else { 1.0 };
                let delta = if choice.ends_with("back") {
                    -magnitude
                } else {
                    magnitude
                };
                set(
                    "wallpaper_solar_time_offset_days",
                    serde_json::json!(
                        (behavior.wallpaper_solar_time_offset_days + delta).clamp(-19000.0, 8500.0)
                    ),
                );
            }
            _ => return Err("Unknown celestial picker action".into()),
        }
    }
    if choice.starts_with("jwm-wallpaper:time:") {
        // Date controls act on the view the user can see: leaving the
        // decorative tour active would make Pause appear ineffective.
        set("wallpaper_solar_orbit_mode", serde_json::json!("ephemeris"));
    }
    Ok(changes)
}

/// A fresh effect selection needs a resolved local texture (configured or
/// bundled). Re-selecting an active effect does not discard its loaded image.
pub(crate) fn astronomy_texture_ready(
    already_active: bool,
    texture: &str,
    texture_available: impl FnOnce(&str) -> bool,
) -> bool {
    already_active || texture_available(texture)
}

/// Highlight the manual daily choice even when the first output has an
/// explicit override. Otherwise reflect that output's currently resolved image.
pub(crate) fn picker_current_at(
    behavior: &crate::config::BehaviorConfig,
    active_tags: u32,
    day: i32,
) -> &str {
    use crate::backend::compositor_common::wallpaper::{
        daily_picker_wallpaper_at, resolve_wallpaper_for_tag_at,
    };
    if behavior.wallpaper_effect == "solar-system" {
        return SOLAR_SYSTEM_CHOICE;
    }
    if behavior.wallpaper_effect == "astronomy" {
        return scene_choice(&behavior.wallpaper_astronomy_scene);
    }
    daily_picker_wallpaper_at(behavior, day)
        .unwrap_or_else(|| resolve_wallpaper_for_tag_at(behavior, 0, active_tags, day).0)
}

/// Extensions the compositor's image loader handles.
const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "gif"];

/// Most entries listed; a pictures directory can be enormous and the panel
/// scrolls, but building and ordering rows for tens of thousands of pictures
/// would stall the frame that opened it.
///
/// Selection retains at most twice this many candidates while the worker
/// reads a bounded prefix of the directory. Only entries whose type is unresolved need a
/// stat, see [`select_wallpapers`].
pub const MAX_ENTRIES: usize = 200;

/// Stop pathological directories after a bounded number of raw entries,
/// including sidecars and unreadable entries. Ordering applies to this prefix.
pub const MAX_SCAN_ENTRIES: usize = 100_000;

/// Dropping the owner cancels its worker without joining on filesystem I/O.
#[derive(Debug, Default)]
pub struct ScanCancellation(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl ScanCancellation {
    pub fn observer(&self) -> impl Fn() -> bool + Send + 'static {
        let cancelled = std::sync::Arc::clone(&self.0);
        move || cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for ScanCancellation {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct WallpaperListing {
    pub directory: PathBuf,
    pub paths: Vec<PathBuf>,
    /// Configuration identity at scan start and its original matching row key.
    /// Applied only if the configuration still has that identity at completion.
    pub selection: Option<(String, String)>,
    /// Keep partial and failed directory scans distinguishable from an empty
    /// folder. The event loop formats this without touching the filesystem.
    pub summary: ScanSummary,
}

impl From<(PathBuf, Vec<PathBuf>)> for WallpaperListing {
    fn from((directory, paths): (PathBuf, Vec<PathBuf>)) -> Self {
        let images = paths.len();
        Self {
            directory,
            paths,
            selection: None,
            summary: ScanSummary {
                images,
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, Default)]
pub struct ScanSummary {
    pub images: usize,
    pub visited: usize,
    pub unreadable: usize,
    pub unsupported_names: usize,
    pub error: Option<std::io::ErrorKind>,
}

impl ScanSummary {
    /// A successful small listing needs no warning. Effects are always rows,
    /// so an empty directory must be reported explicitly instead of relying
    /// on the list panel's otherwise-unreachable empty state.
    pub(crate) fn message(&self, directory: &Path, listed_images: usize) -> String {
        let mut notes = Vec::new();
        if let Some(error) = self.error {
            let reason = match error {
                std::io::ErrorKind::NotFound => "folder not found",
                std::io::ErrorKind::PermissionDenied => "permission denied",
                std::io::ErrorKind::NotADirectory => "not a folder",
                _ => "directory unavailable",
            };
            notes.push(format!("Cannot read {}: {reason}", directory.display()));
        } else if listed_images == 0 {
            notes.push(format!("No images in {}", directory.display()));
        }
        if self.visited >= MAX_SCAN_ENTRIES {
            notes.push(format!("Scan limit reached ({MAX_SCAN_ENTRIES} entries)"));
        }
        if self.images > MAX_ENTRIES {
            notes.push(format!(
                "{} images found; search covers {listed_images} listed images",
                self.images
            ));
        }
        if self.unreadable > 0 {
            notes.push(format!("{} unreadable entries skipped", self.unreadable));
        }
        if self.unsupported_names > 0 {
            notes.push(format!(
                "{} unsupported filenames skipped",
                self.unsupported_names
            ));
        }
        notes.join(" · ")
    }
}

/// Resolve aliases off the event loop, keeping the listing's original paths.
/// Retain a valid current picture even if it falls outside the bounded listing.
pub fn resolve_selection(
    paths: &mut Vec<PathBuf>,
    current: &str,
    home: &Path,
    cancelled: impl Fn() -> bool,
) -> Option<(String, String)> {
    if current.is_empty() || is_effect_choice(current) || cancelled() {
        return None;
    }
    let expanded = expand_home(current, home);
    // Prefer the literal row over another symlink to the same image.
    let selected = paths
        .iter()
        .find(|path| **path == expanded)
        .cloned()
        .or_else(|| {
            let canonical = std::fs::canonicalize(&expanded).ok()?;
            paths
                .iter()
                .take_while(|_| !cancelled())
                .find(|path| std::fs::canonicalize(path).ok().as_ref() == Some(&canonical))
                .cloned()
        });
    let selected = match selected {
        Some(path) => path,
        None if !cancelled() && is_image(&expanded) && expanded.is_file() => {
            if paths.len() >= MAX_ENTRIES {
                paths.pop();
            }
            paths.push(expanded.clone());
            paths.sort_unstable_by(|a, b| sort_key(a).cmp(&sort_key(b)).then_with(|| a.cmp(b)));
            expanded
        }
        None => return None,
    };
    Some((current.to_owned(), selected.to_str()?.to_owned()))
}

/// What a directory entry's own type says about it, as the directory listing
/// reports it (`d_type`) rather than as a stat would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory, socket, FIFO or device: never a wallpaper.
    Other,
    /// A symlink, or an entry whose type could not be read: only a stat that
    /// follows the link can tell whether a file is behind it.
    Unresolved,
}

impl EntryKind {
    /// Classify a [`std::fs::DirEntry::file_type`] result. On Linux that
    /// comes from `d_type` for free; `std` only falls back to an `lstat` on
    /// file systems that do not report entry types.
    #[must_use]
    pub fn of(file_type: std::io::Result<std::fs::FileType>) -> Self {
        match file_type {
            Ok(file_type) if file_type.is_file() => Self::File,
            Ok(file_type) if file_type.is_symlink() => Self::Unresolved,
            Ok(_) => Self::Other,
            Err(_) => Self::Unresolved,
        }
    }
}

/// Whether a path looks like an image this compositor can draw.
#[must_use]
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

/// Where to look for wallpapers.
///
/// An explicit `wallpaper_dir` wins. Otherwise the directory holding the
/// current wallpaper is the best guess — whoever set one almost certainly
/// keeps the rest beside it — and failing that the usual pictures locations.
#[must_use]
pub fn resolve_directory(configured_dir: &str, current_wallpaper: &str, home: &Path) -> PathBuf {
    let configured = configured_dir.trim();
    if !configured.is_empty() {
        return expand_home(configured, home);
    }
    let current = current_wallpaper.trim();
    if !current.is_empty()
        && let Some(parent) = expand_home(current, home).parent()
        && parent.is_dir()
    {
        return parent.to_path_buf();
    }
    let wallpapers = home.join("Pictures").join("Wallpapers");
    if wallpapers.is_dir() {
        return wallpapers;
    }
    home.join("Pictures")
}

/// User-local bundled wallpapers are not hidden behind an application-specific
/// prefix: the source installer copies them into this flat XDG directory.
#[must_use]
pub fn bundled_wallpaper_directory(
    home: &Path,
    xdg_config_home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    xdg_config_home
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .map_or_else(|| home.join(".config"), Path::to_path_buf)
        .join("jwm/wallpapers")
}

/// Prefer the bundled collection on a fresh install, but never override an
/// explicitly configured folder or a valid current picture's directory.
/// The background scanner also appends bundled entries if another folder wins.
#[must_use]
pub fn resolve_picker_directory(
    configured_dir: &str,
    current_wallpaper: &str,
    home: &Path,
    bundled: &Path,
) -> PathBuf {
    if !configured_dir.trim().is_empty() {
        return resolve_directory(configured_dir, current_wallpaper, home);
    }
    let current = current_wallpaper.trim();
    if !current.is_empty()
        && let Some(parent) = expand_home(current, home).parent()
        && parent.is_dir()
    {
        return parent.to_path_buf();
    }
    if bundled.is_dir() {
        return bundled.to_path_buf();
    }
    resolve_directory(configured_dir, current_wallpaper, home)
}

/// Expand a leading `~` against `home`; other paths are taken as given.
#[must_use]
pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

/// Image files in `dir`, sorted by name, capped at [`MAX_ENTRIES`].
///
/// Sorting is case-insensitive so `Alps.jpg` and `alps.jpg` do not end up at
/// opposite ends of the list.
#[cfg(test)]
#[must_use]
pub fn list_wallpapers(dir: &Path) -> Vec<PathBuf> {
    list_wallpapers_cancellable(dir, || false)
}

/// Cancellation is checked before every raw directory entry, not just images.
/// A blocked filesystem syscall itself cannot be interrupted by this flag.
#[must_use]
pub fn list_wallpapers_cancellable(dir: &Path, cancelled: impl Fn() -> bool) -> Vec<PathBuf> {
    scan_wallpapers_cancellable(dir, cancelled).0
}

/// Bounded scan with enough metadata to explain an incomplete picker.
#[must_use]
pub fn scan_wallpapers_cancellable(
    dir: &Path,
    cancelled: impl Fn() -> bool,
) -> (Vec<PathBuf>, ScanSummary) {
    let mut summary = ScanSummary::default();
    if cancelled() {
        return (Vec::new(), summary);
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            summary.error = Some(error.kind());
            return (Vec::new(), summary);
        }
    };
    // Bound/check before filtering: millions of sidecars must remain cancellable.
    let candidates = bounded_scan(entries, &cancelled)
        .inspect(|_| summary.visited += 1)
        .filter_map(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    summary.unreadable += 1;
                    return None;
                }
            };
            let path = entry.path();
            if !is_image(&path) {
                return None;
            }
            // Config and IPC paths are UTF-8 strings. A lossy replacement can
            // name another image, so never put an unrepresentable key in a row.
            if path.to_str().is_none() {
                summary.unsupported_names += 1;
                return None;
            }
            Some((path, EntryKind::of(entry.file_type())))
        });
    let (paths, images) = select_wallpapers_counted(candidates, Path::is_file);
    summary.images = images;
    (paths, summary)
}

/// Read the user's preferred directory and the installed JWM collection on
/// the same background worker. Bundled images reserve bounded slots even when
/// the user's folder has hundreds of images, so they remain picker-visible.
/// The original directory wins for aliases to the same on-disk picture.
#[must_use]
pub fn scan_wallpapers_with_bundled(
    primary: &Path,
    bundled: &Path,
    cancelled: impl Fn() -> bool,
) -> (Vec<PathBuf>, ScanSummary) {
    let (mut paths, mut summary) = scan_wallpapers_cancellable(primary, &cancelled);
    if cancelled() || !bundled.is_dir() || primary == bundled {
        return (paths, summary);
    }
    if let (Ok(first), Ok(second)) = (
        std::fs::canonicalize(primary),
        std::fs::canonicalize(bundled),
    ) && first == second
    {
        return (paths, summary);
    }
    let (mut extras, additional) = scan_wallpapers_cancellable(bundled, &cancelled);
    if cancelled() || additional.error.is_some() {
        return (paths, summary);
    }
    // Canonicalization is bounded to <=400 retained candidates and never runs
    // on the compositor event loop. It avoids duplicate rows for symlinked
    // collections without following symlinked directories recursively.
    let mut seen: std::collections::HashSet<PathBuf> = paths
        .iter()
        .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect();
    extras.retain(|path| {
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        seen.insert(target)
    });
    paths.truncate(MAX_ENTRIES.saturating_sub(extras.len()));
    paths.extend(extras);
    paths.sort_unstable_by(|a, b| sort_key(a).cmp(&sort_key(b)).then_with(|| a.cmp(b)));
    summary.images = summary.images.saturating_add(additional.images);
    summary.visited = summary.visited.saturating_add(additional.visited);
    summary.unreadable = summary.unreadable.saturating_add(additional.unreadable);
    summary.unsupported_names = summary
        .unsupported_names
        .saturating_add(additional.unsupported_names);
    (paths, summary)
}

fn bounded_scan<T>(
    entries: impl IntoIterator<Item = T>,
    cancelled: impl Fn() -> bool,
) -> impl Iterator<Item = T> {
    let mut entries = entries.into_iter();
    (0..MAX_SCAN_ENTRIES).map_while(move |_| if cancelled() { None } else { entries.next() })
}

/// The wallpapers among `entries`, sorted by name, capped at [`MAX_ENTRIES`].
///
/// The extension is checked first because it costs nothing. `is_file` — a
/// stat that follows symlinks — then runs only for images whose entry type
/// did not already settle the question, so a directory of forty thousand
/// photos is not forty thousand stats on the thread that opened the picker.
///
/// Each sort key is built once rather than per comparison. Batches retain
/// only the best [`MAX_ENTRIES`] names, bounding candidate storage even for
/// an enormous directory; only the final selection is fully ordered.
/// Equal keys (`Alps.jpg`
/// beside `alps.jpg`) fall back to the path, so the order never depends on
/// the order the directory happened to list them in.
#[must_use]
pub fn select_wallpapers(
    entries: impl IntoIterator<Item = (PathBuf, EntryKind)>,
    is_file: impl FnMut(&Path) -> bool,
) -> Vec<PathBuf> {
    select_wallpapers_counted(entries, is_file).0
}

fn select_wallpapers_counted(
    entries: impl IntoIterator<Item = (PathBuf, EntryKind)>,
    mut is_file: impl FnMut(&Path) -> bool,
) -> (Vec<PathBuf>, usize) {
    let candidates = entries
        .into_iter()
        .filter(|(path, kind)| {
            is_image(path)
                && path.to_str().is_some()
                && match kind {
                    EntryKind::File => true,
                    EntryKind::Other => false,
                    EntryKind::Unresolved => is_file(path),
                }
        })
        .map(|(path, _)| (sort_key(&path), path));
    let mut files = Vec::with_capacity(MAX_ENTRIES * 2);
    let mut images = 0;
    for candidate in candidates {
        images += 1;
        files.push(candidate);
        if files.len() == MAX_ENTRIES * 2 {
            // A discarded name already has MAX_ENTRIES better names, so
            // later entries can never bring it back into the final result.
            // Partition in batches to keep both storage and total ordering
            // work bounded without sorting every candidate on insertion.
            files.select_nth_unstable(MAX_ENTRIES);
            files.truncate(MAX_ENTRIES);
        }
    }
    if files.len() > MAX_ENTRIES {
        // Partition around the cap in linear time; only what survives the
        // cap is worth a full sort.
        files.select_nth_unstable(MAX_ENTRIES);
        files.truncate(MAX_ENTRIES);
    }
    files.sort_unstable();
    (files.into_iter().map(|(_, path)| path).collect(), images)
}

/// Search both the filename/path and the human-readable effect name. Tokens
/// are normalized once per query; a multiword search can match in any order.
pub(crate) fn matches_query(key: &str, label: &str, words: &[&str]) -> bool {
    if words.is_empty() {
        return true;
    }
    let key = key.to_lowercase();
    let label = label.to_lowercase();
    words
        .iter()
        .all(|word| key.contains(word) || label.contains(word))
}

/// Case-insensitive name a wallpaper is ordered by.
fn sort_key(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

/// One picker row: the file name, marked when it is the current wallpaper.
#[must_use]
pub fn picker_row(path: &Path, current: &str) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("(unnamed)");
    let marker = if path.to_str() == Some(current) {
        "\u{f00c}" // fa-check
    } else {
        " "
    };
    format!("\u{f03e} {marker} {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn celestial_facts_fit_picker_rows_without_losing_source_urls() {
        use crate::backend::compositor_common::celestial_catalog::body_by_id;
        for (scene, _) in crate::config::astronomy::ASTRONOMY_SCENES {
            let lines = scene_information(scene);
            assert!(!lines.is_empty(), "missing facts for {scene}");
            assert!(
                lines.iter().all(|line| line.chars().count() <= 60),
                "facts would clip in the native picker for {scene}"
            );
            if let Some(body) = body_by_id(scene) {
                let content = lines.concat();
                for source in [
                    Some(body.source_url),
                    Some(body.physical_source_url),
                    body.texture_source_url,
                ]
                .into_iter()
                .flatten()
                {
                    assert!(content.contains(source), "missing {scene} source {source}");
                }
            }
        }
    }

    #[test]
    fn celestial_global_selection_migrates_legacy_layouts_to_mirror() {
        for layout in ["per-output", "earth-moon"] {
            let mut config = crate::config::Config::default();
            config
                .set_values(&[(
                    "behavior.wallpaper_astronomy_layout".into(),
                    serde_json::json!(layout),
                )])
                .unwrap();
            let changes = astronomy_changes(
                config.behavior(),
                &WallpaperTarget::All,
                &["DP-1".into(), "HDMI-1".into()],
                "jwm-effect:mars",
            )
            .unwrap();
            config.set_values(&changes).unwrap();
            assert_eq!(config.behavior().wallpaper_astronomy_layout, "mirror");
            assert_eq!(config.behavior().wallpaper_astronomy_scene, "mars");
            assert_eq!(config.behavior().wallpaper_effect, "astronomy");
        }
    }

    #[test]
    fn celestial_solar_selection_keeps_the_selected_global_layout() {
        for layout in ["span", "mirror", "independent"] {
            let mut config = crate::config::Config::default();
            config
                .set_values(&[(
                    "behavior.wallpaper_astronomy_layout".into(),
                    serde_json::json!(layout),
                )])
                .unwrap();
            let changes = astronomy_changes(
                config.behavior(),
                &WallpaperTarget::All,
                &["DP-1".into(), "HDMI-1".into()],
                SOLAR_SYSTEM_CHOICE,
            )
            .unwrap();
            config.set_values(&changes).unwrap();
            assert_eq!(config.behavior().wallpaper_astronomy_layout, layout);
            assert_eq!(config.behavior().wallpaper_astronomy_scene, "solar-system");
            assert_eq!(config.behavior().wallpaper_effect, "astronomy");
        }
    }

    #[test]
    fn celestial_pause_switches_visible_orbits_from_illustrative_to_ephemeris() {
        let mut config = crate::config::Config::default();
        config
            .set_values(&[
                (
                    "behavior.wallpaper_solar_orbit_mode".into(),
                    serde_json::json!("illustrative"),
                ),
                (
                    "behavior.wallpaper_solar_time_scale".into(),
                    serde_json::json!(86400.0),
                ),
            ])
            .unwrap();
        let changes = astronomy_changes(
            config.behavior(),
            &WallpaperTarget::All,
            &[],
            "jwm-wallpaper:time:pause",
        )
        .unwrap();
        config.set_values(&changes).unwrap();
        assert_eq!(config.behavior().wallpaper_solar_orbit_mode, "ephemeris");
        assert_eq!(config.behavior().wallpaper_solar_time_scale, 0.0);
    }

    #[test]
    fn celestial_one_display_edit_keeps_the_other_solar_scene() {
        let mut config = crate::config::Config::default();
        config
            .set_values(&[(
                "behavior.wallpaper_effect".into(),
                serde_json::json!("solar-system"),
            )])
            .unwrap();
        let connectors = vec!["DP-1".into(), "HDMI-1".into()];
        let changes = astronomy_changes(
            config.behavior(),
            &WallpaperTarget::Output("HDMI-1".into()),
            &connectors,
            "jwm-effect:saturn",
        )
        .unwrap();
        config.set_values(&changes).unwrap();
        assert_eq!(config.behavior().wallpaper_effect, "astronomy");
        assert_eq!(config.behavior().wallpaper_astronomy_layout, "independent");
        assert_eq!(config.behavior().wallpaper_astronomy_scene, "solar-system");
        assert_eq!(
            config.behavior().wallpaper_astronomy_outputs["DP-1"].scene,
            "solar-system"
        );
        assert_eq!(
            config.behavior().wallpaper_astronomy_outputs["HDMI-1"].scene,
            "saturn"
        );
    }

    #[test]
    fn celestial_legacy_earth_moon_migration_keeps_every_other_display() {
        let mut config = crate::config::Config::default();
        config
            .set_values(&[
                (
                    "behavior.wallpaper_effect".into(),
                    serde_json::json!("astronomy"),
                ),
                (
                    "behavior.wallpaper_astronomy_layout".into(),
                    serde_json::json!("earth-moon"),
                ),
                (
                    "behavior.wallpaper_astronomy_bodies".into(),
                    serde_json::json!({"DP-2":"earth"}),
                ),
            ])
            .unwrap();
        let connectors = vec!["HDMI-1".into(), "DP-2".into(), "DP-1".into()];
        let changes = astronomy_changes(
            config.behavior(),
            &WallpaperTarget::Output("HDMI-1".into()),
            &connectors,
            "jwm-effect:galaxy",
        )
        .unwrap();
        config.set_values(&changes).unwrap();
        assert_eq!(
            config.behavior().wallpaper_astronomy_outputs["DP-1"].scene,
            "earth"
        );
        assert_eq!(
            config.behavior().wallpaper_astronomy_outputs["DP-2"].scene,
            "earth"
        );
        assert_eq!(
            config.behavior().wallpaper_astronomy_outputs["HDMI-1"].scene,
            "galaxy"
        );
        assert_eq!(
            config.behavior().wallpaper_astronomy_bodies["DP-2"],
            "earth"
        );
    }

    #[test]
    fn celestial_share_detach_and_hot_unplug_do_not_retarget_a_screen() {
        let mut config = crate::config::Config::default();
        let connectors = vec!["DP-1".into(), "DP-2".into(), "HDMI-1".into()];
        let target = WallpaperTarget::Output("DP-2".into());
        let changes = astronomy_changes(
            config.behavior(),
            &target,
            &connectors,
            "jwm-wallpaper:share:DP-1",
        )
        .unwrap();
        config.set_values(&changes).unwrap();
        assert!(
            !config.behavior().wallpaper_astronomy_outputs["DP-1"]
                .group
                .is_empty()
        );
        assert_eq!(
            config.behavior().wallpaper_astronomy_outputs["DP-1"],
            config.behavior().wallpaper_astronomy_outputs["DP-2"]
        );
        let changes = astronomy_changes(
            config.behavior(),
            &target,
            &connectors,
            "jwm-effect:jupiter",
        )
        .unwrap();
        config.set_values(&changes).unwrap();
        assert_eq!(
            config.behavior().wallpaper_astronomy_outputs["DP-1"].scene,
            "jupiter"
        );
        assert_eq!(
            config.behavior().wallpaper_astronomy_outputs["HDMI-1"].scene,
            "earth"
        );
        let changes = astronomy_changes(
            config.behavior(),
            &target,
            &connectors,
            "jwm-wallpaper:detach",
        )
        .unwrap();
        config.set_values(&changes).unwrap();
        assert!(
            config.behavior().wallpaper_astronomy_outputs["DP-2"]
                .group
                .is_empty()
        );
        assert!(
            astronomy_changes(
                config.behavior(),
                &target,
                &["HDMI-1".into()],
                "jwm-effect:mars"
            )
            .is_err()
        );
        assert_eq!(target.next(&["HDMI-1".into()]), WallpaperTarget::All);
    }

    #[test]
    fn celestial_solar_clock_controls_leave_physical_earth_moon_time_settings_alone() {
        let mut config = crate::config::Config::default();
        let original = config.behavior().clone();
        for action in ["fast", "day-forward", "month-back", "pause", "now"] {
            let changes = astronomy_changes(
                config.behavior(),
                &WallpaperTarget::All,
                &[],
                &format!("jwm-wallpaper:time:{action}"),
            )
            .unwrap();
            config.set_values(&changes).unwrap();
            assert_eq!(
                config.behavior().wallpaper_earth_rotation_minutes,
                original.wallpaper_earth_rotation_minutes
            );
            assert_eq!(
                config.behavior().wallpaper_moon_rotation_minutes,
                original.wallpaper_moon_rotation_minutes
            );
            assert_eq!(
                config.behavior().wallpaper_timezone_offset_hours,
                original.wallpaper_timezone_offset_hours
            );
            assert_eq!(
                config.behavior().wallpaper_solar_spin_speed,
                original.wallpaper_solar_spin_speed
            );
        }
        assert_eq!(config.behavior().wallpaper_solar_time_scale, 1.0);
        assert_eq!(config.behavior().wallpaper_solar_time_offset_days, 0.0);
    }

    #[test]
    fn search_matches_unicode_paths_and_all_query_words() {
        assert!(matches_query(
            "/walls/Été Alps.png",
            "Été Alps.png",
            &["été", "alps"]
        ));
        assert!(matches_query(
            SOLAR_SYSTEM_CHOICE,
            "Solar System 3D · drag to orbit",
            &["orbit", "solar"]
        ));
        assert!(!matches_query(
            "/walls/Alps.png",
            "Alps.png",
            &["alps", "beach"]
        ));
        assert!(matches_query("/walls/a.png", "a.png", &[]));
    }

    #[test]
    fn scan_summary_distinguishes_unavailable_empty_and_capped_folders() {
        let dir = temp_dir("scan-summary");
        let (empty, summary) = scan_wallpapers_cancellable(&dir, || false);
        assert!(empty.is_empty());
        assert!(summary.error.is_none());
        assert!(
            summary
                .message(&dir, empty.len())
                .starts_with("No images in ")
        );
        let (missing, summary) = scan_wallpapers_cancellable(&dir.join("absent"), || false);
        assert!(missing.is_empty());
        assert_eq!(summary.error, Some(std::io::ErrorKind::NotFound));
        assert!(summary.message(&dir, 0).contains("folder not found"));

        for index in 0..MAX_ENTRIES + 3 {
            std::fs::write(dir.join(format!("{index:04}.png")), b"x").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(dir.join("folder.png")).unwrap();
        let (paths, summary) = scan_wallpapers_cancellable(&dir, || false);
        assert_eq!(paths.len(), MAX_ENTRIES);
        assert_eq!(summary.images, MAX_ENTRIES + 3);
        assert_eq!(summary.visited, MAX_ENTRIES + 5);
        let message = summary.message(&dir, paths.len());
        assert!(message.contains("203 images found"));
        assert!(message.contains("search covers 200 listed images"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_never_turns_non_utf8_filenames_into_the_wrong_image_path() {
        use std::os::unix::ffi::OsStringExt as _;
        let dir = temp_dir("scan-invalid-name");
        let invalid = dir.join(std::ffi::OsString::from_vec(b"bad\xff.png".to_vec()));
        let valid = dir.join("bad\u{fffd}.png");
        std::fs::write(&invalid, b"invalid name").unwrap();
        std::fs::write(&valid, b"valid name").unwrap();
        let (paths, summary) = scan_wallpapers_cancellable(&dir, || false);
        assert_eq!(paths, [valid]);
        assert_eq!(summary.images, 1);
        assert_eq!(summary.unsupported_names, 1);
        assert!(
            summary
                .message(&dir, 1)
                .contains("unsupported filenames skipped")
        );
        assert!(select_wallpapers([(invalid, EntryKind::File)], |_| true).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_scan_status_explains_both_limits_and_read_errors() {
        let summary = ScanSummary {
            images: MAX_ENTRIES + 1,
            visited: MAX_SCAN_ENTRIES,
            unreadable: 2,
            ..Default::default()
        };
        let message = summary.message(Path::new("/walls"), MAX_ENTRIES);
        assert!(message.contains("Scan limit reached (100000 entries)"));
        assert!(message.contains("201 images found"));
        assert!(message.contains("search covers 200 listed images"));
        assert!(message.contains("2 unreadable entries skipped"));
    }

    #[test]
    fn cancelled_scan_does_not_consume_even_one_raw_entry() {
        let cancellation = ScanCancellation::default();
        let cancelled = cancellation.observer();
        drop(cancellation);
        let entries =
            std::iter::from_fn(|| -> Option<()> { panic!("cancelled scan read an entry") });
        assert_eq!(bounded_scan(entries, cancelled).count(), 0);
    }

    #[test]
    fn scan_budget_counts_non_images_and_checks_cancellation_between_entries() {
        let visited = std::cell::Cell::new(0);
        let entries = std::iter::repeat_with(|| {
            visited.set(visited.get() + 1);
            "sidecar.txt"
        });
        assert_eq!(bounded_scan(entries, || false).count(), MAX_SCAN_ENTRIES);
        assert_eq!(visited.get(), MAX_SCAN_ENTRIES);
        visited.set(0);
        let entries = std::iter::repeat_with(|| {
            visited.set(visited.get() + 1);
            "sidecar.txt"
        });
        assert_eq!(bounded_scan(entries, || visited.get() >= 7).count(), 7);
        assert_eq!(visited.get(), 7);
    }

    #[test]
    fn selection_resolves_home_and_symlinks_without_rewriting_rows() {
        let dir = temp_dir("selection-alias");
        let real = dir.join("z-real.png");
        let alias = dir.join("a-alias.png");
        std::fs::write(&real, b"x").unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let mut paths = vec![alias.clone(), real.clone()];
        let original = paths.clone();
        let selected = resolve_selection(&mut paths, "~/z-real.png", &dir, || false).unwrap();
        assert_eq!(selected.1, real.to_str().unwrap());
        assert_eq!(paths, original);
        paths.pop();
        let selected =
            resolve_selection(&mut paths, real.to_str().unwrap(), &dir, || false).unwrap();
        assert_eq!(selected.1, alias.to_str().unwrap());
        assert_eq!(paths, [alias]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn current_picture_survives_the_row_cap_without_growing_it() {
        let dir = temp_dir("selection-cap");
        let current = dir.join("z-current.png");
        std::fs::write(&current, b"x").unwrap();
        let mut paths: Vec<_> = (0..MAX_ENTRIES)
            .map(|i| dir.join(format!("{i:04}.png")))
            .collect();
        let selected =
            resolve_selection(&mut paths, current.to_str().unwrap(), &dir, || false).unwrap();
        assert_eq!(paths.len(), MAX_ENTRIES);
        assert_eq!(selected.1, current.to_str().unwrap());
        assert!(paths.contains(&current));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selection_does_not_add_missing_effect_or_cancelled_paths() {
        let dir = temp_dir("selection-missing");
        let current = dir.join("a.png");
        let mut paths = Vec::new();
        assert!(resolve_selection(&mut paths, current.to_str().unwrap(), &dir, || false).is_none());
        std::fs::write(&current, b"x").unwrap();
        assert!(resolve_selection(&mut paths, current.to_str().unwrap(), &dir, || true).is_none());
        assert!(resolve_selection(&mut paths, ASTRONOMY_CHOICE, &dir, || false).is_none());
        assert!(paths.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn solar_system_choice_is_selected_and_never_an_image() {
        use crate::jwm::features::SystemUiState;
        let config = crate::config::Config::default()
            .with_validated_value(
                "behavior.wallpaper_effect",
                &serde_json::json!("solar-system"),
            )
            .unwrap();
        assert_eq!(
            picker_current_at(config.behavior(), 1, 0),
            SOLAR_SYSTEM_CHOICE
        );
        let panel = SystemUiState::wallpaper_picker(&[], SOLAR_SYSTEM_CHOICE, "/walls");
        assert_eq!(panel.selected_wallpaper_choice(), Some(SOLAR_SYSTEM_CHOICE));
        assert_eq!(panel.selected_wallpaper(), None);
        assert!(is_effect_choice(ASTRONOMY_CHOICE));
        assert!(is_effect_choice(SOLAR_SYSTEM_CHOICE));
        assert!(!is_effect_choice("/walls/solar-system.jpg"));
    }

    #[test]
    fn astronomy_checks_resolved_texture_only_for_new_selection() {
        assert!(astronomy_texture_ready(false, "", |_| true));
        assert!(!astronomy_texture_ready(false, "", |_| false));
        assert!(!astronomy_texture_ready(false, "/missing.jpg", |_| false));
        assert!(astronomy_texture_ready(false, "/earth.jpg", |path| path == "/earth.jpg"));
        assert!(astronomy_texture_ready(true, "", |_| panic!(
            "active effect must be preserved"
        )));
    }

    #[test]
    fn astronomy_is_selected_without_becoming_an_image_preview_or_path() {
        let mut config = crate::config::Config::default();
        config
            .set_value("behavior.wallpaper_effect", &serde_json::json!("astronomy"))
            .unwrap();
        assert_eq!(
            picker_current_at(config.behavior(), 1, 100),
            ASTRONOMY_CHOICE
        );
        let paths = [PathBuf::from("/walls/static.png")];
        let mut panel = crate::jwm::features::SystemUiState::wallpaper_picker(
            &paths,
            ASTRONOMY_CHOICE,
            "/walls",
        );
        assert_eq!(panel.selected_wallpaper_choice(), Some(ASTRONOMY_CHOICE));
        assert_eq!(panel.selected_wallpaper(), None);
        panel.move_selection(-1);
        assert_eq!(panel.selected_wallpaper(), Some("/walls/static.png"));
        let picked = config
            .with_picker_wallpaper(panel.selected_wallpaper().unwrap(), 100)
            .unwrap();
        assert_eq!(picked.behavior().wallpaper_effect, "off");
        assert_eq!(picked.behavior().wallpaper, "/walls/static.png");
        let empty = crate::jwm::features::SystemUiState::wallpaper_picker(&[], "", "/missing");
        assert_eq!(empty.selected_wallpaper_choice(), Some(ASTRONOMY_CHOICE));
        assert_eq!(empty.selected_wallpaper(), None);
    }

    #[test]
    fn wallpaper_picker_reopens_on_todays_effective_choice_without_stopping_rotation() {
        let mut config = crate::config::Config::default();
        config
            .set_values(&[
                (
                    "behavior.wallpaper_rotation".into(),
                    serde_json::json!("daily"),
                ),
                (
                    "behavior.wallpaper_rotation_paths".into(),
                    serde_json::json!(["/walls/a.png", "/walls/b.png"]),
                ),
            ])
            .unwrap();
        assert_eq!(picker_current_at(config.behavior(), 1, 100), "/walls/a.png");
        let paths = [
            PathBuf::from("/walls/a.png"),
            PathBuf::from("/walls/picked.png"),
        ];
        let mut panel = crate::jwm::features::SystemUiState::wallpaper_picker(
            &paths,
            picker_current_at(config.behavior(), 1, 100),
            "/walls",
        );
        panel.move_selection(1);
        // Preview/navigation never touches the config; dropping this panel is
        // the model-level cancel operation.
        assert_eq!(picker_current_at(config.behavior(), 1, 100), "/walls/a.png");
        let picked = config
            .with_picker_wallpaper(panel.selected_wallpaper().unwrap(), 100)
            .unwrap();
        drop(panel);
        let reopened = crate::jwm::features::SystemUiState::wallpaper_picker(
            &paths,
            picker_current_at(picked.behavior(), 1, 100),
            "/walls",
        );
        assert_eq!(reopened.selected_wallpaper(), Some("/walls/picked.png"));
        assert_eq!(picker_current_at(picked.behavior(), 1, 101), "/walls/b.png");
        assert_eq!(picked.behavior().wallpaper_rotation, "daily");
        let source = include_str!("toggles.rs");
        assert_eq!(source.matches("with_picker_wallpaper(").count(), 1);
        assert!(source.contains("\"config/wallpaper\""));
        assert!(
            crate::ipc::IPC_REGISTRY
                .subscription_topics
                .contains(&"config")
        );
        assert_eq!(source.matches("self.current_picker_wallpaper()").count(), 2);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jwm-wallpaper-{}-{tag}", std::process::id()));
        // Whatever an interrupted earlier run left behind would turn the
        // symlink fixtures into "already exists" failures.
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn image_extensions_are_recognized_case_insensitively() {
        assert!(is_image(Path::new("a.png")));
        assert!(is_image(Path::new("a.JPG")));
        assert!(is_image(Path::new("a.jpeg")));
        assert!(is_image(Path::new("a.WebP")));
        assert!(!is_image(Path::new("a.txt")));
        assert!(!is_image(Path::new("a")));
    }

    #[test]
    fn a_configured_directory_wins() {
        let home = Path::new("/home/ada");
        assert_eq!(
            resolve_directory("/srv/walls", "/home/ada/Pictures/x.png", home),
            PathBuf::from("/srv/walls")
        );
    }

    #[test]
    fn a_tilde_is_expanded_against_home() {
        let home = Path::new("/home/ada");
        assert_eq!(
            resolve_directory("~/walls", "", home),
            PathBuf::from("/home/ada/walls")
        );
        assert_eq!(expand_home("~", home), PathBuf::from("/home/ada"));
        assert_eq!(expand_home("/abs", home), PathBuf::from("/abs"));
    }

    #[test]
    fn without_configuration_the_current_wallpapers_directory_is_used() {
        let dir = temp_dir("current");
        let wallpaper = dir.join("now.png");
        std::fs::write(&wallpaper, b"x").expect("write");

        let resolved = resolve_directory("", wallpaper.to_str().unwrap(), Path::new("/home/ada"));
        assert_eq!(resolved, dir);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bundled_directory_honors_only_absolute_xdg_config_home() {
        let home = Path::new("/home/ada");
        assert_eq!(
            bundled_wallpaper_directory(home, None),
            PathBuf::from("/home/ada/.config/jwm/wallpapers")
        );
        assert_eq!(
            bundled_wallpaper_directory(home, Some(std::ffi::OsStr::new("/tmp/xdg config"))),
            PathBuf::from("/tmp/xdg config/jwm/wallpapers")
        );
        assert_eq!(
            bundled_wallpaper_directory(home, Some(std::ffi::OsStr::new("relative/xdg"))),
            PathBuf::from("/home/ada/.config/jwm/wallpapers")
        );
    }

    #[test]
    fn a_fresh_picker_prefers_bundled_but_keeps_explicit_and_current_directories() {
        let home = temp_dir("preferred-home");
        let bundled = home.join(".config/jwm/wallpapers");
        std::fs::create_dir_all(&bundled).unwrap();
        assert_eq!(resolve_picker_directory("", "", &home, &bundled), bundled);
        assert_eq!(
            resolve_picker_directory("~/custom", "", &home, &bundled),
            home.join("custom")
        );
        let current = home.join("Pictures/other.png");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, b"x").unwrap();
        assert_eq!(
            resolve_picker_directory("", current.to_str().unwrap(), &home, &bundled),
            home.join("Pictures")
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn bundled_images_are_always_visible_alongside_a_capped_custom_directory() {
        let primary = temp_dir("large-custom");
        let bundled = temp_dir("bundled");
        for index in 0..(MAX_ENTRIES + 5) {
            std::fs::write(primary.join(format!("{index:04}.png")), b"x").unwrap();
        }
        for file in ["01-bundled.png", "02-bundled.jpg"] {
            std::fs::write(bundled.join(file), b"x").unwrap();
        }
        let (paths, summary) = scan_wallpapers_with_bundled(&primary, &bundled, || false);
        assert_eq!(paths.len(), MAX_ENTRIES);
        assert_eq!(summary.images, MAX_ENTRIES + 7);
        assert!(paths.contains(&bundled.join("01-bundled.png")));
        assert!(paths.contains(&bundled.join("02-bundled.jpg")));
        let (same, summary) = scan_wallpapers_with_bundled(&bundled, &bundled, || false);
        assert_eq!(same.len(), 2);
        assert_eq!(summary.images, 2);
        std::fs::remove_dir_all(primary).unwrap();
        std::fs::remove_dir_all(bundled).unwrap();
    }

    #[test]
    fn bundled_symlink_aliases_do_not_create_duplicate_rows() {
        let primary = temp_dir("custom-alias");
        let bundled = temp_dir("bundled-alias");
        let picture = bundled.join("image.png");
        std::fs::write(&picture, b"x").unwrap();
        std::os::unix::fs::symlink(&picture, primary.join("alias.png")).unwrap();
        let (paths, _) = scan_wallpapers_with_bundled(&primary, &bundled, || false);
        assert_eq!(paths, [primary.join("alias.png")]);
        std::fs::remove_dir_all(primary).unwrap();
        std::fs::remove_dir_all(bundled).unwrap();
    }

    #[test]
    fn listing_keeps_only_images_sorted_by_name() {
        let dir = temp_dir("list");
        for name in ["b.png", "A.jpg", "notes.txt", "c.webp"] {
            std::fs::write(dir.join(name), b"x").expect("write");
        }
        std::fs::create_dir_all(dir.join("subdir.png")).expect("dir");

        let names: Vec<String> = list_wallpapers(&dir)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        // Case-insensitive order, no text file, no directory.
        assert_eq!(names, ["A.jpg", "b.png", "c.webp"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn listing_follows_symlinks_to_decide_what_is_a_file() {
        let dir = temp_dir("symlinks");
        std::fs::write(dir.join("real.png"), b"x").expect("write");
        std::fs::create_dir_all(dir.join("folder")).expect("dir");
        std::os::unix::fs::symlink(dir.join("real.png"), dir.join("linked.jpg")).expect("link");
        std::os::unix::fs::symlink(dir.join("folder"), dir.join("folder.png")).expect("link");
        std::os::unix::fs::symlink(dir.join("missing.png"), dir.join("dangling.png"))
            .expect("link");

        let names: Vec<String> = list_wallpapers(&dir)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        // A link to a picture is a wallpaper; a link to a folder or to
        // nothing is not, whatever its name says.
        assert_eq!(names, ["linked.jpg", "real.png"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_images_the_entry_type_cannot_settle_are_stat_ed() {
        // Regression: the listing used to stat every entry, images or not,
        // before looking at the extension.
        let entries = vec![
            (PathBuf::from("/w/a.png"), EntryKind::File),
            (PathBuf::from("/w/notes.txt"), EntryKind::Unresolved),
            (PathBuf::from("/w/readme.txt"), EntryKind::File),
            (PathBuf::from("/w/folder.png"), EntryKind::Other),
            (PathBuf::from("/w/linked.jpg"), EntryKind::Unresolved),
            (PathBuf::from("/w/dangling.jpg"), EntryKind::Unresolved),
        ];
        let mut stats = Vec::new();
        let listed = select_wallpapers(entries, |path| {
            stats.push(path.to_path_buf());
            path.ends_with("linked.jpg")
        });

        assert_eq!(
            listed,
            [PathBuf::from("/w/a.png"), PathBuf::from("/w/linked.jpg")]
        );
        assert_eq!(
            stats,
            [
                PathBuf::from("/w/linked.jpg"),
                PathBuf::from("/w/dangling.jpg")
            ]
        );
    }

    #[test]
    fn the_cap_keeps_the_first_names_in_order() {
        let total = MAX_ENTRIES + 50;
        // Listed backwards, the way a directory may well hand them out.
        let entries: Vec<(PathBuf, EntryKind)> = (0..total)
            .rev()
            .map(|index| (PathBuf::from(format!("/w/{index:04}.png")), EntryKind::File))
            .collect();

        let listed = select_wallpapers(entries, |path| {
            unreachable!("{} has a known type", path.display())
        });

        let expected: Vec<PathBuf> = (0..MAX_ENTRIES)
            .map(|index| PathBuf::from(format!("/w/{index:04}.png")))
            .collect();
        assert_eq!(listed, expected);
    }

    #[test]
    fn names_that_differ_only_in_case_have_a_fixed_order() {
        for entries in [
            ["/w/b.png", "/w/alps.jpg", "/w/Alps.jpg"],
            ["/w/Alps.jpg", "/w/b.png", "/w/alps.jpg"],
        ] {
            let listed = select_wallpapers(
                entries.map(|path| (PathBuf::from(path), EntryKind::File)),
                |_| false,
            );
            assert_eq!(
                listed,
                [
                    PathBuf::from("/w/Alps.jpg"),
                    PathBuf::from("/w/alps.jpg"),
                    PathBuf::from("/w/b.png")
                ]
            );
        }
    }

    #[test]
    fn batched_selection_matches_global_order_across_batch_boundaries() {
        for total in [
            MAX_ENTRIES - 1,
            MAX_ENTRIES,
            MAX_ENTRIES * 2 - 1,
            MAX_ENTRIES * 2,
            MAX_ENTRIES * 2 + 1,
            MAX_ENTRIES * 13 + 57,
        ] {
            let paths: Vec<PathBuf> = (0..total)
                .map(|index| {
                    let name = if index % 2 == 0 { "Alps" } else { "alps" };
                    PathBuf::from(format!("/w/{index:05}/{name}{:03}.png", index % 97))
                })
                .collect();
            let mut expected = paths.clone();
            expected.sort_unstable_by(|a, b| sort_key(a).cmp(&sort_key(b)).then_with(|| a.cmp(b)));
            expected.truncate(MAX_ENTRIES);

            for reverse in [false, true] {
                let mut entries = paths.clone();
                if reverse {
                    entries.reverse();
                }
                let selected = select_wallpapers(
                    entries.into_iter().map(|path| (path, EntryKind::File)),
                    |_| unreachable!("regular files need no stat"),
                );
                assert_eq!(selected, expected, "total={total}, reverse={reverse}");
            }
        }
    }

    #[test]
    fn entry_types_map_to_what_a_stat_would_still_need_to_answer() {
        let dir = temp_dir("kinds");
        std::fs::write(dir.join("file.png"), b"x").expect("write");
        std::fs::create_dir_all(dir.join("folder")).expect("dir");
        std::os::unix::fs::symlink(dir.join("file.png"), dir.join("link.png")).expect("link");

        let kind = |name: &str| {
            let entry = std::fs::read_dir(&dir)
                .expect("read_dir")
                .flatten()
                .find(|entry| entry.file_name() == name)
                .expect("entry");
            EntryKind::of(entry.file_type())
        };
        assert_eq!(kind("file.png"), EntryKind::File);
        assert_eq!(kind("folder"), EntryKind::Other);
        assert_eq!(kind("link.png"), EntryKind::Unresolved);
        assert_eq!(
            EntryKind::of(Err(std::io::Error::other("no type"))),
            EntryKind::Unresolved
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_directory_lists_nothing() {
        assert!(list_wallpapers(Path::new("/nonexistent/jwm/walls")).is_empty());
    }

    #[test]
    fn the_current_wallpaper_is_marked() {
        let path = Path::new("/walls/alps.jpg");
        assert!(picker_row(path, "/walls/alps.jpg").contains('\u{f00c}'));
        assert!(!picker_row(path, "/walls/other.jpg").contains('\u{f00c}'));
        assert!(picker_row(path, "").contains("alps.jpg"));
    }

    #[test]
    fn every_glyph_stays_in_the_widely_available_range() {
        // Same rule as the connectivity rows: FA5-era f6xx codepoints render
        // as hollow boxes in common Nerd Font builds.
        let row = picker_row(Path::new("/walls/a.jpg"), "/walls/a.jpg");
        for ch in row
            .chars()
            .filter(|ch| ('\u{f000}'..'\u{f900}').contains(ch))
        {
            assert!(
                (ch as u32) < 0xf600,
                "{ch:?} is outside the FontAwesome-4 range"
            );
        }
    }
}
