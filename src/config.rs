use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use std::fmt;
use std::rc::Rc;

use crate::core::animation::{AnimationSpeed, Easing};
use crate::core::layout::LayoutEnum;
use crate::jwm::WMFuncType;
use crate::jwm::{self, Jwm, WMButton, WMClickType, WMKey, WMRule};
use crate::terminal_prober::{ADVANCED_TERMINAL_PROBER, TerminalPurpose};
use std::time::Duration;

use crate::backend::common_define::keys as k;
use crate::backend::common_define::{KeySym, Mods, MouseButton};

mod validation;

pub use validation::{ConfigDiagnostic, ConfigDiagnosticLevel, ConfigDiagnostics};

pub const LOAD_LOCAL_CONFIG: bool = true;
pub(crate) const MAX_CURSOR_SIZE: u32 = 512;
const DEFAULT_CURSOR_SIZE: u32 = 24;

/// Resolve the effective scene-linear render-path gate.
///
/// `scene_linear_compositing` is a dependent feature: without the per-surface
/// color-management render path there is no reliable source transfer function
/// to decode into the linear intermediate.
#[cfg_attr(not(feature = "backend-wayland-udev"), allow(dead_code))]
pub(crate) const fn scene_linear_render_path_requested(
    color_management_render_path: bool,
    scene_linear_compositing: bool,
) -> bool {
    color_management_render_path && scene_linear_compositing
}

static CONFIG_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn resolve_cursor_size(configured: u32, environment: Option<&OsStr>) -> u32 {
    if configured != 0 {
        return configured.min(MAX_CURSOR_SIZE);
    }
    environment
        .and_then(OsStr::to_str)
        .map(str::trim)
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|size| (1..=MAX_CURSOR_SIZE).contains(size))
        .unwrap_or(DEFAULT_CURSOR_SIZE)
}

pub(crate) fn parse_terminal_override(
    value: Option<&OsStr>,
) -> Result<Option<Vec<String>>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let value = value
        .to_str()
        .ok_or_else(|| "value is not valid UTF-8".to_string())?;
    let argv = crate::command_line::split_command_line(value).map_err(|error| error.to_string())?;
    if argv.is_empty() {
        return Ok(None);
    }
    if argv.first().is_none_or(|program| program.trim().is_empty()) {
        return Err("command program is empty".into());
    }
    Ok(Some(argv))
}

fn terminal_override_from_env(name: &str) -> Option<Vec<String>> {
    match parse_terminal_override(std::env::var_os(name).as_deref()) {
        Ok(command) => command,
        Err(error) => {
            log::warn!("[config] ignoring invalid {name}: {error}");
            None
        }
    }
}

fn configured_terminal_execution_prefix(mut command: Vec<String>) -> Option<Vec<String>> {
    let program = command.first()?;
    if let Some(config) = ADVANCED_TERMINAL_PROBER.config_for_command(program) {
        command.push(config.execute_flag.clone()?);
        Some(command)
    } else {
        // Preserve the historical contract for explicitly configured,
        // third-party terminals. Known terminals use their declared flag.
        command.push("-e".to_string());
        Some(command)
    }
}

fn configured_scratchpad_terminal(command: Vec<String>) -> Option<Vec<String>> {
    let program = command.first()?;
    match ADVANCED_TERMINAL_PROBER.config_for_command(program) {
        Some(config) if !config.scratchpad_pid_stable => None,
        // Unknown explicit commands retain their historical behavior because
        // JWM cannot infer whether their child PID owns the resulting window.
        Some(_) | None => Some(command),
    }
}

fn renamed_legacy_terminal(program: &str) -> Option<&'static str> {
    match program {
        "jterm1" => Some("forge"),
        "jterm2" => Some("anvil"),
        "jterm3" => Some("ember"),
        "jterm4" => Some("frost"),
        _ => None,
    }
}

fn migrate_legacy_terminal_argument(function: &str, mut arg: jwm::WMArgEnum) -> jwm::WMArgEnum {
    let jwm::WMArgEnum::StringVec(argv) = &mut arg else {
        return arg;
    };
    let program_index = match function {
        "spawn" => 0,
        "togglescratchpad" if argv.len() > 1 => 1,
        _ => return arg,
    };
    let Some(program) = argv.get_mut(program_index) else {
        return arg;
    };
    if let Some(replacement) = renamed_legacy_terminal(program) {
        log::warn!(
            "[config] migrated legacy terminal command {program:?} to {replacement:?} in memory"
        );
        *program = replacement.to_string();
    }
    arg
}

fn resolve_write_destination(path: &Path) -> std::io::Result<std::path::PathBuf> {
    const MAX_SYMLINK_DEPTH: usize = 40;

    let mut destination = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_DEPTH {
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&destination)?;
                destination = if target.is_absolute() {
                    target
                } else {
                    destination
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(target)
                };
            }
            Ok(_) => return Ok(destination),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(destination);
            }
            Err(error) => return Err(error),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "configuration symlink chain exceeds {MAX_SYMLINK_DEPTH} entries: {}",
            path.display()
        ),
    ))
}

/// Quote a string as a TOML basic string.
///
/// Layout names are all lowercase ASCII today, so this is belt-and-braces —
/// but the value ends up in a file the parser has to read back, and a name
/// that ever grows a quote or a backslash would otherwise write a config that
/// no longer loads.
fn toml_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 || ch as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    // Preserve symlink-based dotfile setups. Renaming over `path` itself would
    // replace the link; resolving the complete chain lets us atomically
    // replace its target while leaving every user-managed link intact. The
    // lexical walk also supports a final target that does not exist yet.
    let destination = resolve_write_destination(path)?;
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let file_name = destination
        .file_name()
        .map_or_else(|| "config".into(), |name| name.to_string_lossy());
    let sequence = CONFIG_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.tmp-{}-{sequence}",
        std::process::id()
    ));

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        if let Ok(metadata) = fs::metadata(&destination) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        // Persist the directory entry as well as the file contents so a
        // successful return survives a sudden power loss on local filesystems.
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

// ---------------------------------------------------------------------------
// Backend family — set once by main() before CONFIG is first accessed.
// ---------------------------------------------------------------------------

/// Which backend family is running.  All wayland variants (udev, x11, winit)
/// map to `Wayland`; only the native X11 backend maps to `X11`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendFamily {
    X11,
    Wayland,
}

static ACTIVE_BACKEND: OnceLock<BackendFamily> = OnceLock::new();

/// Called from main.rs immediately after the backend is resolved, before any
/// CONFIG access.  Subsequent calls are silently ignored.
pub fn set_backend_family(family: BackendFamily) {
    let _ = ACTIVE_BACKEND.set(family);
}

/// Returns the active backend family, defaulting to X11 if not yet set.
pub fn get_backend_family() -> BackendFamily {
    *ACTIVE_BACKEND.get().unwrap_or(&BackendFamily::X11)
}

/// Matches the bar installed by `scripts/install_jwm_scripts.sh` when the user
/// does not select another implementation explicitly.
pub const STATUS_BAR_NAME: &str = "tao_pixels_bar";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TomlConfig {
    pub appearance: AppearanceConfig,
    pub behavior: BehaviorConfig,
    pub status_bar: StatusBarConfig,
    pub colors: ColorsConfig,
    pub keybindings: KeyBindingsConfig,
    pub mouse_bindings: MouseBindingsConfig,
    pub rules: Vec<RuleConfig>,
    pub layout: LayoutConfig,
    #[serde(default = "AnimationConfig::default_value")]
    pub animation: AnimationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppearanceConfig {
    pub border_px: u32,
    pub gap_px: u32,
    pub snap: u32,
    /// Fontconfig/Xft-style font used by JWM-owned overlays such as the lock
    /// screen, application launcher and keybinding viewer.
    #[serde(alias = "dmenu_font")]
    pub system_ui_font: String,
    pub status_bar_padding: i32,
    pub status_bar_height: i32,
    /// Xcursor theme name that decides the pointer's *shape/look*, e.g.
    /// "Adwaita", "Bibata-Modern-Ice", "capitaine-cursors".
    /// Must match an *installed* theme directory name under one of the icon
    /// search paths (`~/.local/share/icons`, `~/.icons`, `/usr/share/icons`,
    /// or `$XCURSOR_PATH`); the value is a directory name, not a display name.
    /// A name with no matching theme resolves to no images, so every backend
    /// silently falls back to its built-in glyph pointer and `cursor_size` is
    /// ignored — see the `[cursor]` warning logged by `XcursorImages`.
    /// Empty string = defer to the `XCURSOR_THEME` environment variable and
    /// then to "default". Applied by the Wayland DRM/KMS backend and exported
    /// to launched clients so the whole session shares one cursor style.
    #[serde(default)]
    pub cursor_theme: String,
    /// Pointer size in logical pixels — the macOS-style "pointer size" slider.
    /// Typical values: 24 (normal), 32/48/64 (progressively larger).
    /// 0 = defer to the `XCURSOR_SIZE` environment variable and then to 24.
    #[serde(default)]
    pub cursor_size: u32,
    /// Design language for the surfaces JWM draws itself — the debug HUD, the
    /// modal system-UI card, toasts and the volume/brightness OSD.
    ///
    /// * `"glass"` (default) — Apple's light frosted glass: each card samples
    ///   a blurred copy of the desktop behind it through a squircle mask,
    ///   refracts it at the bevel, and lifts it under a white veil with a rim
    ///   hairline.
    /// * `"glass-dark"` — the same optics under a graphite veil, for a dark UI.
    /// * `"aurora"` — the same optics under a deep indigo veil with an
    ///   aurora-teal rim: tinted glass rather than neutral.
    /// * `"material"` — opaque elevated cards with a drop shadow.
    /// * `"nord"` — flat cards in the Nord palette.
    /// * `"tokyo-night"` — flat cards in the Tokyo Night palette.
    /// * `"paper"` — flat off-white cards with dark ink; light without blur.
    ///
    /// The compositor keeps its blur chain alive for the glass themes even
    /// when `blur_enabled` is off; without a usable chain the cards fall back
    /// to flat translucent fills.
    ///
    /// Only affects JWM's own overlays; client windows are unchanged.
    #[serde(default = "default_ui_theme")]
    pub ui_theme: String,
}

/// Where a freshly managed window is inserted into its monitor's client list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewClientPosition {
    /// Head of the list: the new window becomes master (dwm's `attach`).
    Master,
    /// End of the list: the new window trails every existing one.
    Tail,
    /// Directly behind the focused window.
    AfterFocused,
}

impl NewClientPosition {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "tail" => NewClientPosition::Tail,
            "after_focused" => NewClientPosition::AfterFocused,
            _ => NewClientPosition::Master,
        }
    }
}

/// Which windows may start an interactive move/resize through the client
/// protocol (`_NET_WM_MOVERESIZE`: CSD title bars, invisible resize borders).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientMoveResize {
    /// Every window: floating windows move/resize, tiled windows reorder
    /// within the layout on move and float on resize.
    Always,
    /// Only windows that already float (default). A tiled window's layout
    /// slot cannot be disturbed by a client-side drag region.
    FloatingOnly,
    /// Ignore client move/resize requests entirely.
    Never,
}

impl ClientMoveResize {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" => ClientMoveResize::Always,
            "never" => ClientMoveResize::Never,
            _ => ClientMoveResize::FloatingOnly,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorConfig {
    pub focus_follows_new_window: bool,
    /// Where a newly-managed window lands in its monitor's client list, which
    /// is what decides its slot in tiling layouts:
    /// - "master" (default): head of the list, i.e. the master area (dwm-like);
    /// - "tail": end of the list, behind every existing window;
    /// - "after_focused": right behind the currently focused window, falling
    ///   back to "master" when nothing comparable is focused.
    /// Floating windows keep their own group at the end of the list either way.
    #[serde(default = "default_new_client_position")]
    pub new_client_position: String,
    /// Drag dead zone in pixels: an interactive move/resize only engages once
    /// the pointer has travelled this far from the press. Below it the button
    /// release is a plain click and the window is left untouched, so a
    /// click-and-hold can no longer accidentally pop a window out of the
    /// tiling layout.
    #[serde(default = "default_drag_threshold_px")]
    pub drag_threshold_px: u32,
    /// Which windows may start an interactive move/resize through the client
    /// protocol (`_NET_WM_MOVERESIZE`: CSD title bars, invisible resize
    /// borders):
    /// - "floating-only" (default): only already-floating windows respond;
    /// - "always": floating windows move/resize; tiled windows reorder
    ///   within the layout on move, and float on resize;
    /// - "never": client requests are ignored entirely.
    #[serde(default = "default_client_moveresize")]
    pub client_moveresize: String,
    pub resize_hints: bool,
    pub lock_fullscreen: bool,
    #[serde(default)]
    pub compositor: bool,
    /// X11 compositor graphics API: "egl" (GLES 3, default), "glx" (legacy),
    /// or "auto" (prefer EGL/GLES and fall back to GLX).
    #[serde(default = "default_compositor_api")]
    pub compositor_api: String,
    /// Corner radius in pixels for window rounding (0 = sharp corners).
    #[serde(default = "default_corner_radius")]
    pub corner_radius: f32,
    /// Enable drop shadows behind windows.
    #[serde(default = "default_true")]
    pub shadow_enabled: bool,
    /// Shadow blur radius in pixels.
    #[serde(default = "default_shadow_radius")]
    pub shadow_radius: f32,
    /// Shadow offset (x, y) in pixels.
    #[serde(default = "default_shadow_offset")]
    pub shadow_offset: [f32; 2],
    /// Shadow color as [r, g, b, a] in 0.0..1.0 range.
    #[serde(default = "default_shadow_color")]
    pub shadow_color: [f32; 4],

    /// Multiplier on the shadow alpha of unfocused windows (0.0..1.0).
    /// 1.0 keeps every shadow equally strong; lower values let the focused
    /// window cast a visibly deeper shadow, a subtle depth cue
    /// (default 0.65).
    #[serde(default = "default_shadow_inactive_opacity")]
    pub shadow_inactive_opacity: f32,
    /// Opacity for unfocused windows (0.0..1.0). 1.0 = fully opaque (no dim).
    #[serde(default = "default_inactive_opacity")]
    pub inactive_opacity: f32,
    /// Opacity for active/focused windows (0.0..1.0). 1.0 = fully opaque.
    #[serde(default = "default_active_opacity")]
    pub active_opacity: f32,
    /// Enable background blur behind translucent windows.
    #[serde(default = "default_true")]
    pub blur_enabled: bool,
    /// Blur strength / number of passes (1..5). Higher = more blur.
    #[serde(default = "default_blur_strength")]
    pub blur_strength: u32,
    /// Automatically reduce blur quality during animations/transitions for better performance.
    #[serde(default = "default_true")]
    pub blur_quality_auto: bool,
    /// Enable fade-in/fade-out when windows map/unmap.
    #[serde(default = "default_true")]
    pub fading: bool,
    /// Fade-in step per frame (0.0..1.0). Higher = faster fade-in.
    #[serde(default = "default_fade_step")]
    pub fade_in_step: f32,
    /// Fade-out step per frame (0.0..1.0). Higher = faster fade-out.
    #[serde(default = "default_fade_step")]
    pub fade_out_step: f32,
    /// Window classes to exclude from shadows, e.g. ["Alacritty", "kitty"].
    #[serde(default)]
    pub shadow_exclude: Vec<String>,
    /// Per-window opacity rules, e.g. ["90:Alacritty", "85:kitty"].
    /// Format: "opacity_percent:class_name".
    #[serde(default)]
    pub opacity_rules: Vec<String>,
    /// Window classes to exclude from blur.
    #[serde(default)]
    pub blur_exclude: Vec<String>,
    /// Blur behind the status bar like any other translucent window.
    ///
    /// The bar is the one window that always sits directly over the wallpaper,
    /// so leaving it out of the blur pass is what makes a translucent bar show
    /// a sharp backdrop. Turn this off for a bar that frosts its own wallpaper
    /// strip, which would otherwise composite over a second blur.
    #[serde(default = "default_true")]
    pub blur_status_bar: bool,
    /// Enable temporal blur reuse: blend current frame blur with previous frame for stable content.
    #[serde(default = "default_true")]
    pub blur_temporal_enabled: bool,
    /// Temporal blur mix ratio: 0.0 = all new, 1.0 = all previous frame. Default 0.8 = 80% prev + 20% new.
    #[serde(default = "default_temporal_blur_ratio")]
    pub blur_temporal_mix_ratio: f32,
    /// Dynamic blur strength based on monitor refresh rate (Hz). Format: "60:2,75:2.5,144:3.5".
    /// If monitor Hz not listed, uses closest lower Hz value. If no lower value, uses closest higher.
    #[serde(default = "default_blur_strength_by_hz")]
    pub blur_strength_by_hz: String,
    /// Per-monitor blur quality. Format: "primary:Full,secondary:Reduced".
    /// Monitors: "primary" (0), "secondary" (1), "tertiary" (2), etc.
    /// Quality: "Full", "Reduced", "Minimal".
    #[serde(default = "default_blur_quality_by_monitor")]
    pub blur_quality_by_monitor: String,
    /// Window classes to exclude from rounded corners.
    #[serde(default)]
    pub rounded_corners_exclude: Vec<String>,
    /// Detect windows that manage their own opacity (skip forced opacity).
    #[serde(default = "default_true")]
    pub detect_client_opacity: bool,
    /// Unredirect fullscreen windows for direct scanout (better perf).
    #[serde(default = "default_true")]
    pub fullscreen_unredirect: bool,
    /// VSync method: "global" (default), "oml_sync_control", "present".
    /// "oml_sync_control" uses GLX_OML_sync_control for per-window MSC-based vblank timing.
    /// "present" uses X11 Present extension for per-window independent presentation.
    /// Falls back to "global" if the selected method is unavailable.
    #[serde(default = "default_vsync_method")]
    pub vsync_method: String,
    /// Enable audio-video synchronization: windows with audio streams will render
    /// at their audio's frame rate instead of the compositor's fixed rate.
    #[serde(default = "default_true")]
    pub enable_audio_sync: bool,
    /// Audio buffer latency in milliseconds (used for sync calculations).
    #[serde(default = "default_audio_buffer_latency")]
    pub audio_buffer_latency_ms: u32,
    /// Enable Present extension for per-window independent presentation.
    #[serde(default = "default_true")]
    pub present_enabled: bool,

    // --- VRR (Variable Refresh Rate) Support ---
    /// Enable Variable Refresh Rate (VRR/G-Sync/FreeSync) support for game windows.
    #[serde(default = "default_true")]
    pub vrr_enabled: bool,
    /// Minimum FPS for VRR range (Hz).
    #[serde(default = "default_vrr_min_fps")]
    pub vrr_min_fps: u32,
    /// Maximum FPS for VRR range (Hz).
    #[serde(default = "default_vrr_max_fps")]
    pub vrr_max_fps: u32,
    /// Window classes to treat as games (enable VRR when focused).
    /// Examples: ["steam", "lutris", "wine", "minecraft"].
    #[serde(default)]
    pub game_classes: Vec<String>,

    /// Allow wlr-output-management clients (kanshi, wlr-randr) to perform a
    /// real DRM modeset on Apply.
    /// Default false: jwm advertises mode information but rejects mode changes
    /// at the Apply step until explicitly enabled. Position/scale/transform
    /// changes are honored when the resulting physical framebuffer envelope
    /// stays the same. Runtime envelope growth/shrink is rejected even with
    /// this flag enabled until KMS configuration and transactional GLES target
    /// preparation can be committed as one operation; a failed modeset or GPU
    /// allocation must never leave the output and compositor at different sizes.
    #[serde(default)]
    pub wlr_output_mgmt_allow_modeset: bool,

    // --- Wayland optional protocol globals ---
    /// Publish zwlr_screencopy_manager_v1 for grim/slurp-style screenshots.
    #[serde(default = "default_true")]
    pub wayland_enable_screencopy: bool,
    /// Publish wp_tearing_control_manager_v1 for game/latency hints.
    #[serde(default = "default_true")]
    pub wayland_enable_tearing_control: bool,
    /// Publish wp_color_manager_v1. Default off until users opt into advanced
    /// client color protocol negotiation.
    #[serde(default)]
    pub wayland_enable_color_management: bool,
    /// Publish zwlr_output_manager_v1 for kanshi/wlr-randr.
    #[serde(default = "default_true")]
    pub wayland_enable_output_management: bool,
    /// Publish zwlr_output_power_manager_v1 for DPMS tools.
    #[serde(default = "default_true")]
    pub wayland_enable_output_power: bool,
    /// Publish ext_workspace_manager_v1 for bars/task switchers.
    #[serde(default = "default_true")]
    pub wayland_enable_workspace: bool,
    /// Publish ext-image-copy-capture protocol globals.
    #[serde(default = "default_true")]
    pub wayland_enable_image_copy_capture: bool,
    /// Publish zwlr_gamma_control_manager_v1 for wlsunset/gammastep.
    #[serde(default = "default_true")]
    pub wayland_enable_gamma_control: bool,
    /// Publish zwlr_foreign_toplevel_manager_v1 for taskbars/window tools.
    #[serde(default = "default_true")]
    pub wayland_enable_foreign_toplevel_management: bool,
    /// Publish zwlr_virtual_pointer_manager_v1 for remote-control tools.
    #[serde(default = "default_true")]
    pub wayland_enable_virtual_pointer: bool,

    // --- Feature 1: Window borders ---
    /// Enable window border/outline rendering.
    #[serde(default = "default_true")]
    pub border_enabled: bool,
    /// Border width in pixels.
    #[serde(default = "default_border_width")]
    pub border_width: f32,
    /// Border color for focused window [r, g, b, a].
    #[serde(default = "default_border_color_focused")]
    pub border_color_focused: [f32; 4],
    /// Border color for unfocused windows [r, g, b, a].
    #[serde(default = "default_border_color_unfocused")]
    pub border_color_unfocused: [f32; 4],

    // --- Client window border glow ---
    /// Enable the compositor-drawn directional outer glow around client windows.
    #[serde(default = "default_true")]
    pub border_glow_enabled: bool,
    /// Restrict the glow to the focused client window.
    #[serde(default = "default_true")]
    pub border_glow_focused_only: bool,
    /// Maximum glow reach outside the client rectangle, in pixels.
    #[serde(default = "default_border_glow_radius")]
    pub border_glow_radius: f32,
    /// Multiplier applied to the configured glow alpha.
    #[serde(default = "default_border_glow_intensity")]
    pub border_glow_intensity: f32,
    /// Glow color as [r, g, b, a] in the 0.0..1.0 range.
    #[serde(default = "default_border_glow_color")]
    pub border_glow_color: [f32; 4],
    /// Case-insensitive class/app-id substrings allowed to glow. Empty means all clients.
    #[serde(default)]
    pub border_glow_include: Vec<String>,
    /// Case-insensitive class/app-id substrings excluded from glow; takes precedence.
    #[serde(default)]
    pub border_glow_exclude: Vec<String>,

    // --- Gradient border (focused window) ---
    /// Draw the focused window's border as a two-color linear gradient
    /// instead of the flat `border_color_focused`. Enabled by default.
    #[serde(default = "default_true")]
    pub border_gradient_enabled: bool,
    /// Gradient start color as [r, g, b, a] in the 0.0..1.0 range.
    #[serde(default = "default_border_gradient_color_a")]
    pub border_gradient_color_a: [f32; 4],
    /// Gradient end color as [r, g, b, a] in the 0.0..1.0 range.
    #[serde(default = "default_border_gradient_color_b")]
    pub border_gradient_color_b: [f32; 4],
    /// Gradient direction in degrees. 0 = left→right, 90 = top→bottom.
    #[serde(default = "default_border_gradient_angle")]
    pub border_gradient_angle: f32,
    /// Gradient rotation speed in degrees per second. 0 keeps the gradient
    /// static; non-zero slowly rotates the direction (costs continuous
    /// redraws while a focused window is visible).
    #[serde(default)]
    pub border_gradient_speed: f32,

    // --- Feature 3: Per-window corner radius ---
    /// Per-window corner radius rules, e.g. ["0:Alacritty", "20:firefox"].
    /// Format: "radius:class_name".
    #[serde(default)]
    pub corner_radius_rules: Vec<String>,

    // --- Feature 4: Window scale (PiP/overview) ---
    /// Window classes that should render at a smaller scale (PiP mode).
    #[serde(default)]
    pub scale_rules: Vec<String>,

    // --- Feature 8: Color temperature / night mode ---
    /// Color temperature shift: 0.0 = neutral, >0 = warm (night mode), <0 = cool.
    #[serde(default)]
    pub color_temperature: f32,
    /// Saturation multiplier: 1.0 = normal, 0.0 = grayscale.
    #[serde(default = "default_one")]
    pub saturation: f32,
    /// Brightness multiplier.
    #[serde(default = "default_one")]
    pub brightness: f32,
    /// Contrast multiplier.
    #[serde(default = "default_one")]
    pub contrast: f32,

    // --- Feature 10: Invert / accessibility ---
    /// Invert all colors (accessibility).
    #[serde(default)]
    pub invert_colors: bool,
    /// Force grayscale (accessibility).
    #[serde(default)]
    pub grayscale: bool,

    // --- P3: HDR / 10-bit output ---
    /// Enable JWM's HDR post-processing policy. This does not currently enable
    /// DRM HDR signalling; the Udev backend keeps `HDR_OUTPUT_METADATA`
    /// fail-closed until KMS-external elements join the color pipeline.
    #[serde(default)]
    pub hdr_enabled: bool,
    /// Target display peak luminance in nits (400=HDR400, 600=HDR600, 1000=HDR1000).
    #[serde(default = "default_hdr_peak_nits")]
    pub hdr_peak_nits: f32,
    /// Tone mapping method: "none", "reinhard", "aces".
    #[serde(default = "default_tone_mapping_method")]
    pub tone_mapping_method: String,
    /// Apply per-surface wp-color-management transforms in the window shader.
    /// With `scene_linear_compositing`, described surfaces are decoded and
    /// mapped into the normalized linear-sRGB common workspace; the output
    /// gamut/transfer step is deferred. Without that workspace, the historical
    /// encoded path still targets the window's largest-overlap output. Default
    /// off — the math is pixel-tested but still awaits broad HW visual checks.
    /// When off, described and undescribed content retain the legacy sRGB
    /// assumption; renderer-wide alpha and safety fixes still apply.
    #[serde(default)]
    pub color_management_render_path: bool,

    /// SOTA #2: composite windows and scene-linear-aware overlays in an FP16,
    /// normalized linear-sRGB common workspace. On a safe frame tail, supported
    /// physical output regions receive their own gamut/transfer encode, or all
    /// participating CRTCs coherently own the matching CTM+LUT pair. Encoded
    /// late overlays, KMS-external elements/capture, unsupported topology or a
    /// missing FP16 target select the conservative global-sRGB route. Because
    /// the normal desktop cursor is currently KMS-external, that fallback is
    /// expected for most interactive frames. Wallpaper, shadows and blur are
    /// still assembled encoded then decoded once, so this does not claim a
    /// physically linear blur. Requires `color_management_render_path`; costs
    /// one FP16 target plus decode/finalization passes. Default off.
    #[serde(default)]
    pub scene_linear_compositing: bool,

    /// Offload final linear-sRGB→output gamut and transfer work as one CRTC
    /// `CTM` + `GAMMA_LUT` pair. It is active only when every participating,
    /// DPMS-on output supports and accepts both properties and the frame tail
    /// is linear-safe; a LUT without its CTM is rolled back. Delivery ownership
    /// is all-or-nothing across outputs. Otherwise the renderer uses eligible
    /// software output regions or the global-sRGB fallback. Default off pending
    /// HW visual A/B.
    #[serde(default)]
    pub kms_color_pipeline_offload: bool,

    // --- Feature 11: Performance debug HUD ---
    /// Show FPS / frame time debug overlay.
    #[serde(default)]
    pub debug_hud: bool,

    // --- Phase 2 Optimizations ---
    /// Enable frame profiling (logs zone timing every 5s).
    #[serde(default)]
    pub profiling_enabled: bool,
    /// Enable direct scanout for fullscreen windows (bypass compositor).
    #[serde(default = "default_true")]
    pub direct_scanout_enabled: bool,
    /// Enable GL state tracking to avoid redundant state changes.
    #[serde(default = "default_true")]
    pub gl_state_tracking_enabled: bool,

    // --- Feature 13: Blur mask / frame extents ---
    /// Exclude window frame/title area from blur (use _NET_FRAME_EXTENTS).
    #[serde(default)]
    pub blur_use_frame_extents: bool,

    // --- Feature 14: Shadow shape / non-uniform offset ---
    /// Extra shadow offset for bottom edge (heavier shadow below).
    #[serde(default = "default_shadow_bottom_extra")]
    pub shadow_bottom_extra: f32,

    // --- Tag-switch transition mode ---
    /// Workspace switch transition mode: "none" (default), "slide", "cube", "fade", "flip", "zoom", "stack", "blinds".
    #[serde(default = "default_transition_mode")]
    pub transition_mode: String,

    // --- Window open/close scale animation ---
    #[serde(default)]
    pub window_animation: bool,
    #[serde(default = "default_window_animation_scale")]
    pub window_animation_scale: f32,

    // --- Dim inactive windows ---
    #[serde(default = "default_one")]
    pub inactive_dim: f32,

    // --- Desaturate inactive windows ---
    /// How far unfocused windows shift toward grayscale: 0.0 keeps full
    /// color, 1.0 renders them fully desaturated (default 0.25). Combines
    /// with `inactive_dim` and `inactive_opacity` to make the focused
    /// window pop.
    #[serde(default = "default_inactive_desaturate")]
    pub inactive_desaturate: f32,

    // --- Screen edge glow ---
    #[serde(default)]
    pub edge_glow: bool,
    #[serde(default = "default_edge_glow_color")]
    pub edge_glow_color: [f32; 4],
    #[serde(default = "default_edge_glow_width")]
    pub edge_glow_width: f32,

    // --- Attention animation (urgent pulse) ---
    #[serde(default = "default_true")]
    pub attention_animation: bool,
    #[serde(default = "default_attention_color")]
    pub attention_color: [f32; 4],

    // --- PiP visual treatment ---
    #[serde(default = "default_pip_border_color")]
    pub pip_border_color: [f32; 4],
    #[serde(default = "default_pip_border_width")]
    pub pip_border_width: f32,

    // --- Night light ---
    #[serde(default)]
    pub night_light: bool,
    #[serde(default = "default_night_light_temp")]
    pub night_light_temp: f32,
    #[serde(default = "default_night_light_start")]
    pub night_light_start: String,
    #[serde(default = "default_night_light_end")]
    pub night_light_end: String,
    #[serde(default = "default_night_light_transition")]
    pub night_light_transition_mins: u32,

    // --- Session menu ---
    // Commands the session menu runs. Each is parsed into argv without a
    // shell; quotes preserve spaces, while operators remain literal. Logout
    // is handled inside JWM (it quits the window manager) and has no command.
    #[serde(default = "default_suspend_command")]
    pub suspend_command: String,
    #[serde(default = "default_hibernate_command")]
    pub hibernate_command: String,
    #[serde(default = "default_reboot_command")]
    pub reboot_command: String,
    #[serde(default = "default_shutdown_command")]
    pub shutdown_command: String,

    // --- Magnifier ---
    #[serde(default)]
    pub magnifier_enabled: bool,
    #[serde(default = "default_magnifier_radius")]
    pub magnifier_radius: f32,
    #[serde(default = "default_magnifier_zoom")]
    pub magnifier_zoom: f32,

    // --- Window 3D tilt ---
    #[serde(default)]
    pub window_tilt: bool,
    #[serde(default = "default_tilt_amount")]
    pub tilt_amount: f32,
    #[serde(default = "default_tilt_perspective")]
    pub tilt_perspective: f32,
    #[serde(default = "default_tilt_speed")]
    pub tilt_speed: f32,
    #[serde(default = "default_tilt_grid")]
    pub tilt_grid: u32,

    // --- Frosted glass ---
    #[serde(default)]
    pub frosted_glass_rules: Vec<String>,
    #[serde(default = "default_frosted_glass_strength")]
    pub frosted_glass_strength: u32,

    // --- Alt-Tab window overview ---
    #[serde(default = "default_true")]
    pub overview_enabled: bool,
    #[serde(default = "default_overview_gap")]
    pub overview_thumbnail_gap: f32,

    // --- Wobbly windows ---
    #[serde(default)]
    pub wobbly_windows: bool,
    #[serde(default = "default_wobbly_stiffness")]
    pub wobbly_stiffness: f32,
    #[serde(default = "default_wobbly_damping")]
    pub wobbly_damping: f32,
    #[serde(default = "default_wobbly_restore_stiffness")]
    pub wobbly_restore_stiffness: f32,
    #[serde(default = "default_wobbly_grid_size")]
    pub wobbly_grid_size: u32,

    // --- Particle effects ---
    #[serde(default)]
    pub particle_effects: bool,
    #[serde(default = "default_particle_count")]
    pub particle_count: u32,
    #[serde(default = "default_particle_lifetime")]
    pub particle_lifetime: f32,
    #[serde(default = "default_particle_gravity")]
    pub particle_gravity: f32,

    // --- Expose/Mission Control ---
    #[serde(default = "default_true")]
    pub expose_enabled: bool,
    #[serde(default = "default_expose_gap")]
    pub expose_gap: f32,

    // --- Smart Snap Preview ---
    #[serde(default = "default_true")]
    pub snap_preview: bool,
    #[serde(default = "default_snap_preview_color")]
    pub snap_preview_color: [f32; 4],
    #[serde(default = "default_snap_animation_duration_ms")]
    pub snap_animation_duration_ms: u64,

    // --- Window Peek (Boss Key) ---
    #[serde(default = "default_true")]
    pub peek_enabled: bool,
    #[serde(default)]
    pub peek_exclude: Vec<String>,

    // --- Window Tabs ---
    /// Strip across the top of a monitor's tiling area, one cell per tiled
    /// window. It is drawn in the `appearance.ui_theme` palette, like every
    /// other surface JWM paints itself, so it has no colors of its own.
    #[serde(default = "default_true")]
    pub window_tabs: bool,
    #[serde(default = "default_tab_bar_height")]
    pub tab_bar_height: f32,

    // --- Motion trail (drag ghosting) ---
    /// Enable motion trail ghost copies when dragging windows.
    #[serde(default = "default_true")]
    pub motion_trail: bool,
    /// Number of ghost frames in the motion trail.
    #[serde(default = "default_motion_trail_frames")]
    pub motion_trail_frames: u32,
    /// Base opacity for motion trail ghosts (0.0..1.0).
    #[serde(default = "default_motion_trail_opacity")]
    pub motion_trail_opacity: f32,

    // --- Genie minimize animation ---
    /// Enable genie/magic lamp minimize animation.
    #[serde(default = "default_true")]
    pub genie_minimize: bool,
    /// Duration of the genie animation in milliseconds.
    #[serde(default = "default_genie_duration")]
    pub genie_duration_ms: u64,

    // --- Window open ripple ---
    /// Enable ripple distortion effect when a window opens.
    #[serde(default)]
    pub ripple_on_open: bool,
    /// Duration of the ripple effect in seconds.
    #[serde(default = "default_ripple_duration")]
    pub ripple_duration: f32,
    /// Amplitude of the ripple distortion in UV space.
    #[serde(default = "default_ripple_amplitude")]
    pub ripple_amplitude: f32,

    // --- Focus switch highlight ---
    /// Enable a smooth border highlight on focus change.
    #[serde(default = "default_true")]
    pub focus_highlight: bool,
    /// Focus highlight border color [r, g, b, a].
    #[serde(default = "default_focus_highlight_color")]
    pub focus_highlight_color: [f32; 4],
    /// Duration of focus highlight in milliseconds.
    #[serde(default = "default_focus_highlight_duration")]
    pub focus_highlight_duration_ms: u64,

    // --- Wallpaper crossfade ---
    /// Enable smooth crossfade when wallpaper changes.
    #[serde(default = "default_true")]
    pub wallpaper_crossfade: bool,
    /// Duration of wallpaper crossfade in milliseconds.
    #[serde(default = "default_wallpaper_crossfade_duration")]
    pub wallpaper_crossfade_duration_ms: u64,

    // --- Phase 6: Accessibility & Utility ---
    /// Colorblind correction mode: "", "deuteranopia", "protanopia", "tritanopia".
    #[serde(default)]
    pub colorblind_mode: String,
    /// Annotation pen color [r, g, b, a].
    #[serde(default = "default_annotation_color")]
    pub annotation_color: [f32; 4],
    /// Annotation pen width in pixels.
    #[serde(default = "default_annotation_line_width")]
    pub annotation_line_width: f32,
    /// Freeze the fully composited desktop behind the interactive screenshot
    /// selector/editor. Disable this to keep client windows and animations
    /// live while a region is selected or annotated.
    #[serde(default = "default_true")]
    pub screenshot_freeze_enabled: bool,

    // --- Phase 7: Diagnostics ---
    /// Enable shader hot reload from files.
    #[serde(default)]
    pub shader_hot_reload: bool,
    /// Directory path to watch for shader files.
    #[serde(default)]
    pub shader_dir: String,
    /// Enable extended debug HUD (draw calls, texture memory, etc.).
    #[serde(default)]
    pub debug_hud_extended: bool,
    /// Recording FPS (frames per second) for screen recording.
    #[serde(default = "default_recording_fps")]
    pub recording_fps: u32,
    /// Recording bitrate (e.g. "20M", "10M", "5000k"). Used by NVENC and software encoders.
    #[serde(default = "default_recording_bitrate")]
    pub recording_bitrate: String,
    /// Recording quality (QP value 0-51, lower=better). Used by VAAPI (CQP mode).
    #[serde(default = "default_recording_quality")]
    pub recording_quality: u32,
    /// Recording encoder: "auto" (probe NVENC>VAAPI>SW), "nvenc", "vaapi", "software".
    #[serde(default = "default_recording_encoder")]
    pub recording_encoder: String,
    /// Recording output directory (empty = $XDG_VIDEOS_DIR or ~/Videos).
    #[serde(default)]
    pub recording_output_dir: String,
    /// Capture microphone audio alongside screen recordings.
    #[serde(default = "default_true")]
    pub recording_audio_enabled: bool,
    /// ALSA capture device used for the screen recording audio track.
    #[serde(default = "default_audio_recording_device")]
    pub recording_audio_device: String,
    /// AAC bitrate for the screen recording audio track.
    #[serde(default = "default_recording_audio_bitrate")]
    pub recording_audio_bitrate: String,
    /// ALSA capture device used by the built-in audio recorder.
    #[serde(default = "default_audio_recording_device")]
    pub audio_recording_device: String,
    /// Standalone recorder backend: "auto", "direct", or "ffmpeg".
    #[serde(default = "default_audio_recording_backend")]
    pub audio_recording_backend: String,
    /// Default standalone recording format: "wav", "flac", "opus", or "mp3".
    #[serde(default = "default_audio_recording_format")]
    pub audio_recording_format: String,
    /// Bitrate used for standalone Opus and MP3 recording.
    #[serde(default = "default_recording_audio_bitrate")]
    pub audio_recording_bitrate: String,
    /// Audio recording output directory (empty = $XDG_MUSIC_DIR or ~/Music).
    #[serde(default)]
    pub audio_recording_output_dir: String,
    /// Requested WAV sample rate. ALSA may negotiate the nearest supported rate.
    #[serde(default = "default_audio_recording_sample_rate")]
    pub audio_recording_sample_rate: u32,
    /// Requested capture channels (1 or 2).
    #[serde(default = "default_audio_recording_channels")]
    pub audio_recording_channels: u16,

    // --- Idle ---
    /// Seconds of inactivity before the screen dims. 0 switches the stage off.
    #[serde(default = "default_idle_dim_secs")]
    pub idle_dim_secs: u64,
    /// Fraction of normal brightness while dimmed.
    #[serde(default = "default_idle_dim_level")]
    pub idle_dim_level: f32,
    /// Seconds of inactivity before the built-in lock screen appears. 0 (the
    /// default) switches it off: unlocking needs PAM, and a session that
    /// cannot reach PAM would be locked out of itself, so turning this on is
    /// a decision only the user can make.
    #[serde(default)]
    pub idle_lock_secs: u64,
    /// Seconds of inactivity before `idle_screen_off_command` runs. 0, or an
    /// empty command, switches the stage off.
    #[serde(default)]
    pub idle_screen_off_secs: u64,
    /// Command that powers the displays down, e.g. `xset dpms force off` or
    /// `wlopm --off '*'`. JWM does not do this itself: which knob is right
    /// depends on the session, and getting it wrong leaves a black screen.
    #[serde(default)]
    pub idle_screen_off_command: String,
    /// Command that powers the displays back up on the next input. Needed
    /// only for tools that do not restore themselves.
    #[serde(default)]
    pub idle_screen_on_command: String,

    /// Show CPU, memory and network rows in the control center. Switching it
    /// off also stops the sampling: without the rows there is nothing to read
    /// `/proc` for.
    #[serde(default = "default_true")]
    pub resource_rows: bool,

    // --- Wallpaper ---
    /// Path to wallpaper image file (empty = solid black background).
    /// Used as the default wallpaper for all monitors unless overridden by wallpaper_monitors.
    #[serde(default)]
    pub wallpaper: String,
    /// Wallpaper display mode: "fill" (crop to fill), "fit" (letterbox), "stretch", "center".
    #[serde(default = "default_wallpaper_mode")]
    pub wallpaper_mode: String,
    /// Remember what was copied, so the clipboard picker has something to
    /// offer. Memory only: the history is never written to disk and does not
    /// survive a restart. Offers marked as secrets are never recorded.
    #[serde(default = "default_clipboard_history")]
    pub clipboard_history: bool,
    /// Show the film-strip layout picker when cycling layouts, instead of
    /// switching silently. The picker still cycles on every press of the same
    /// key, so turning this off only removes the panel. Needs the compositor;
    /// without one, cycling falls back to switching silently anyway.
    #[serde(default = "default_true")]
    pub layout_picker: bool,
    /// Directory the wallpaper picker lists. Empty means "beside the current
    /// wallpaper", then ~/Pictures/Wallpapers, then ~/Pictures.
    #[serde(default)]
    pub wallpaper_dir: String,
    /// Take the border, gradient and glow colours from the wallpaper whenever
    /// it changes. On by default: a shell that matches the picture behind it
    /// is what the wallpaper is for. The colours are only overwritten in
    /// memory — the config file is never rewritten, so setting this to false
    /// and reloading restores whatever is on disk.
    #[serde(default = "default_true")]
    pub wallpaper_colors: bool,
    /// Per-monitor wallpaper overrides. Each entry specifies a monitor index and its wallpaper.
    /// Monitor index 0 is the primary monitor, 1 is the second, etc.
    /// Monitors without an entry fall back to the global `wallpaper` setting.
    #[serde(default)]
    pub wallpaper_monitors: Vec<WallpaperMonitorConfig>,
    /// Per-tag wallpaper overrides. Each entry specifies a tag (and optionally monitor)
    /// with its own wallpaper. Resolution priority when the tag is active:
    /// tag-specific (monitor match) > tag-specific (any monitor) > monitor override > global.
    #[serde(default)]
    pub wallpaper_tags: Vec<WallpaperTagConfig>,

    // --- Window swallowing ---
    /// Hide a terminal window when a child process opens its own window
    /// (X11 only — relies on _NET_WM_PID + /proc walk).
    #[serde(default = "default_true")]
    pub swallow_enabled: bool,
    /// Class names of terminals that may be swallowed. Empty = no swallowing.
    /// Match is case-insensitive against both class and instance.
    #[serde(default)]
    pub swallow_terminals: Vec<String>,
    /// Class names that should NEVER swallow their parent (popups, menus, etc).
    #[serde(default)]
    pub swallow_exceptions: Vec<String>,

    // --- Scrolling layout identity ---
    /// Default scrolling column width rules. Format: "factor:pattern"; pattern
    /// is matched as a substring against window name, class, or instance when a
    /// new window creates a new scrolling column. Example: "1.35:Firefox".
    #[serde(default)]
    pub scrolling_column_width_rules: Vec<String>,

    // --- Touchpad gestures (Wayland only) ---
    /// Touchpad swipe-gesture bindings. 3+ finger swipes are intercepted only
    /// when their finger count has at least one configured binding; 1- and
    /// 2-finger swipes continue to forward to clients.
    #[serde(default)]
    pub gesture_swipe: Vec<GestureSwipeConfig>,
    /// Minimum cumulative pixel delta along the dominant axis before a swipe
    /// triggers its action. Smaller = more sensitive. Default 80.
    #[serde(default = "default_gesture_swipe_threshold")]
    pub gesture_swipe_threshold: f64,

    // --- Do-not-disturb ---
    /// When true, suppress urgent-window focus-stealing and hide notification
    /// surfaces (X11 _NET_WM_WINDOW_TYPE_NOTIFICATION). Toggle live via the
    /// `toggle_dnd` IPC command.
    #[serde(default)]
    pub do_not_disturb: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GestureSwipeConfig {
    /// Number of fingers (3, 4, or 5).
    pub fingers: u32,
    /// Direction: "left", "right", "up", "down".
    pub direction: String,
    /// Command name (any IPC dispatch_command name, e.g. "loopview").
    pub function: String,
    /// Argument passed to the command. See ArgumentConfig.
    #[serde(default)]
    pub argument: ArgumentConfig,
}

fn default_gesture_swipe_threshold() -> f64 {
    80.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WallpaperMonitorConfig {
    /// Monitor index (0-based, matching monitor order).
    pub monitor: u32,
    /// Path to wallpaper image file for this monitor.
    #[serde(default)]
    pub path: String,
    /// Wallpaper display mode for this monitor (defaults to global wallpaper_mode).
    #[serde(default)]
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WallpaperTagConfig {
    /// Tag index (0-based). Matches when (active_tags & (1 << tag)) != 0.
    pub tag: u32,
    /// Monitor index (0-based). Use -1 to match any monitor.
    #[serde(default = "default_wallpaper_tag_monitor")]
    pub monitor: i32,
    /// Path to wallpaper image file for this tag.
    #[serde(default)]
    pub path: String,
    /// Wallpaper display mode for this tag (defaults to global wallpaper_mode).
    #[serde(default)]
    pub mode: String,
}

fn default_wallpaper_tag_monitor() -> i32 {
    -1
}

fn default_corner_radius() -> f32 {
    10.0
}
fn default_true() -> bool {
    true
}
fn default_shadow_radius() -> f32 {
    24.0
}
fn default_shadow_offset() -> [f32; 2] {
    [4.0, 4.0]
}
fn default_shadow_color() -> [f32; 4] {
    [0.0, 0.0, 0.0, 0.5]
}
fn default_inactive_opacity() -> f32 {
    0.98
}
fn default_active_opacity() -> f32 {
    1.0
}
fn default_blur_strength() -> u32 {
    3
}
/// Frosted glass is the default: an untouched config gets the flagship look,
/// and both compositors fall back to flat fills wherever a blur chain cannot
/// be built.
fn default_ui_theme() -> String {
    "glass".to_string()
}
fn default_fade_step() -> f32 {
    0.03
}
fn default_border_width() -> f32 {
    2.0
}
fn default_border_color_focused() -> [f32; 4] {
    [0.4, 0.6, 0.9, 1.0]
}
fn default_border_color_unfocused() -> [f32; 4] {
    [0.3, 0.3, 0.3, 0.6]
}
fn default_border_glow_radius() -> f32 {
    28.0
}
fn default_border_glow_intensity() -> f32 {
    1.0
}
fn default_border_glow_color() -> [f32; 4] {
    [0.0, 0.55, 1.0, 0.38]
}
fn default_border_gradient_color_a() -> [f32; 4] {
    [0.24, 0.65, 1.0, 1.0]
}
fn default_border_gradient_color_b() -> [f32; 4] {
    [0.72, 0.35, 1.0, 1.0]
}
fn default_border_gradient_angle() -> f32 {
    45.0
}
fn default_inactive_desaturate() -> f32 {
    0.25
}
fn default_shadow_inactive_opacity() -> f32 {
    0.65
}
fn default_one() -> f32 {
    1.0
}
fn default_shadow_bottom_extra() -> f32 {
    4.0
}
fn default_transition_mode() -> String {
    "coverflow".to_string()
}
fn default_compositor_api() -> String {
    "egl".to_string()
}
fn default_vsync_method() -> String {
    "global".to_string()
}
fn default_new_client_position() -> String {
    "master".to_string()
}
fn default_drag_threshold_px() -> u32 {
    12
}
fn default_client_moveresize() -> String {
    "floating-only".to_string()
}
fn default_audio_buffer_latency() -> u32 {
    50
}
fn default_vrr_min_fps() -> u32 {
    30
}
fn default_vrr_max_fps() -> u32 {
    240
}
fn default_hdr_peak_nits() -> f32 {
    400.0 // Conservative HDR400 baseline
}
fn default_tone_mapping_method() -> String {
    "aces".to_string() // ACES filmic tone mapping (best quality)
}
fn default_temporal_blur_ratio() -> f32 {
    0.8 // 80% previous frame + 20% new
}
fn default_blur_strength_by_hz() -> String {
    // Default: 60Hz→2, 75Hz→2.5, 90Hz→3, 120Hz→3.5, 144Hz→4
    "60:2,75:2.5,90:3,120:3.5,144:4".to_string()
}
fn default_blur_quality_by_monitor() -> String {
    // Default: primary=Full, others=Reduced (can be overridden per-monitor)
    "".to_string()
}
fn default_window_animation_scale() -> f32 {
    0.85
}
fn default_edge_glow_color() -> [f32; 4] {
    [0.3, 0.5, 1.0, 0.6]
}
fn default_edge_glow_width() -> f32 {
    50.0
}
fn default_attention_color() -> [f32; 4] {
    [1.0, 0.4, 0.1, 1.0]
}
fn default_pip_border_color() -> [f32; 4] {
    [0.0, 0.8, 1.0, 0.8]
}
fn default_pip_border_width() -> f32 {
    3.0
}
fn default_night_light_temp() -> f32 {
    0.4
}
fn default_night_light_start() -> String {
    "20:00".to_string()
}
fn default_night_light_end() -> String {
    "06:00".to_string()
}
fn default_night_light_transition() -> u32 {
    30
}
fn default_clipboard_history() -> bool {
    true
}
fn default_idle_dim_secs() -> u64 {
    120
}
fn default_idle_dim_level() -> f32 {
    crate::jwm::features::idle::DEFAULT_DIM_LEVEL
}
fn default_suspend_command() -> String {
    "systemctl suspend".to_string()
}
fn default_hibernate_command() -> String {
    "systemctl hibernate".to_string()
}
fn default_reboot_command() -> String {
    "systemctl reboot".to_string()
}
fn default_shutdown_command() -> String {
    "systemctl poweroff".to_string()
}
fn default_magnifier_radius() -> f32 {
    100.0
}
fn default_magnifier_zoom() -> f32 {
    2.0
}
fn default_tilt_amount() -> f32 {
    0.26
}
fn default_tilt_perspective() -> f32 {
    800.0
}
fn default_tilt_speed() -> f32 {
    12.0
}
fn default_tilt_grid() -> u32 {
    8
}
fn default_frosted_glass_strength() -> u32 {
    2
}
fn default_overview_gap() -> f32 {
    20.0
}
fn default_wobbly_stiffness() -> f32 {
    400.0
}
fn default_wobbly_damping() -> f32 {
    25.0
}
fn default_wobbly_restore_stiffness() -> f32 {
    200.0
}
fn default_wobbly_grid_size() -> u32 {
    8
}
fn default_particle_count() -> u32 {
    150
}
fn default_particle_lifetime() -> f32 {
    0.8
}
fn default_particle_gravity() -> f32 {
    800.0
}
fn default_wallpaper_mode() -> String {
    "fill".to_string()
}
fn default_annotation_color() -> [f32; 4] {
    [1.0, 0.0, 0.0, 1.0]
}
fn default_annotation_line_width() -> f32 {
    3.0
}
fn default_recording_fps() -> u32 {
    30
}
fn default_recording_bitrate() -> String {
    "20M".to_string()
}
fn default_recording_encoder() -> String {
    "auto".to_string()
}
fn default_recording_quality() -> u32 {
    23
}
fn default_audio_recording_device() -> String {
    "default".to_string()
}
fn default_audio_recording_backend() -> String {
    "auto".to_string()
}
fn default_audio_recording_format() -> String {
    "wav".to_string()
}
fn default_audio_recording_sample_rate() -> u32 {
    48_000
}
fn default_audio_recording_channels() -> u16 {
    1
}
fn default_recording_audio_bitrate() -> String {
    "128k".to_string()
}
fn default_motion_trail_frames() -> u32 {
    5
}
fn default_motion_trail_opacity() -> f32 {
    0.3
}
fn default_genie_duration() -> u64 {
    300
}
fn default_ripple_duration() -> f32 {
    0.6
}
fn default_ripple_amplitude() -> f32 {
    0.015
}
fn default_focus_highlight_color() -> [f32; 4] {
    [0.4, 0.7, 1.0, 0.9]
}
fn default_focus_highlight_duration() -> u64 {
    300
}
fn default_wallpaper_crossfade_duration() -> u64 {
    500
}
fn default_expose_gap() -> f32 {
    20.0
}
fn default_snap_preview_color() -> [f32; 4] {
    [0.3, 0.5, 1.0, 0.3]
}
fn default_snap_animation_duration_ms() -> u64 {
    200
}
fn default_tab_bar_height() -> f32 {
    28.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarConfig {
    pub name: String,
    pub show_bar: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorsConfig {
    pub dark_sea_green1: String,
    pub dark_sea_green2: String,
    pub pale_turquoise1: String,
    pub light_sky_blue1: String,
    pub grey84: String,
    pub cyan: String,
    pub white: String,
    pub black: String,
    pub transparent: u8,
    pub opaque: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub m_fact: f32,
    pub n_master: u32,
    pub tags_length: usize,
    /// Carry each tag's layout across restarts.
    ///
    /// With this on, JWM writes the `[[layout.tags]]` block below back to this
    /// file whenever a tag's layout, master count, master fraction or gap
    /// changes, and reads it again on the next start, so a desktop comes back
    /// arranged the way it was left. Turn it off to keep the file entirely
    /// under your own hand: the entries are then only ever read.
    #[serde(default = "default_true")]
    pub persist_tags: bool,
    /// Per-tag, per-monitor layout state.
    ///
    /// Hand-written entries seed a fresh session; JWM replaces the whole block
    /// when it saves, so comments inside it do not survive — everything else in
    /// the file does.
    ///
    /// An empty list is left out of the file entirely rather than written as
    /// `tags = []`: the saved block is appended as `[[layout.tags]]` tables,
    /// and TOML rejects a document that defines the same key both ways.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<LayoutTagConfig>,
}

/// One tag's layout on one monitor.
///
/// The `layout`/`alt` pair mirrors what a monitor actually holds: the layout
/// in use and the one the tag was on before its last change, which is where
/// `lastlayout` goes back to. A restart lands on `layout`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutTagConfig {
    /// Tag number as the status bar shows it: 1 for the first tag. Index 0 is
    /// the "all tags" view every monitor keeps beside its numbered tags.
    pub tag: usize,
    /// Monitor index (0-based). `-1` matches any monitor, which is what a
    /// hand-written entry usually wants; JWM writes one entry per monitor.
    #[serde(default = "default_layout_tag_monitor")]
    pub monitor: i32,
    /// Layout in use, by name: `tile`, `fibonacci`, `monocle`, `scrolling`, …
    /// An unknown name leaves the tag on the built-in default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub layout: String,
    /// The layout in use before the last change, reached with `lastlayout`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alt: String,
    /// Windows in the master area. Absent means the global `n_master`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_master: Option<u32>,
    /// Share of the screen the master area takes. Absent means the global
    /// `m_fact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m_fact: Option<f32>,
    /// Pixels between tiled windows. Absent means `appearance.gap_px`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<i32>,
}

fn default_layout_tag_monitor() -> i32 {
    -1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationConfig {
    pub enabled: bool,
    pub duration_ms: u64,
    pub easing: String,
    /// Speed mode: "slow", "normal" (default), "fast", "instant".
    /// Multiplies all animation timings (duration, fade steps, transitions).
    #[serde(default = "default_animation_speed")]
    pub speed: String,
}

fn default_animation_speed() -> String {
    "normal".to_string()
}

impl AnimationConfig {
    pub fn default_value() -> Self {
        Self {
            enabled: true,
            duration_ms: 250,
            easing: "ease-out".to_string(),
            speed: "normal".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyBindingsConfig {
    pub modkey: String, // "Mod1", "Mod4", etc.
    pub keys: Vec<KeyConfig>,
    /// Optional two-step chord prefix (e.g. Mod+Space then 'b' for browser).
    /// When `leader_key` is empty, chord support is disabled.
    #[serde(default)]
    pub chord: ChordConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChordConfig {
    /// Modifier(s) for the leader key (e.g. ["Mod4"]). Empty means no modifier.
    #[serde(default)]
    pub leader_modifier: Vec<String>,
    /// Leader key name (e.g. "space"). Empty disables chord mode.
    #[serde(default)]
    pub leader_key: String,
    /// Time in milliseconds the chord stays armed waiting for the second key.
    #[serde(default = "default_chord_timeout")]
    pub timeout_ms: u64,
    /// Sequence bindings: each entry's `key` is the second key after the leader.
    #[serde(default)]
    pub bindings: Vec<KeyConfig>,
}

fn default_chord_timeout() -> u64 {
    1500
}

/// Runtime-ready chord state compiled from `ChordConfig`.
#[derive(Debug, Clone)]
pub struct CompiledChord {
    pub leader: (Mods, KeySym),
    pub timeout: Duration,
    pub bindings: Vec<WMKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyConfig {
    pub modifier: Vec<String>, // ["Mod1", "Shift"]
    pub key: String,           // "Return", "j", "k", etc.
    pub function: String,      // "spawn", "focusstack", etc.
    #[serde(default)]
    pub argument: ArgumentConfig,
}

/// Repeat is policy attached to a binding, not inferred by platform backends
/// from window-manager function addresses.
fn key_function_is_repeatable(function: &str) -> bool {
    matches!(
        function,
        "focusstack"
            | "loopview"
            | "setmfact"
            | "setcfact"
            | "incnmaster"
            | "movestack"
            | "volume_adjust"
            | "brightness_adjust"
            | "cyclelayout"
            | "layout_picker"
            | "scrolling_focus_column"
            | "scrolling_move_column"
            | "scrolling_consume"
            | "scrolling_expel"
            | "scrolling_focus_window"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgumentConfig {
    Int(i32),
    UInt(u32),
    Float(f32),
    String(String),
    StringVec(Vec<String>),
}

impl Default for ArgumentConfig {
    fn default() -> Self {
        ArgumentConfig::Int(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MouseBindingsConfig {
    pub buttons: Vec<ButtonConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonConfig {
    pub click_type: String, //
    pub modifier: Vec<String>,
    pub button: u8, // 1, 2, 3
    pub function: String,
    #[serde(default)]
    pub argument: ArgumentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfig {
    pub class: String,
    pub instance: String,
    pub name: String,
    pub tags: usize,
    pub is_floating: bool,
    pub monitor: i32,
}

#[derive(Clone)]
pub struct Config {
    inner: TomlConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            inner: TomlConfig {
                appearance: AppearanceConfig {
                    border_px: 3,
                    gap_px: 5,
                    snap: 32,
                    system_ui_font: "SauceCodePro Nerd Font Regular 11".to_string(),
                    status_bar_padding: 5,
                    status_bar_height: 42,
                    cursor_theme: String::new(),
                    cursor_size: 0,
                    ui_theme: default_ui_theme(),
                },
                behavior: BehaviorConfig {
                    focus_follows_new_window: false,
                    new_client_position: default_new_client_position(),
                    drag_threshold_px: default_drag_threshold_px(),
                    client_moveresize: default_client_moveresize(),
                    resize_hints: true,
                    lock_fullscreen: true,
                    compositor: true,
                    compositor_api: default_compositor_api(),
                    corner_radius: default_corner_radius(),
                    shadow_enabled: default_true(),
                    shadow_radius: default_shadow_radius(),
                    shadow_offset: default_shadow_offset(),
                    shadow_color: default_shadow_color(),
                    shadow_inactive_opacity: default_shadow_inactive_opacity(),
                    inactive_opacity: default_inactive_opacity(),
                    active_opacity: default_active_opacity(),
                    blur_enabled: true,
                    blur_strength: default_blur_strength(),
                    blur_quality_auto: true,
                    blur_temporal_enabled: default_true(),
                    blur_temporal_mix_ratio: default_temporal_blur_ratio(),
                    blur_strength_by_hz: default_blur_strength_by_hz(),
                    blur_quality_by_monitor: default_blur_quality_by_monitor(),
                    fading: true,
                    fade_in_step: default_fade_step(),
                    fade_out_step: default_fade_step(),
                    shadow_exclude: Vec::new(),
                    opacity_rules: Vec::new(),
                    blur_exclude: Vec::new(),
                    blur_status_bar: default_true(),
                    rounded_corners_exclude: Vec::new(),
                    detect_client_opacity: true,
                    fullscreen_unredirect: true,
                    vsync_method: default_vsync_method(),
                    enable_audio_sync: true,
                    audio_buffer_latency_ms: default_audio_buffer_latency(),
                    present_enabled: true,
                    vrr_enabled: default_true(),
                    vrr_min_fps: default_vrr_min_fps(),
                    vrr_max_fps: default_vrr_max_fps(),
                    game_classes: Vec::new(),
                    wlr_output_mgmt_allow_modeset: false,
                    wayland_enable_screencopy: true,
                    wayland_enable_tearing_control: true,
                    wayland_enable_color_management: false,
                    wayland_enable_output_management: true,
                    wayland_enable_output_power: true,
                    wayland_enable_workspace: true,
                    wayland_enable_image_copy_capture: true,
                    wayland_enable_gamma_control: true,
                    wayland_enable_foreign_toplevel_management: true,
                    wayland_enable_virtual_pointer: true,
                    border_enabled: true,
                    border_width: default_border_width(),
                    border_color_focused: default_border_color_focused(),
                    border_color_unfocused: default_border_color_unfocused(),
                    border_glow_enabled: true,
                    border_glow_focused_only: true,
                    border_glow_radius: default_border_glow_radius(),
                    border_glow_intensity: default_border_glow_intensity(),
                    border_glow_color: default_border_glow_color(),
                    border_glow_include: Vec::new(),
                    border_glow_exclude: Vec::new(),
                    border_gradient_enabled: true,
                    border_gradient_color_a: default_border_gradient_color_a(),
                    border_gradient_color_b: default_border_gradient_color_b(),
                    border_gradient_angle: default_border_gradient_angle(),
                    border_gradient_speed: 0.0,
                    corner_radius_rules: Vec::new(),
                    scale_rules: Vec::new(),
                    color_temperature: 0.0,
                    saturation: default_one(),
                    brightness: default_one(),
                    contrast: default_one(),
                    invert_colors: false,
                    grayscale: false,
                    hdr_enabled: false, // Disabled by default (requires HDR display)
                    hdr_peak_nits: default_hdr_peak_nits(),
                    tone_mapping_method: default_tone_mapping_method(),
                    color_management_render_path: false,
                    scene_linear_compositing: false,
                    kms_color_pipeline_offload: false,
                    debug_hud: false,
                    profiling_enabled: false,
                    direct_scanout_enabled: default_true(),
                    gl_state_tracking_enabled: default_true(),
                    blur_use_frame_extents: false,
                    shadow_bottom_extra: default_shadow_bottom_extra(),
                    transition_mode: default_transition_mode(),
                    window_animation: false,
                    window_animation_scale: default_window_animation_scale(),
                    inactive_dim: default_one(),
                    inactive_desaturate: default_inactive_desaturate(),
                    edge_glow: false,
                    edge_glow_color: default_edge_glow_color(),
                    edge_glow_width: default_edge_glow_width(),
                    attention_animation: true,
                    attention_color: default_attention_color(),
                    pip_border_color: default_pip_border_color(),
                    pip_border_width: default_pip_border_width(),
                    night_light: false,
                    night_light_temp: default_night_light_temp(),
                    night_light_start: default_night_light_start(),
                    night_light_end: default_night_light_end(),
                    night_light_transition_mins: default_night_light_transition(),
                    clipboard_history: default_clipboard_history(),
                    layout_picker: true,
                    idle_dim_secs: default_idle_dim_secs(),
                    idle_dim_level: default_idle_dim_level(),
                    idle_lock_secs: 0,
                    idle_screen_off_secs: 0,
                    idle_screen_off_command: String::new(),
                    idle_screen_on_command: String::new(),
                    resource_rows: true,
                    wallpaper_dir: String::new(),
                    wallpaper_colors: true,
                    suspend_command: default_suspend_command(),
                    hibernate_command: default_hibernate_command(),
                    reboot_command: default_reboot_command(),
                    shutdown_command: default_shutdown_command(),
                    magnifier_enabled: false,
                    magnifier_radius: default_magnifier_radius(),
                    magnifier_zoom: default_magnifier_zoom(),
                    window_tilt: false,
                    tilt_amount: default_tilt_amount(),
                    tilt_perspective: default_tilt_perspective(),
                    tilt_speed: default_tilt_speed(),
                    tilt_grid: default_tilt_grid(),
                    frosted_glass_rules: Vec::new(),
                    frosted_glass_strength: default_frosted_glass_strength(),
                    overview_enabled: default_true(),
                    overview_thumbnail_gap: default_overview_gap(),
                    wobbly_windows: false,
                    wobbly_stiffness: default_wobbly_stiffness(),
                    wobbly_damping: default_wobbly_damping(),
                    wobbly_restore_stiffness: default_wobbly_restore_stiffness(),
                    wobbly_grid_size: default_wobbly_grid_size(),
                    particle_effects: false,
                    particle_count: default_particle_count(),
                    particle_lifetime: default_particle_lifetime(),
                    particle_gravity: default_particle_gravity(),
                    expose_enabled: default_true(),
                    expose_gap: default_expose_gap(),
                    snap_preview: default_true(),
                    snap_preview_color: default_snap_preview_color(),
                    snap_animation_duration_ms: default_snap_animation_duration_ms(),
                    peek_enabled: default_true(),
                    peek_exclude: Vec::new(),
                    window_tabs: true,
                    tab_bar_height: default_tab_bar_height(),
                    // Phase 3: Visual effects
                    motion_trail: true,
                    motion_trail_frames: default_motion_trail_frames(),
                    motion_trail_opacity: default_motion_trail_opacity(),
                    genie_minimize: true,
                    genie_duration_ms: default_genie_duration(),
                    ripple_on_open: false,
                    ripple_duration: default_ripple_duration(),
                    ripple_amplitude: default_ripple_amplitude(),
                    focus_highlight: true,
                    focus_highlight_color: default_focus_highlight_color(),
                    focus_highlight_duration_ms: default_focus_highlight_duration(),
                    wallpaper_crossfade: default_true(),
                    wallpaper_crossfade_duration_ms: default_wallpaper_crossfade_duration(),
                    wallpaper: dirs::config_dir()
                        .unwrap_or_default()
                        .join("jwm")
                        .join("wallpaper.jpg")
                        .to_string_lossy()
                        .into_owned(),
                    wallpaper_mode: default_wallpaper_mode(),
                    wallpaper_monitors: Vec::new(),
                    wallpaper_tags: Vec::new(),
                    swallow_enabled: true,
                    swallow_terminals: Vec::new(),
                    swallow_exceptions: Vec::new(),
                    scrolling_column_width_rules: Vec::new(),
                    gesture_swipe: Vec::new(),
                    gesture_swipe_threshold: default_gesture_swipe_threshold(),
                    do_not_disturb: false,
                    // Phase 6: Accessibility
                    colorblind_mode: String::new(),
                    annotation_color: default_annotation_color(),
                    annotation_line_width: default_annotation_line_width(),
                    screenshot_freeze_enabled: true,
                    // Phase 7: Diagnostics
                    shader_hot_reload: false,
                    shader_dir: String::new(),
                    debug_hud_extended: false,
                    recording_fps: default_recording_fps(),
                    recording_bitrate: default_recording_bitrate(),
                    recording_quality: default_recording_quality(),
                    recording_encoder: default_recording_encoder(),
                    recording_output_dir: String::new(),
                    recording_audio_enabled: true,
                    recording_audio_device: default_audio_recording_device(),
                    recording_audio_bitrate: default_recording_audio_bitrate(),
                    audio_recording_device: default_audio_recording_device(),
                    audio_recording_backend: default_audio_recording_backend(),
                    audio_recording_format: default_audio_recording_format(),
                    audio_recording_bitrate: default_recording_audio_bitrate(),
                    audio_recording_output_dir: String::new(),
                    audio_recording_sample_rate: default_audio_recording_sample_rate(),
                    audio_recording_channels: default_audio_recording_channels(),
                },
                status_bar: StatusBarConfig {
                    name: STATUS_BAR_NAME.to_string(),
                    show_bar: true,
                },
                colors: ColorsConfig {
                    dark_sea_green1: "#afffd7".to_string(),
                    dark_sea_green2: "#afffaf".to_string(),
                    pale_turquoise1: "#afffff".to_string(),
                    light_sky_blue1: "#afd7ff".to_string(),
                    grey84: "#d7d7d7".to_string(),
                    cyan: "#00ffd7".to_string(),
                    black: "#000000".to_string(),
                    white: "#ffffff".to_string(),
                    transparent: 0,
                    opaque: 255,
                },
                layout: LayoutConfig {
                    m_fact: 0.55,
                    n_master: 1,
                    tags_length: 9,
                    persist_tags: true,
                    tags: Vec::new(),
                },
                animation: AnimationConfig::default_value(),
                keybindings: KeyBindingsConfig {
                    modkey: "Mod1".to_string(),
                    keys: Self::get_default_keys(),
                    chord: ChordConfig::default(),
                },
                mouse_bindings: MouseBindingsConfig {
                    buttons: Self::get_default_button_configs(),
                },
                rules: Self::get_default_rules(),
            },
        }
    }
}

#[allow(dead_code)]
impl Config {
    fn get_default_keys() -> Vec<KeyConfig> {
        vec![
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "e".to_string(),
                function: "toggle_expose".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "p".to_string(),
                function: "toggle_peek".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "r".to_string(),
                function: "toggle_recording".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec![
                    "Mod1".to_string(),
                    "Control".to_string(),
                    "Shift".to_string(),
                ],
                key: "r".to_string(),
                function: "adjust_recording_region".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "m".to_string(),
                function: "toggle_audio_recording".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "o".to_string(),
                function: "monitor_layout".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "r".to_string(),
                function: "app_launcher".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "Escape".to_string(),
                function: "lock_screen".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            // Volume / brightness OSD on the dedicated media keys, plus a
            // DMS/Noctalia-style control center.
            KeyConfig {
                modifier: vec![],
                key: "XF86AudioRaiseVolume".to_string(),
                function: "volume_adjust".to_string(),
                argument: ArgumentConfig::Int(5),
            },
            KeyConfig {
                modifier: vec![],
                key: "XF86AudioLowerVolume".to_string(),
                function: "volume_adjust".to_string(),
                argument: ArgumentConfig::Int(-5),
            },
            KeyConfig {
                modifier: vec![],
                key: "XF86AudioMute".to_string(),
                function: "volume_mute".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec![],
                key: "XF86MonBrightnessUp".to_string(),
                function: "brightness_adjust".to_string(),
                argument: ArgumentConfig::Int(5),
            },
            KeyConfig {
                modifier: vec![],
                key: "XF86MonBrightnessDown".to_string(),
                function: "brightness_adjust".to_string(),
                argument: ArgumentConfig::Int(-5),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "F10".to_string(),
                function: "control_center".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "F11".to_string(),
                function: "notification_center".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "F12".to_string(),
                function: "wifi_picker".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "F12".to_string(),
                function: "bluetooth_picker".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "F9".to_string(),
                function: "calendar".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "w".to_string(),
                function: "wallpaper_picker".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "v".to_string(),
                function: "clipboard_picker".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "Escape".to_string(),
                function: "session_menu".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec![],
                key: "XF86AudioPlay".to_string(),
                function: "media_play_pause".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec![],
                key: "XF86AudioNext".to_string(),
                function: "media_next".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec![],
                key: "XF86AudioPrev".to_string(),
                function: "media_previous".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "Return".to_string(),
                function: "spawn".to_string(),
                argument: ArgumentConfig::StringVec(Self::get_termcmd()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "b".to_string(),
                function: "togglebar".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "b".to_string(),
                function: "setgaps".to_string(),
                argument: ArgumentConfig::Int(5),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "b".to_string(),
                function: "setgaps".to_string(),
                argument: ArgumentConfig::Int(-5),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "x".to_string(),
                function: "togglecompositor".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "d".to_string(),
                function: "togglepartialdamage".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "F12".to_string(),
                function: "toggle_debug_hud".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "F11".to_string(),
                function: "toggle_waterlily".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "F10".to_string(),
                function: "waterlily_case".to_string(),
                argument: ArgumentConfig::StringVec(vec!["next".to_string()]),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "F9".to_string(),
                function: "waterlily_palette".to_string(),
                argument: ArgumentConfig::StringVec(vec!["next".to_string()]),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "a".to_string(),
                function: "toggle_annotation".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "j".to_string(),
                function: "focusstack".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "k".to_string(),
                function: "focusstack".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "i".to_string(),
                function: "incnmaster".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "d".to_string(),
                function: "incnmaster".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "h".to_string(),
                function: "setmfact".to_string(),
                argument: ArgumentConfig::Float(-0.025),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "l".to_string(),
                function: "setmfact".to_string(),
                argument: ArgumentConfig::Float(0.025),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "h".to_string(),
                function: "setcfact".to_string(),
                argument: ArgumentConfig::Float(0.2),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "l".to_string(),
                function: "setcfact".to_string(),
                argument: ArgumentConfig::Float(-0.2),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "o".to_string(),
                function: "setcfact".to_string(),
                argument: ArgumentConfig::Float(0.0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "j".to_string(),
                function: "movestack".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "k".to_string(),
                function: "movestack".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "Return".to_string(),
                function: "zoom".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "Tab".to_string(),
                function: "toggle_overview".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "Tab".to_string(),
                function: "loopview".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "Tab".to_string(),
                function: "loopview".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "Page_Up".to_string(),
                function: "loopview".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "Page_Down".to_string(),
                function: "loopview".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "c".to_string(),
                function: "killclient".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "n".to_string(),
                function: "minimize".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "t".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("tile".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "t".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("fibonacci".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "f".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("float".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "m".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("monocle".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "u".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("centeredmaster".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "u".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("bstack".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "g".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("grid".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "g".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("deck".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "y".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("threecol".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "y".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("tatami".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "slash".to_string(),
                function: "show_keybindings".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "space".to_string(),
                function: "cyclelayout".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "space".to_string(),
                function: "cyclelayout".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "grave".to_string(),
                function: "lastlayout".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "f".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("fullscreen".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "0".to_string(),
                function: "view".to_string(),
                argument: ArgumentConfig::UInt(!0), // 所有标签
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "0".to_string(),
                function: "tag".to_string(),
                argument: ArgumentConfig::UInt(!0), // 所有标签
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "comma".to_string(),
                function: "focusmon".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "period".to_string(),
                function: "focusmon".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "s".to_string(),
                function: "take_screenshot".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "s".to_string(),
                function: "take_screenshot_fullscreen".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "comma".to_string(),
                function: "tagmon".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "period".to_string(),
                function: "tagmon".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "n".to_string(),
                function: "togglescratchpad".to_string(),
                argument: ArgumentConfig::StringVec(vec!["term".to_string()]),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "m".to_string(),
                function: "togglescratchpad".to_string(),
                argument: ArgumentConfig::StringVec(vec![
                    "music".to_string(),
                    "spotify".to_string(),
                ]),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "c".to_string(),
                function: "togglescratchpad".to_string(),
                argument: ArgumentConfig::StringVec(vec![
                    "calc".to_string(),
                    "qalculate-gtk".to_string(),
                ]),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "s".to_string(),
                function: "togglesticky".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "p".to_string(),
                function: "togglepip".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "w".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("scrolling".to_string()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "v".to_string(),
                function: "setlayout".to_string(),
                argument: ArgumentConfig::String("vstack".to_string()),
            },
            // Scrolling layout: consume/expel
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "a".to_string(),
                function: "scrolling_toggle_attach_mode".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "h".to_string(),
                function: "scrolling_consume".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "l".to_string(),
                function: "scrolling_consume".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec![
                    "Mod1".to_string(),
                    "Control".to_string(),
                    "Shift".to_string(),
                ],
                key: "h".to_string(),
                function: "scrolling_expel".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec![
                    "Mod1".to_string(),
                    "Control".to_string(),
                    "Shift".to_string(),
                ],
                key: "l".to_string(),
                function: "scrolling_expel".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "q".to_string(),
                function: "quit".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "r".to_string(),
                function: "restart".to_string(),
                argument: ArgumentConfig::Int(0),
            },
        ]
    }

    fn get_default_button_configs() -> Vec<ButtonConfig> {
        vec![
            ButtonConfig {
                click_type: "ClkClientWin".to_string(),
                modifier: vec!["Mod1".to_string()],
                button: 1, // 左键
                function: "movemouse".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            ButtonConfig {
                click_type: "ClkClientWin".to_string(),
                modifier: vec!["Mod1".to_string()],
                button: 2, // 中键
                function: "togglefloating".to_string(),
                argument: ArgumentConfig::Int(0),
            },
            ButtonConfig {
                click_type: "ClkClientWin".to_string(),
                modifier: vec!["Mod1".to_string()],
                button: 3, // 右键
                function: "resizemouse".to_string(),
                argument: ArgumentConfig::Int(0),
            },
        ]
    }

    fn get_default_rules() -> Vec<RuleConfig> {
        vec![]
    }

    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let config: TomlConfig = toml::from_str(&content)?;
        let cfg = Self { inner: config };
        let diagnostics = cfg.diagnostics();
        if diagnostics.has_errors() {
            return Err(ConfigError::Validation(diagnostics));
        }
        if !diagnostics.is_empty() {
            log::warn!("[config] {diagnostics}");
        }
        Ok(cfg)
    }

    pub fn load_default() -> Self {
        let path = Self::resolve_load_path();
        match Self::load_from_file(&path) {
            Ok(config) => {
                println!("Configuration loaded from: {}", path.display());
                config
            }
            Err(error) => {
                eprintln!(
                    "Failed to load configuration from {}: {error}; using built-in defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn key_configs(&self) -> &[KeyConfig] {
        &self.inner.keybindings.keys
    }

    pub fn border_px(&self) -> u32 {
        self.inner.appearance.border_px
    }

    pub fn gap_px(&self) -> u32 {
        self.inner.appearance.gap_px
    }

    pub fn snap(&self) -> u32 {
        self.inner.appearance.snap
    }

    pub fn status_bar_padding(&self) -> i32 {
        self.inner.appearance.status_bar_padding
    }

    pub fn status_bar_height(&self) -> i32 {
        self.inner.appearance.status_bar_height
    }

    pub fn system_ui_font(&self) -> &str {
        &self.inner.appearance.system_ui_font
    }

    /// Configured Xcursor theme name, or "" to defer to the environment.
    /// See [`Config::resolved_cursor`] for the value backends should use.
    pub fn cursor_theme(&self) -> &str {
        &self.inner.appearance.cursor_theme
    }

    /// Configured pointer size in pixels, or 0 to defer to the environment.
    /// See [`Config::resolved_cursor`] for the value backends should use.
    pub fn cursor_size(&self) -> u32 {
        self.inner.appearance.cursor_size
    }

    /// Design language for JWM's own overlays: `"material"` or `"glass"`.
    pub fn ui_theme(&self) -> &str {
        &self.inner.appearance.ui_theme
    }

    /// Resolve the effective cursor theme/size a rendering backend should use.
    ///
    /// Precedence: the `[appearance]` config values win when set; otherwise the
    /// `XCURSOR_THEME` / `XCURSOR_SIZE` environment variables are honored (for
    /// compatibility with existing sessions); otherwise the built-in defaults
    /// ("default", 24px) apply. Sizes outside 1..=512 are rejected for the
    /// environment and clamped for already-loaded configuration so backends
    /// that use signed dimensions can never observe an overflowed value.
    pub fn resolved_cursor(&self) -> (String, u32) {
        let theme = {
            let configured = &self.inner.appearance.cursor_theme;
            if configured.trim().is_empty() {
                std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into())
            } else {
                configured.clone()
            }
        };
        let size = resolve_cursor_size(
            self.inner.appearance.cursor_size,
            std::env::var_os("XCURSOR_SIZE").as_deref(),
        );
        (theme, size)
    }

    pub fn show_bar(&self) -> bool {
        self.inner.status_bar.show_bar
    }

    pub fn status_bar_name(&self) -> &str {
        &self.inner.status_bar.name
    }

    pub fn status_bar_config(&self) -> &StatusBarConfig {
        &self.inner.status_bar
    }

    pub fn colors(&self) -> &ColorsConfig {
        &self.inner.colors
    }

    pub fn behavior(&self) -> &BehaviorConfig {
        &self.inner.behavior
    }

    pub fn drag_threshold_px(&self) -> u32 {
        self.inner.behavior.drag_threshold_px
    }

    pub fn client_moveresize(&self) -> ClientMoveResize {
        ClientMoveResize::from_str(&self.inner.behavior.client_moveresize)
    }

    pub fn new_client_position(&self) -> NewClientPosition {
        NewClientPosition::from_str(&self.inner.behavior.new_client_position)
    }

    pub fn compositor_enabled(&self) -> bool {
        self.inner.behavior.compositor
    }

    pub fn m_fact(&self) -> f32 {
        self.inner.layout.m_fact
    }

    pub fn n_master(&self) -> u32 {
        self.inner.layout.n_master
    }

    pub fn tags_length(&self) -> usize {
        // Tag masks are built with `1u32 << tag`, so a value >= 32 (or 0) from a
        // malformed config would shift-overflow / produce empty masks. Clamp to a
        // usable range so every downstream `1 << i` and `(1 << n) - 1` stays sound.
        self.inner.layout.tags_length.clamp(1, 31)
    }

    pub fn tagmask(&self) -> u32 {
        (1 << self.tags_length()) - 1
    }

    /// Whether a layout change is written back to the config file.
    pub fn layout_persist_tags(&self) -> bool {
        self.inner.layout.persist_tags
    }

    /// Every stored per-tag layout, in file order.
    pub fn layout_tags(&self) -> &[LayoutTagConfig] {
        &self.inner.layout.tags
    }

    /// The stored layout for `tag` on monitor `monitor`.
    ///
    /// A monitor-specific entry wins over an any-monitor one, so the usual
    /// hand-written `monitor = -1` line is a default the saved per-monitor
    /// entries then refine.
    pub fn layout_for_tag(&self, monitor: i32, tag: usize) -> Option<&LayoutTagConfig> {
        let matching = |entry: &&LayoutTagConfig| entry.tag == tag;
        self.inner
            .layout
            .tags
            .iter()
            .filter(matching)
            .find(|entry| entry.monitor == monitor)
            .or_else(|| {
                self.inner
                    .layout
                    .tags
                    .iter()
                    .filter(matching)
                    .find(|entry| entry.monitor < 0)
            })
    }

    /// Replace the stored per-tag layouts in memory. Writing them to disk is
    /// [`Self::persist_layout_tags`].
    pub fn set_layout_tags(&mut self, tags: Vec<LayoutTagConfig>) {
        self.inner.layout.tags = tags;
    }

    pub fn animation_enabled(&self) -> bool {
        self.inner.animation.enabled
    }

    pub fn animation_speed(&self) -> AnimationSpeed {
        AnimationSpeed::from_str(&self.inner.animation.speed)
    }

    pub fn animation_duration(&self) -> Duration {
        let speed = self.animation_speed();
        let base_ms = self.inner.animation.duration_ms;
        Duration::from_millis(speed.apply_duration(base_ms))
    }

    pub fn animation_easing(&self) -> Easing {
        Easing::from_str(&self.inner.animation.easing)
    }

    /// Compile the chord configuration into a runtime-ready structure.
    /// Returns `None` when chord support is disabled or the leader is unparseable.
    pub fn compile_chord(&self) -> Option<CompiledChord> {
        let chord = &self.inner.keybindings.chord;
        if chord.leader_key.is_empty() {
            return None;
        }
        let leader_mods = self.parse_modifiers(&chord.leader_modifier);
        let leader_sym = self.parse_keysym(&chord.leader_key)?;
        let mut bindings = Vec::with_capacity(chord.bindings.len());
        for kc in &chord.bindings {
            if let Some(wmkey) = self.convert_key_config(kc) {
                bindings.push(wmkey);
            }
        }
        Some(CompiledChord {
            leader: (leader_mods, leader_sym),
            timeout: Duration::from_millis(chord.timeout_ms.max(100)),
            bindings,
        })
    }

    pub fn get_keys(&self) -> Vec<WMKey> {
        let mut keys = Vec::new();

        let chord_is_occupied = |modifiers: Mods, key: &str| {
            self.inner.keybindings.keys.iter().any(|binding| {
                binding.key == key && self.parse_modifiers(&binding.modifier) == modifiers
            })
        };
        let calc_migration_available = !chord_is_occupied(Mods::ALT | Mods::CONTROL, "c");
        let sticky_migration_available = !chord_is_occupied(Mods::ALT | Mods::CONTROL, "s");

        for key_config in &self.inner.keybindings.keys {
            // Templates generated before the safe-control-plane upgrade used
            // Alt+Shift+C/S twice. Keep those existing files functional in
            // memory while `--check-config` points users at the explicit new
            // chords written by current templates.
            let migrated = if calc_migration_available
                && key_config.function == "togglescratchpad"
                && key_config.key == "c"
                && self.parse_modifiers(&key_config.modifier) == (Mods::ALT | Mods::SHIFT)
                && matches!(
                    &key_config.argument,
                    ArgumentConfig::StringVec(command)
                        if command.first().is_some_and(|name| name == "calc")
                ) {
                log::info!(
                    "[config] remapping legacy calculator shortcut from Alt+Shift+C to Alt+Ctrl+C"
                );
                Some(KeyConfig {
                    modifier: vec!["Mod1".into(), "Control".into()],
                    key: key_config.key.clone(),
                    function: key_config.function.clone(),
                    argument: key_config.argument.clone(),
                })
            } else if sticky_migration_available
                && key_config.function == "togglesticky"
                && key_config.key == "s"
                && self.parse_modifiers(&key_config.modifier) == (Mods::ALT | Mods::SHIFT)
            {
                log::info!(
                    "[config] remapping legacy sticky shortcut from Alt+Shift+S to Alt+Ctrl+S"
                );
                Some(KeyConfig {
                    modifier: vec!["Mod1".into(), "Control".into()],
                    key: key_config.key.clone(),
                    function: key_config.function.clone(),
                    argument: key_config.argument.clone(),
                })
            } else {
                None
            };
            let effective = migrated.as_ref().unwrap_or(key_config);
            if let Some(key) = self.convert_key_config(effective) {
                keys.push(key);
            }
        }

        // Existing config files contain a full snapshot of the key list, so
        // newly introduced defaults are not picked up automatically. Add the
        // recorder binding only when the user has neither configured the
        // action nor occupied its fallback chord.
        if !self
            .inner
            .keybindings
            .keys
            .iter()
            .any(|key| key.function == "toggle_audio_recording")
        {
            let fallback = KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "m".to_string(),
                function: "toggle_audio_recording".to_string(),
                argument: ArgumentConfig::Int(0),
            };
            if let Some(binding) = self.convert_key_config(&fallback) {
                let occupied = keys
                    .iter()
                    .any(|key| key.mask == binding.mask && key.key_sym == binding.key_sym);
                if occupied {
                    log::warn!(
                        "[config] built-in audio recorder has no shortcut: Alt+Ctrl+M is already occupied"
                    );
                } else {
                    log::info!(
                        "[config] legacy key list detected; enabling audio recorder on Alt+Ctrl+M"
                    );
                    keys.push(binding);
                }
            }
        }
        // Keep the built-in display configurator reachable for configurations
        // generated before the action was introduced.
        if !self
            .inner
            .keybindings
            .keys
            .iter()
            .any(|key| key.function == "monitor_layout")
        {
            let fallback = KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string()],
                key: "o".to_string(),
                function: "monitor_layout".to_string(),
                argument: ArgumentConfig::Int(0),
            };
            if let Some(binding) = self.convert_key_config(&fallback) {
                let occupied = keys
                    .iter()
                    .any(|key| key.mask == binding.mask && key.key_sym == binding.key_sym);
                if occupied {
                    log::warn!(
                        "[config] display layout has no shortcut: Alt+Ctrl+O is already occupied"
                    );
                } else {
                    log::info!(
                        "[config] legacy key list detected; enabling display layout on Alt+Ctrl+O"
                    );
                    keys.push(binding);
                }
            }
        }
        for i in 0..self.tags_length() {
            keys.extend(self.generate_tag_keys(i));
        }
        keys
    }

    pub fn get_rules(&self) -> Vec<WMRule> {
        self.inner
            .rules
            .iter()
            .map(|rule| {
                WMRule::new(
                    rule.class.clone(),
                    rule.instance.clone(),
                    rule.name.clone(),
                    rule.tags,
                    rule.is_floating,
                    rule.monitor,
                )
            })
            .collect()
    }

    pub fn get_termcmd() -> Vec<String> {
        if let Some(command) = terminal_override_from_env("JWM_TERMINAL") {
            return command;
        }
        ADVANCED_TERMINAL_PROBER
            .get_available_terminal()
            .map_or_else(
                || {
                    log::warn!("no supported terminal found; falling back to frost");
                    vec!["frost".to_string()]
                },
                |config| vec![config.command.clone()],
            )
    }

    /// Terminal argv prefix for running a child command from a
    /// `Terminal=true` desktop entry.
    ///
    /// Unlike [`Self::get_termcmd`], this includes the selected terminal's
    /// execution delimiter and skips interactive-only terminals such as frost
    /// and ember.
    pub fn get_terminal_exec_prefix() -> Vec<String> {
        if let Some(command) = terminal_override_from_env("JWM_TERMINAL") {
            if let Some(prefix) = configured_terminal_execution_prefix(command) {
                return prefix;
            }
            log::warn!(
                "[config] JWM_TERMINAL names an interactive-only terminal; selecting an execution-capable fallback"
            );
        }
        ADVANCED_TERMINAL_PROBER
            .get_available_terminal_for(TerminalPurpose::Execute)
            .and_then(|config| {
                Some(vec![
                    config.command.clone(),
                    config.execute_flag.clone()?,
                ])
            })
            .unwrap_or_else(|| {
                log::warn!(
                    "no execution-capable terminal found; Terminal=true applications will run directly"
                );
                Vec::new()
            })
    }

    pub fn get_scratchpad_termcmd() -> Vec<String> {
        if let Some(command) = terminal_override_from_env("JWM_SCRATCHPAD_TERMINAL") {
            if let Some(command) = configured_scratchpad_terminal(command) {
                return command;
            }
            log::warn!(
                "[config] JWM_SCRATCHPAD_TERMINAL names a terminal without stable window PID ownership; selecting a compatible fallback"
            );
        }
        // Prefer frost for scratchpad
        ADVANCED_TERMINAL_PROBER
            .get_available_terminal_for_with_priority(TerminalPurpose::Scratchpad, Some("frost"))
            .map_or_else(
                || {
                    log::warn!("no supported scratchpad terminal found; falling back to frost");
                    vec!["frost".to_string()]
                },
                |config| vec![config.command.clone()],
            )
    }

    fn convert_button_config(&self, btn_config: &ButtonConfig) -> Option<WMButton> {
        let click_type = self.parse_click_type(&btn_config.click_type)?;
        let modifiers = self.parse_modifiers(&btn_config.modifier);
        let button = MouseButton::from_u8(btn_config.button as u8);
        let function = self.parse_function(&btn_config.function)?;
        let arg = self.convert_binding_argument(&btn_config.function, &btn_config.argument)?;

        Some(WMButton::new(
            click_type,
            modifiers,
            button,
            Some(function),
            arg,
        ))
    }

    fn parse_click_type(&self, click_type: &str) -> Option<WMClickType> {
        match click_type {
            "ClkClientWin" => Some(WMClickType::ClickClientWin),
            "ClkRootWin" => Some(WMClickType::ClickRootWin),
            _ => {
                eprintln!("Unknown click type: {}", click_type);
                None
            }
        }
    }

    fn parse_function(&self, func_name: &str) -> Option<WMFuncType> {
        match func_name {
            "spawn" => Some(Jwm::spawn),
            "app_launcher" => Some(Jwm::app_launcher),
            "monitor_layout" => Some(Jwm::monitor_layout),
            "lock_screen" => Some(Jwm::lock_screen),
            "focusstack" => Some(Jwm::focusstack),
            "focusmon" => Some(Jwm::focusmon),
            "take_screenshot" => Some(Jwm::take_screenshot),
            "take_screenshot_fullscreen" => Some(Jwm::take_screenshot_fullscreen),
            "quit" => Some(Jwm::quit),
            "restart" => Some(Jwm::restart),
            "killclient" => Some(Jwm::killclient),
            "minimize" => Some(Jwm::minimize),
            "zoom" => Some(Jwm::zoom),

            "setlayout" => Some(Jwm::setlayout),
            "lastlayout" => Some(Jwm::lastlayout),
            "togglefloating" => Some(Jwm::togglefloating),
            "togglebar" => Some(Jwm::togglebar),
            "setmfact" => Some(Jwm::setmfact),
            "setgaps" => Some(Jwm::setgaps),
            "setcfact" => Some(Jwm::setcfact),
            "incnmaster" => Some(Jwm::incnmaster),
            "movestack" => Some(Jwm::movestack),

            "view" => Some(Jwm::view),
            "tag" => Some(Jwm::tag),
            "toggleview" => Some(Jwm::toggleview),
            "toggletag" => Some(Jwm::toggletag),
            "tagmon" => Some(Jwm::tagmon),
            "loopview" => Some(Jwm::loopview),

            "movemouse" => Some(Jwm::movemouse),
            "resizemouse" => Some(Jwm::resizemouse),
            "show_keybindings" => Some(Jwm::show_keybindings),
            "cyclelayout" => Some(Jwm::cyclelayout),
            "layout_picker" => Some(Jwm::layout_picker),
            "togglesticky" => Some(Jwm::togglesticky),
            "togglescratchpad" => Some(Jwm::togglescratchpad),
            "togglepip" => Some(Jwm::togglepip),
            "togglecompositor" => Some(Jwm::togglecompositor),
            "togglepartialdamage" => Some(Jwm::togglepartialdamage),
            "toggle_debug_hud" => Some(Jwm::toggle_debug_hud),
            "toggle_waterlily" => Some(Jwm::toggle_waterlily),
            "waterlily_case" => Some(Jwm::waterlily_case),
            "waterlily_palette" => Some(Jwm::waterlily_palette),
            // Compatibility only: new/default configuration must use the canonical name.
            "toggle_slime" => {
                log::warn!(
                    "config action `toggle_slime` is deprecated; use `toggle_waterlily` instead"
                );
                Some(Jwm::toggle_waterlily)
            }
            "toggle_overview" => Some(Jwm::toggle_overview),
            "cycle_overview" => Some(Jwm::cycle_overview),
            "toggle_magnifier" => Some(Jwm::toggle_magnifier),
            "toggle_peek" => Some(Jwm::toggle_peek),
            "toggle_annotation" => Some(Jwm::toggle_annotation),
            "save_session" => Some(Jwm::save_session),
            "restore_session" => Some(Jwm::restore_session),
            "toggle_expose" => Some(Jwm::toggle_expose),
            "toggle_recording" => Some(Jwm::toggle_recording),
            "volume_adjust" => Some(Jwm::volume_adjust),
            "volume_mute" => Some(Jwm::volume_mute),
            "brightness_adjust" => Some(Jwm::brightness_adjust),
            "control_center" => Some(Jwm::control_center),
            "notification_center" => Some(Jwm::notification_center),
            "media_play_pause" => Some(Jwm::media_play_pause),
            "media_next" => Some(Jwm::media_next),
            "media_previous" => Some(Jwm::media_previous),
            "media_stop" => Some(Jwm::media_stop),
            "session_menu" => Some(Jwm::session_menu),
            "toggle_night_light" => Some(Jwm::toggle_night_light),
            "toggle_idle_inhibit" => Some(Jwm::toggle_idle_inhibit),
            "toggle_wifi" => Some(Jwm::toggle_wifi),
            "wifi_picker" => Some(Jwm::wifi_picker),
            "audio_output_picker" => Some(Jwm::audio_output_picker),
            "audio_input_picker" => Some(Jwm::audio_input_picker),
            "bluetooth_picker" => Some(Jwm::bluetooth_picker),
            "calendar" => Some(Jwm::calendar),
            "clipboard_picker" => Some(Jwm::clipboard_picker),
            "wallpaper_picker" => Some(Jwm::wallpaper_picker),
            "toggle_bluetooth" => Some(Jwm::toggle_bluetooth),
            "adjust_recording_region" => Some(Jwm::adjust_recording_region),
            "toggle_audio_recording" => Some(Jwm::toggle_audio_recording),

            "scrolling_focus_column" => Some(Jwm::scrolling_focus_column),
            "scrolling_move_column" => Some(Jwm::scrolling_move_column),
            "scrolling_consume" => Some(Jwm::scrolling_consume),
            "scrolling_expel" => Some(Jwm::scrolling_expel),
            "scrolling_focus_window" => Some(Jwm::scrolling_focus_window),
            "scrolling_toggle_attach_mode" => Some(Jwm::scrolling_toggle_attach_mode),

            _ => None,
        }
    }

    fn parse_keysym(&self, key: &str) -> Option<KeySym> {
        let ks: KeySym = match key {
            "Return" => k::KEY_Return,
            "Tab" => k::KEY_Tab,
            "space" => k::KEY_space,
            "Page_Up" => k::KEY_Page_Up,
            "Page_Down" => k::KEY_Page_Down,
            "comma" => k::KEY_comma,
            "period" => k::KEY_period,

            "a" => k::KEY_a,
            "b" => k::KEY_b,
            "c" => k::KEY_c,
            "d" => k::KEY_d,
            "e" => k::KEY_e,
            "f" => k::KEY_f,
            "g" => k::KEY_g,
            "h" => k::KEY_h,
            "i" => k::KEY_i,
            "j" => k::KEY_j,
            "k" => k::KEY_k,
            "l" => k::KEY_l,
            "m" => k::KEY_m,
            "n" => k::KEY_n,
            "o" => k::KEY_o,
            "p" => k::KEY_p,
            "q" => k::KEY_q,
            "r" => k::KEY_r,
            "s" => k::KEY_s,
            "t" => k::KEY_t,
            "u" => k::KEY_u,
            "v" => k::KEY_v,
            "w" => k::KEY_w,
            "x" => k::KEY_x,
            "y" => k::KEY_y,
            "z" => k::KEY_z,

            "0" => k::KEY_0,
            "1" => k::KEY_1,
            "2" => k::KEY_2,
            "3" => k::KEY_3,
            "4" => k::KEY_4,
            "5" => k::KEY_5,
            "6" => k::KEY_6,
            "7" => k::KEY_7,
            "8" => k::KEY_8,
            "9" => k::KEY_9,

            "F1" => k::KEY_F1,
            "F2" => k::KEY_F2,
            "F3" => k::KEY_F3,
            "F4" => k::KEY_F4,
            "F5" => k::KEY_F5,
            "F6" => k::KEY_F6,
            "F7" => k::KEY_F7,
            "F8" => k::KEY_F8,
            "F9" => k::KEY_F9,
            "F10" => k::KEY_F10,
            "F11" => k::KEY_F11,
            "F12" => k::KEY_F12,

            "Left" => k::KEY_Left,
            "Right" => k::KEY_Right,
            "Up" => k::KEY_Up,
            "Down" => k::KEY_Down,

            "slash" => k::KEY_slash,
            "question" => k::KEY_question,
            "grave" => k::KEY_grave,

            "Escape" => k::KEY_Escape,
            "BackSpace" => k::KEY_BackSpace,
            "Delete" => k::KEY_Delete,
            "Home" => k::KEY_Home,
            "End" => k::KEY_End,

            // Media keys (dedicated laptop/keyboard function row).
            "XF86AudioRaiseVolume" => k::KEY_XF86AudioRaiseVolume,
            "XF86AudioLowerVolume" => k::KEY_XF86AudioLowerVolume,
            "XF86AudioMute" => k::KEY_XF86AudioMute,
            "XF86AudioPlay" => k::KEY_XF86AudioPlay,
            "XF86AudioPause" => k::KEY_XF86AudioPause,
            "XF86AudioNext" => k::KEY_XF86AudioNext,
            "XF86AudioPrev" => k::KEY_XF86AudioPrev,
            "XF86AudioStop" => k::KEY_XF86AudioStop,
            "XF86MonBrightnessUp" => k::KEY_XF86MonBrightnessUp,
            "XF86MonBrightnessDown" => k::KEY_XF86MonBrightnessDown,
            _ => return None,
        };
        Some(ks)
    }

    fn parse_modifiers(&self, modifiers: &[String]) -> Mods {
        let mut mask = Mods::empty();
        for modifier in modifiers {
            match modifier.as_str() {
                "Mod1" | "Alt" => mask |= Mods::ALT,
                "Mod2" => mask |= Mods::MOD2,
                "Mod3" => mask |= Mods::MOD3,
                "Mod4" | "Super" | "Win" => mask |= Mods::SUPER,
                "Mod5" => mask |= Mods::MOD5,
                "Control" | "Ctrl" => mask |= Mods::CONTROL,
                "Shift" => mask |= Mods::SHIFT,
                "Lock" | "CapsLock" => mask |= Mods::CAPS,
                _ => {
                    eprintln!("Unknown modifier: {}", modifier);
                }
            };
        }
        mask
    }

    fn convert_argument(&self, arg: &ArgumentConfig) -> jwm::WMArgEnum {
        match arg {
            ArgumentConfig::Int(i) => jwm::WMArgEnum::Int(*i),
            ArgumentConfig::UInt(u) => jwm::WMArgEnum::UInt(*u),
            ArgumentConfig::Float(f) => jwm::WMArgEnum::Float(*f),
            ArgumentConfig::StringVec(v) => jwm::WMArgEnum::StringVec(v.clone()),
            // A string argument is a layout name when one goes by it, and a
            // one-element command line otherwise.
            ArgumentConfig::String(s) => match LayoutEnum::from_name(s) {
                Some(layout) => jwm::WMArgEnum::Layout(Rc::new(layout.clone())),
                None => jwm::WMArgEnum::StringVec(vec![s.clone()]),
            },
        }
    }

    fn convert_binding_argument(
        &self,
        function_name: &str,
        argument: &ArgumentConfig,
    ) -> Option<jwm::WMArgEnum> {
        let argument = if function_name == "spawn" {
            let command = match argument {
                ArgumentConfig::String(command) => {
                    crate::command_line::split_command_line(command).ok()?
                }
                ArgumentConfig::StringVec(command) => command.clone(),
                _ => return None,
            };
            if command
                .first()
                .is_none_or(|program| program.trim().is_empty())
            {
                return None;
            }
            jwm::WMArgEnum::StringVec(command)
        } else {
            self.convert_argument(argument)
        };

        Some(migrate_legacy_terminal_argument(function_name, argument))
    }

    pub fn get_buttons(&self) -> Vec<WMButton> {
        let button_configs = if self.inner.mouse_bindings.buttons.is_empty() {
            Self::get_default_button_configs()
        } else {
            self.inner.mouse_bindings.buttons.clone()
        };

        button_configs
            .iter()
            .filter_map(|btn| self.convert_button_config(btn))
            .collect()
    }

    fn convert_key_config(&self, key_config: &KeyConfig) -> Option<WMKey> {
        let modifiers = self.parse_modifiers(&key_config.modifier);
        let keysym = self.parse_keysym(&key_config.key)?;
        let function = self.parse_function(&key_config.function)?;
        let arg = self.convert_binding_argument(&key_config.function, &key_config.argument)?;

        Some(
            WMKey::new(modifiers, keysym, Some(function), arg)
                .with_repeatable(key_function_is_repeatable(&key_config.function)),
        )
    }

    fn generate_tag_keys(&self, tag: usize) -> Vec<WMKey> {
        let key = match tag {
            0 => k::KEY_1,
            1 => k::KEY_2,
            2 => k::KEY_3,
            3 => k::KEY_4,
            4 => k::KEY_5,
            5 => k::KEY_6,
            6 => k::KEY_7,
            7 => k::KEY_8,
            8 => k::KEY_9,
            _ => return vec![],
        };

        let modkey = self.parse_modifiers(std::slice::from_ref(&self.inner.keybindings.modkey));
        vec![
            WMKey::new(modkey, key, Some(Jwm::view), jwm::WMArgEnum::UInt(1 << tag)),
            WMKey::new(
                modkey | Mods::CONTROL,
                key,
                Some(Jwm::toggleview),
                jwm::WMArgEnum::UInt(1 << tag),
            ),
            WMKey::new(
                modkey | Mods::SHIFT,
                key,
                Some(Jwm::tag),
                jwm::WMArgEnum::UInt(1 << tag),
            ),
            WMKey::new(
                modkey | Mods::CONTROL | Mods::SHIFT,
                key,
                Some(Jwm::toggletag),
                jwm::WMArgEnum::UInt(1 << tag),
            ),
        ]
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), ConfigError> {
        let toml_string =
            toml::to_string_pretty(&self.inner).map_err(|e| ConfigError::Serialize(e))?;
        let toml_string = Self::add_option_comments(&toml_string);
        atomic_write(path.as_ref(), toml_string.as_bytes())?;
        Ok(())
    }

    /// The comment header above the block JWM owns, so a reader can see which
    /// lines are written by the window manager rather than by hand. Written
    /// verbatim and recognized verbatim, so a save replaces its own header
    /// instead of stacking a new copy on every write.
    const LAYOUT_TAGS_HEADER: [&'static str; 3] = [
        "# --- per-tag layout, saved by jwm ---",
        "# Rewritten when a tag's layout changes; set layout.persist_tags",
        "# to false to keep this block under your own hand.",
    ];

    /// Write `entries` into the config file's `[[layout.tags]]` block, leaving
    /// every other byte of the file exactly as it was.
    ///
    /// A full `save_to_file` would be the obvious way to do this and is the
    /// wrong one: serializing the whole config back out normalizes the
    /// formatting and drops every comment the user wrote. The block JWM owns
    /// is small and always at the end, so it is cut out and re-appended as
    /// text instead — the rest of the file is never re-serialized.
    ///
    /// Returns the file's new modification time, which the caller records so
    /// the config watcher does not treat JWM's own write as an edit to reload.
    pub fn persist_layout_tags(
        &self,
        entries: &[LayoutTagConfig],
    ) -> Result<std::time::SystemTime, ConfigError> {
        self.persist_layout_tags_to(Self::resolve_load_path(), entries)
    }

    /// [`Self::persist_layout_tags`] against an explicit path.
    pub fn persist_layout_tags_to<P: AsRef<Path>>(
        &self,
        path: P,
        entries: &[LayoutTagConfig],
    ) -> Result<std::time::SystemTime, ConfigError> {
        let path = path.as_ref();
        let existing = match fs::read_to_string(path) {
            Ok(text) => text,
            // No file to preserve: fall back to writing the whole config,
            // which is also what the first start does.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut whole = self.clone();
                whole.set_layout_tags(entries.to_vec());
                whole.save_to_file(&path)?;
                return Ok(fs::metadata(&path)?.modified()?);
            }
            Err(error) => return Err(error.into()),
        };

        let mut text = Self::strip_layout_tag_blocks(&existing);
        let block = Self::render_layout_tag_blocks(entries);
        if !block.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(&block);
        }
        atomic_write(&path, text.as_bytes())?;
        Ok(fs::metadata(&path)?.modified()?)
    }

    /// Remove every `[[layout.tags]]` table from a config file's text, along
    /// with the comment header JWM writes above them.
    ///
    /// A table runs until the next table header, which is the only structure
    /// TOML gives us to cut on; comments a user parked *inside* the block are
    /// part of it and go too. Everything outside is untouched, which is the
    /// whole reason this path exists.
    fn strip_layout_tag_blocks(text: &str) -> String {
        let mut kept: Vec<&str> = Vec::with_capacity(text.lines().count());
        let mut in_block = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if Self::LAYOUT_TAGS_HEADER.contains(&trimmed) {
                continue;
            }
            if trimmed == "[[layout.tags]]" {
                in_block = true;
                continue;
            }
            if in_block {
                if trimmed.starts_with('[') {
                    in_block = false;
                } else {
                    continue;
                }
            }
            kept.push(line);
        }
        // A block at the end of the file leaves the blank line that separated
        // it behind; the new block brings its own.
        while kept.last().is_some_and(|last| last.trim().is_empty()) {
            kept.pop();
        }
        let mut out = kept.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// Render `entries` as TOML tables.
    ///
    /// Hand-written rather than serialized because the file is edited as text:
    /// `toml` would happily produce the same tables, but only as part of a
    /// document, and the point of this path is to leave the document alone.
    /// Every value here is a number or an identifier-shaped layout name, so
    /// the only quoting needed is on the two string fields.
    fn render_layout_tag_blocks(entries: &[LayoutTagConfig]) -> String {
        if entries.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(entries.len() * 128);
        for line in Self::LAYOUT_TAGS_HEADER {
            out.push_str(line);
            out.push('\n');
        }
        for entry in entries {
            out.push_str("[[layout.tags]]\n");
            out.push_str(&format!("tag = {}\n", entry.tag));
            out.push_str(&format!("monitor = {}\n", entry.monitor));
            if !entry.layout.is_empty() {
                out.push_str(&format!(
                    "layout = {}\n",
                    toml_string_literal(&entry.layout)
                ));
            }
            if !entry.alt.is_empty() {
                out.push_str(&format!("alt = {}\n", toml_string_literal(&entry.alt)));
            }
            if let Some(n_master) = entry.n_master {
                out.push_str(&format!("n_master = {n_master}\n"));
            }
            // TOML floats need a decimal point; `{:?}` on f32 keeps one and
            // round-trips the value exactly. A NaN would print as `nan` and
            // make the file unloadable, so it is simply not written.
            if let Some(m_fact) = entry.m_fact.filter(|value| value.is_finite()) {
                out.push_str(&format!("m_fact = {m_fact:?}\n"));
            }
            if let Some(gap) = entry.gap {
                out.push_str(&format!("gap = {gap}\n"));
            }
        }
        out
    }

    /// Post-process TOML output to add comments showing available options for enum-like fields.
    fn add_option_comments(toml: &str) -> String {
        let mut result = String::with_capacity(toml.len() + 512);
        let mut section = String::new();
        for line in toml.lines() {
            let trimmed = line.trim();

            // Track current TOML section
            if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
                section = trimmed.trim_matches(|c| c == '[' || c == ']').to_string();
            }

            // transition_mode (in [behavior])
            if section == "behavior" && trimmed.starts_with("transition_mode") {
                result.push_str("# available: slide, cube, fade, flip, zoom, stack, blinds, coverflow, helix, portal, book\n");
            }
            // wallpaper_mode (in [behavior])
            else if section == "behavior" && trimmed.starts_with("wallpaper_mode") {
                result.push_str("# available: fill, fit, stretch, center\n");
            }
            // new_client_position (in [behavior])
            else if section == "behavior" && trimmed.starts_with("new_client_position") {
                result.push_str("# available: master, tail, after_focused\n");
            }
            // colorblind_mode (in [behavior])
            else if section == "behavior" && trimmed.starts_with("colorblind_mode") {
                result.push_str(
                    "# available: \"\" (disabled), deuteranopia, protanopia, tritanopia\n",
                );
            }
            // easing (in [animation])
            else if section == "animation" && trimmed.starts_with("easing") {
                result.push_str(
                    "# available: linear, ease-out, ease-in, ease-in-out, bounce, elastic\n",
                );
            }
            // speed (in [animation])
            else if section == "animation" && trimmed.starts_with("speed") {
                result.push_str("# available: slow, normal, fast, instant\n");
            }

            result.push_str(line);
            result.push('\n');
        }
        result
    }

    pub fn save_default(&self) -> Result<(), ConfigError> {
        let config_path = Self::get_default_config_path();
        self.save_to_file(config_path)
    }

    pub fn get_config_path_for(family: BackendFamily) -> std::path::PathBuf {
        let name = match family {
            BackendFamily::X11 => "config_x11.toml",
            BackendFamily::Wayland => "config_wayland.toml",
        };
        dirs::config_dir()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("jwm")
            .join(name)
    }

    pub fn get_default_config_path() -> std::path::PathBuf {
        Self::get_config_path_for(get_backend_family())
    }

    pub fn resolve_load_path() -> std::path::PathBuf {
        Self::get_default_config_path()
    }

    pub fn generate_template<P: AsRef<Path>>(path: P) -> Result<(), ConfigError> {
        let default_config = Self::default();
        default_config.save_to_file(path)
    }

    pub fn backup_config<P: AsRef<Path>>(
        original_path: P,
    ) -> Result<std::path::PathBuf, ConfigError> {
        let original = original_path.as_ref();
        let backup_path = original.with_extension("toml.backup");

        if original.exists() {
            fs::copy(original, &backup_path)?;
        }

        Ok(backup_path)
    }

    pub fn restore_from_backup<P: AsRef<Path>>(
        backup_path: P,
        target_path: P,
    ) -> Result<(), ConfigError> {
        let backup = backup_path.as_ref();
        let target = target_path.as_ref();

        if !backup.exists() {
            return Err(ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Backup file not found",
            )));
        }

        fs::copy(backup, target)?;
        Ok(())
    }

    pub fn validate_config_file<P: AsRef<Path>>(path: P) -> Result<ConfigDiagnostics, ConfigError> {
        let content = fs::read_to_string(path)?;
        let config: TomlConfig = toml::from_str(&content)?;
        Ok(Self { inner: config }.diagnostics())
    }

    pub fn merge_config(&mut self, other: TomlConfig) {
        self.inner = other;
    }

    /// Apply a single key/value override to the in-memory config without
    /// touching the on-disk file. Only a small set of hot-tunable scalar
    /// keys are accepted; unknown or unsupported keys return Err.
    pub fn set_value(&mut self, key: &str, value: &serde_json::Value) -> Result<(), String> {
        let as_u32 = || {
            value
                .as_u64()
                .filter(|v| *v <= u32::MAX as u64)
                .map(|v| v as u32)
                .ok_or_else(|| format!("expected u32 for '{key}'"))
        };
        let as_f32 = || {
            value
                .as_f64()
                .map(|v| v as f32)
                .ok_or_else(|| format!("expected number for '{key}'"))
        };
        let as_bool = || {
            value
                .as_bool()
                .ok_or_else(|| format!("expected bool for '{key}'"))
        };
        let as_string = || {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("expected string for '{key}'"))
        };
        let as_rgba = || {
            let items = value
                .as_array()
                .filter(|items| items.len() == 4)
                .ok_or_else(|| format!("expected [r, g, b, a] for '{key}'"))?;
            let mut rgba = [0.0f32; 4];
            for (slot, item) in rgba.iter_mut().zip(items) {
                let component = item
                    .as_f64()
                    .ok_or_else(|| format!("expected numbers in '{key}'"))?
                    as f32;
                if !(0.0..=1.0).contains(&component) {
                    return Err(format!("{key}={component} out of [0, 1]"));
                }
                *slot = component;
            }
            Ok(rgba)
        };
        match key {
            "appearance.border_px" => self.inner.appearance.border_px = as_u32()?,
            "appearance.gap_px" => self.inner.appearance.gap_px = as_u32()?,
            "appearance.snap" => self.inner.appearance.snap = as_u32()?,
            "appearance.cursor_theme" => self.inner.appearance.cursor_theme = as_string()?,
            "appearance.cursor_size" => {
                let v = as_u32()?;
                if v > MAX_CURSOR_SIZE {
                    return Err(format!(
                        "appearance.cursor_size={v} out of [0, {MAX_CURSOR_SIZE}]"
                    ));
                }
                self.inner.appearance.cursor_size = v;
            }
            "appearance.ui_theme" => {
                let v = as_string()?;
                let normalized = v.trim().to_ascii_lowercase().replace('_', "-");
                if !matches!(
                    normalized.as_str(),
                    "material"
                        | "glass"
                        | "glass-dark"
                        | "aurora"
                        | "nord"
                        | "tokyo-night"
                        | "paper"
                ) {
                    return Err(format!(
                        "appearance.ui_theme={v} is not one of: material, glass, glass-dark, \
                         aurora, nord, tokyo-night, paper"
                    ));
                }
                self.inner.appearance.ui_theme = normalized;
            }
            "layout.m_fact" => {
                let v = as_f32()?;
                if !(0.05..=0.95).contains(&v) {
                    return Err(format!("layout.m_fact={v} out of [0.05, 0.95]"));
                }
                self.inner.layout.m_fact = v;
            }
            "layout.n_master" => self.inner.layout.n_master = as_u32()?,
            "status_bar.show_bar" => self.inner.status_bar.show_bar = as_bool()?,
            "behavior.active_opacity" => {
                let v = as_f32()?;
                if !(0.0..=1.0).contains(&v) {
                    return Err(format!("behavior.active_opacity={v} out of [0, 1]"));
                }
                self.inner.behavior.active_opacity = v;
            }
            "behavior.inactive_opacity" => {
                let v = as_f32()?;
                if !(0.0..=1.0).contains(&v) {
                    return Err(format!("behavior.inactive_opacity={v} out of [0, 1]"));
                }
                self.inner.behavior.inactive_opacity = v;
            }
            "behavior.blur_strength" => {
                let v = as_u32()?;
                if v > 5 {
                    return Err(format!("behavior.blur_strength={v} out of [0, 5]"));
                }
                self.inner.behavior.blur_strength = v;
            }
            "behavior.new_client_position" => {
                let position = as_string()?;
                let normalized = position.trim().to_ascii_lowercase();
                if !["master", "tail", "after_focused"].contains(&normalized.as_str()) {
                    return Err(format!(
                        "behavior.new_client_position={position:?} (expected master, tail, or after_focused)"
                    ));
                }
                self.inner.behavior.new_client_position = normalized;
            }
            "behavior.drag_threshold_px" => {
                self.inner.behavior.drag_threshold_px = as_u32()?;
            }
            "behavior.client_moveresize" => {
                let policy = as_string()?;
                let normalized = policy.trim().to_ascii_lowercase();
                if !["always", "floating-only", "never"].contains(&normalized.as_str()) {
                    return Err(format!(
                        "behavior.client_moveresize={policy:?} (expected always, floating-only, or never)"
                    ));
                }
                self.inner.behavior.client_moveresize = normalized;
            }
            "behavior.wallpaper" => self.inner.behavior.wallpaper = as_string()?,
            "behavior.wallpaper_mode" => {
                let mode = as_string()?;
                if !["fill", "fit", "stretch", "center"].contains(&mode.as_str()) {
                    return Err(format!(
                        "behavior.wallpaper_mode={mode:?} (expected fill, fit, stretch, or center)"
                    ));
                }
                self.inner.behavior.wallpaper_mode = mode;
            }
            "behavior.wallpaper_dir" => self.inner.behavior.wallpaper_dir = as_string()?,
            "behavior.wallpaper_colors" => self.inner.behavior.wallpaper_colors = as_bool()?,
            "behavior.idle_dim_secs" => self.inner.behavior.idle_dim_secs = u64::from(as_u32()?),
            "behavior.idle_dim_level" => {
                let level = as_f32()?;
                if !(0.0..=1.0).contains(&level) {
                    return Err(format!("behavior.idle_dim_level={level} out of [0, 1]"));
                }
                self.inner.behavior.idle_dim_level = level;
            }
            "behavior.idle_lock_secs" => self.inner.behavior.idle_lock_secs = u64::from(as_u32()?),
            "behavior.idle_screen_off_secs" => {
                self.inner.behavior.idle_screen_off_secs = u64::from(as_u32()?)
            }
            "behavior.idle_screen_off_command" => {
                self.inner.behavior.idle_screen_off_command = as_string()?
            }
            "behavior.idle_screen_on_command" => {
                self.inner.behavior.idle_screen_on_command = as_string()?
            }
            "behavior.resource_rows" => self.inner.behavior.resource_rows = as_bool()?,
            "behavior.border_color_focused" => {
                self.inner.behavior.border_color_focused = as_rgba()?
            }
            "behavior.border_gradient_color_a" => {
                self.inner.behavior.border_gradient_color_a = as_rgba()?
            }
            "behavior.border_gradient_color_b" => {
                self.inner.behavior.border_gradient_color_b = as_rgba()?
            }
            "behavior.border_glow_color" => self.inner.behavior.border_glow_color = as_rgba()?,
            "behavior.clipboard_history" => self.inner.behavior.clipboard_history = as_bool()?,
            "behavior.blur_enabled" => self.inner.behavior.blur_enabled = as_bool()?,
            "behavior.shadow_enabled" => self.inner.behavior.shadow_enabled = as_bool()?,
            "behavior.compositor" => self.inner.behavior.compositor = as_bool()?,
            "behavior.corner_radius" => {
                let v = as_f32()?;
                if !(0.0..=64.0).contains(&v) {
                    return Err(format!("behavior.corner_radius={v} out of [0, 64]"));
                }
                self.inner.behavior.corner_radius = v;
            }
            "behavior.fading" => self.inner.behavior.fading = as_bool()?,
            "behavior.wobbly_windows" => self.inner.behavior.wobbly_windows = as_bool()?,
            "behavior.motion_trail" => self.inner.behavior.motion_trail = as_bool()?,
            "behavior.screenshot_freeze_enabled" => {
                self.inner.behavior.screenshot_freeze_enabled = as_bool()?
            }
            "behavior.recording_fps" => {
                let v = as_u32()?;
                if !(1..=240).contains(&v) {
                    return Err(format!("behavior.recording_fps={v} out of [1, 240]"));
                }
                self.inner.behavior.recording_fps = v;
            }
            "behavior.recording_audio_enabled" => {
                self.inner.behavior.recording_audio_enabled = as_bool()?
            }
            "behavior.recording_audio_device" => {
                let device = as_string()?;
                if device.trim().is_empty() {
                    return Err("behavior.recording_audio_device must not be empty".into());
                }
                self.inner.behavior.recording_audio_device = device;
            }
            "behavior.recording_audio_bitrate" => {
                let bitrate = as_string()?;
                if bitrate.trim().is_empty() {
                    return Err("behavior.recording_audio_bitrate must not be empty".into());
                }
                self.inner.behavior.recording_audio_bitrate = bitrate;
            }
            "behavior.audio_recording_backend" => {
                let backend = as_string()?;
                if !matches!(backend.as_str(), "auto" | "direct" | "ffmpeg") {
                    return Err(
                        "behavior.audio_recording_backend must be auto, direct, or ffmpeg".into(),
                    );
                }
                self.inner.behavior.audio_recording_backend = backend;
            }
            "behavior.audio_recording_format" => {
                let format = as_string()?;
                if !matches!(format.as_str(), "wav" | "flac" | "opus" | "mp3") {
                    return Err(
                        "behavior.audio_recording_format must be wav, flac, opus, or mp3".into(),
                    );
                }
                self.inner.behavior.audio_recording_format = format;
            }
            "behavior.audio_recording_bitrate" => {
                let bitrate = as_string()?;
                if bitrate.trim().is_empty() {
                    return Err("behavior.audio_recording_bitrate must not be empty".into());
                }
                self.inner.behavior.audio_recording_bitrate = bitrate;
            }
            _ => {
                return Err(format!(
                    "set_config: unknown or non-hot-tunable key '{key}'"
                ));
            }
        }
        Ok(())
    }

    /// Atomically apply a batch of hot-tunable in-memory overrides. The
    /// current config is unchanged if any key/value pair is invalid.
    pub fn set_values(&mut self, changes: &[(String, serde_json::Value)]) -> Result<(), String> {
        let mut candidate = self.clone();
        for (key, value) in changes {
            candidate.set_value(key, value)?;
        }
        *self = candidate;
        Ok(())
    }

    pub fn reload(&mut self) -> Result<(), ConfigError> {
        let config_path = Self::resolve_load_path();
        if config_path.exists() {
            let new_config = Self::load_from_file(&config_path)?;
            // load_from_file already ran semantic diagnostics.
            self.inner = new_config.inner;
        }
        Ok(())
    }

    pub fn config_exists() -> bool {
        Self::resolve_load_path().exists()
    }

    pub fn get_config_modified_time() -> Result<std::time::SystemTime, ConfigError> {
        let config_path = Self::get_default_config_path();
        let metadata = fs::metadata(config_path)?;
        Ok(metadata.modified()?)
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    Validation(ConfigDiagnostics),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(err) => write!(f, "IO error: {}", err),
            ConfigError::Parse(err) => write!(f, "Parse error: {}", err),
            ConfigError::Serialize(err) => write!(f, "Serialize error: {}", err),
            ConfigError::Validation(diagnostics) => write!(f, "{diagnostics}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(err) => Some(err),
            ConfigError::Parse(err) => Some(err),
            ConfigError::Serialize(err) => Some(err),
            ConfigError::Validation(_) => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        ConfigError::Io(err)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(err: toml::de::Error) -> Self {
        ConfigError::Parse(err)
    }
}

impl From<toml::ser::Error> for ConfigError {
    fn from(err: toml::ser::Error) -> Self {
        ConfigError::Serialize(err)
    }
}

pub static CONFIG: LazyLock<ArcSwap<Config>> = LazyLock::new(|| {
    let config = if !LOAD_LOCAL_CONFIG {
        Config::default()
    } else {
        if !Config::config_exists() {
            let path = Config::get_default_config_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match Config::generate_template(&path) {
                Ok(()) => println!("Generated default config file at: {}", path.display()),
                Err(e) => eprintln!(
                    "Failed to write default config at {}: {e}; using built-in defaults",
                    path.display()
                ),
            }
        }
        Config::load_default()
    };
    ArcSwap::from_pointee(config)
});

/// Reload the global CONFIG from disk. Returns Ok on success.
pub fn reload_global() -> Result<(), ConfigError> {
    let new_config = Config::load_from_file(Config::resolve_load_path())?;
    CONFIG.store(Arc::new(new_config));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ArgumentConfig, ButtonConfig, CONFIG_WRITE_COUNTER, ClientMoveResize, Config,
        ConfigDiagnosticLevel, ConfigError, GestureSwipeConfig, KeyConfig, LayoutTagConfig,
        MAX_CURSOR_SIZE, Mods, NewClientPosition, Ordering, STATUS_BAR_NAME, TomlConfig,
        WallpaperMonitorConfig, WallpaperTagConfig, configured_scratchpad_terminal,
        configured_terminal_execution_prefix, key_function_is_repeatable,
        migrate_legacy_terminal_argument, parse_terminal_override, resolve_cursor_size,
        scene_linear_render_path_requested,
    };

    fn temporary_config_path(label: &str) -> std::path::PathBuf {
        let sequence = CONFIG_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "jwm-{label}-{}-{sequence}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn built_in_configuration_has_no_semantic_diagnostics() {
        let diagnostics = Config::default().diagnostics();
        assert!(diagnostics.is_empty(), "{diagnostics}");
    }

    #[test]
    fn scene_linear_render_path_requires_both_config_gates() {
        assert!(!scene_linear_render_path_requested(false, false));
        assert!(!scene_linear_render_path_requested(false, true));
        assert!(!scene_linear_render_path_requested(true, false));
        assert!(scene_linear_render_path_requested(true, true));
    }

    #[test]
    fn no_argument_bindings_may_omit_the_placeholder_argument() {
        let key: KeyConfig = toml::from_str(
            r#"
                modifier = ["Mod1"]
                key = "q"
                function = "quit"
            "#,
        )
        .unwrap();
        assert!(matches!(key.argument, ArgumentConfig::Int(0)));

        let button: ButtonConfig = toml::from_str(
            r#"
                click_type = "ClkClientWin"
                modifier = []
                button = 1
                function = "movemouse"
            "#,
        )
        .unwrap();
        assert!(matches!(button.argument, ArgumentConfig::Int(0)));
    }

    #[test]
    fn generated_legacy_terminal_commands_migrate_without_rewriting_other_arguments() {
        let migrated = migrate_legacy_terminal_argument(
            "spawn",
            crate::jwm::WMArgEnum::StringVec(vec!["jterm4".into(), "--profile".into()]),
        );
        assert_eq!(
            migrated,
            crate::jwm::WMArgEnum::StringVec(vec!["frost".into(), "--profile".into()])
        );

        let scratchpad = migrate_legacy_terminal_argument(
            "togglescratchpad",
            crate::jwm::WMArgEnum::StringVec(vec![
                "term".into(),
                "jterm1".into(),
                "--safe-mode".into(),
            ]),
        );
        assert_eq!(
            scratchpad,
            crate::jwm::WMArgEnum::StringVec(vec![
                "term".into(),
                "forge".into(),
                "--safe-mode".into(),
            ])
        );

        let explicit_path = crate::jwm::WMArgEnum::StringVec(vec!["/opt/jterm4".into()]);
        assert_eq!(
            migrate_legacy_terminal_argument("spawn", explicit_path.clone()),
            explicit_path
        );
    }

    #[test]
    fn terminal_override_parsing_preserves_safe_argv_boundaries() {
        use std::ffi::OsStr;

        assert_eq!(parse_terminal_override(None), Ok(None));
        assert_eq!(parse_terminal_override(Some(OsStr::new("   "))), Ok(None));
        assert_eq!(
            parse_terminal_override(Some(OsStr::new("\"\" --profile tools"))),
            Err("command program is empty".into())
        );
        assert!(parse_terminal_override(Some(OsStr::new("'   '"))).is_err());
        assert_eq!(
            parse_terminal_override(Some(OsStr::new("custom-terminal --profile 'Focused work'"))),
            Ok(Some(vec![
                "custom-terminal".into(),
                "--profile".into(),
                "Focused work".into(),
            ]))
        );
        assert!(parse_terminal_override(Some(OsStr::new("terminal 'unfinished"))).is_err());
    }

    #[test]
    fn terminal_execution_prefix_uses_declared_capabilities() {
        assert_eq!(
            configured_terminal_execution_prefix(vec!["gnome-terminal".into()]),
            Some(vec!["gnome-terminal".into(), "--".into()])
        );
        assert_eq!(
            configured_terminal_execution_prefix(vec!["terminator".into()]),
            Some(vec!["terminator".into(), "-x".into()])
        );
        assert_eq!(
            configured_terminal_execution_prefix(vec!["frost".into()]),
            None
        );
        assert_eq!(
            configured_terminal_execution_prefix(vec![
                "custom-terminal".into(),
                "--profile".into(),
                "work".into(),
            ]),
            Some(vec![
                "custom-terminal".into(),
                "--profile".into(),
                "work".into(),
                "-e".into(),
            ])
        );
    }

    #[test]
    fn scratchpad_override_rejects_known_pid_unstable_terminals() {
        assert_eq!(
            configured_scratchpad_terminal(vec!["gnome-terminal".into()]),
            None
        );
        assert_eq!(
            configured_scratchpad_terminal(vec!["frost".into(), "--profile".into()]),
            Some(vec!["frost".into(), "--profile".into()])
        );
        assert_eq!(
            configured_scratchpad_terminal(vec!["custom-terminal".into(), "--safe".into()]),
            Some(vec!["custom-terminal".into(), "--safe".into()])
        );
    }

    #[test]
    fn string_spawn_bindings_are_parsed_as_argv_instead_of_one_program_name() {
        let config = Config::default();
        let binding = KeyConfig {
            modifier: vec!["Mod1".into()],
            key: "F8".into(),
            function: "spawn".into(),
            argument: ArgumentConfig::String("custom-terminal --profile 'Focused work'".into()),
        };
        let runtime = config.convert_key_config(&binding).expect("valid binding");
        assert_eq!(
            runtime.arg,
            crate::jwm::WMArgEnum::StringVec(vec![
                "custom-terminal".into(),
                "--profile".into(),
                "Focused work".into(),
            ])
        );

        let button = ButtonConfig {
            click_type: "ClkRootWin".into(),
            modifier: vec![],
            button: 3,
            function: "spawn".into(),
            argument: ArgumentConfig::String("launcher --label 'Mouse menu'".into()),
        };
        let runtime = config
            .convert_button_config(&button)
            .expect("valid button binding");
        assert_eq!(
            runtime.arg,
            crate::jwm::WMArgEnum::StringVec(vec![
                "launcher".into(),
                "--label".into(),
                "Mouse menu".into(),
            ])
        );

        let mut malformed = binding;
        malformed.argument = ArgumentConfig::String("terminal 'unfinished".into());
        assert!(config.convert_key_config(&malformed).is_none());
        let mut malformed_config = Config::default();
        malformed_config.inner.keybindings.keys = vec![malformed];
        assert!(malformed_config.diagnostics().has_errors());
    }

    #[test]
    fn compositor_api_defaults_to_egl() {
        let config = Config::default();
        assert_eq!(config.behavior().compositor_api, "egl");

        // Existing configs created before compositor_api was introduced use
        // the same serde default when the field is absent.
        let serialized = toml::to_string(&config.inner).unwrap();
        let without_api = serialized
            .lines()
            .filter(|line| !line.starts_with("compositor_api ="))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: TomlConfig = toml::from_str(&without_api).unwrap();
        assert_eq!(parsed.behavior.compositor_api, "egl");
    }

    #[test]
    fn screenshot_freeze_defaults_on_and_old_configs_remain_compatible() {
        let config = Config::default();
        assert!(config.behavior().screenshot_freeze_enabled);

        let serialized = toml::to_string(&config.inner).unwrap();
        assert!(serialized.contains("screenshot_freeze_enabled = true"));
        let without_freeze_setting = serialized
            .lines()
            .filter(|line| !line.trim_start().starts_with("screenshot_freeze_enabled"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: TomlConfig = toml::from_str(&without_freeze_setting).unwrap();
        assert!(parsed.behavior.screenshot_freeze_enabled);
    }

    #[test]
    fn waterlily_binding_and_migration_alias_resolve_to_the_canonical_action() {
        let config = Config::default();
        assert!(
            config
                .inner
                .keybindings
                .keys
                .iter()
                .any(|key| key.function == "toggle_waterlily")
        );
        assert!(
            !config
                .inner
                .keybindings
                .keys
                .iter()
                .any(|key| key.function == "toggle_slime")
        );

        let canonical = config.parse_function("toggle_waterlily").unwrap();
        let deprecated = config.parse_function("toggle_slime").unwrap();
        assert!(std::ptr::fn_addr_eq(canonical, deprecated));
    }

    #[test]
    fn built_in_status_bar_matches_installer_default() {
        let expected = format!("JWM_BAR_NAME=\"{STATUS_BAR_NAME}\"");
        assert!(
            include_str!("../scripts/install_jwm_scripts.sh").contains(&expected),
            "installer default must match {STATUS_BAR_NAME}"
        );
    }

    #[test]
    fn duplicate_shortcut_is_reported_as_unreachable() {
        let mut config = Config::default();
        config
            .inner
            .keybindings
            .keys
            .push(config.inner.keybindings.keys[0].clone());

        let diagnostics = config.diagnostics();
        assert!(diagnostics.issues().iter().any(|issue| {
            issue.level == ConfigDiagnosticLevel::Warning
                && issue.path.contains("keybindings.keys")
                && issue.message.contains("unreachable")
        }));
    }

    #[test]
    fn semantic_validation_preserves_supported_compact_spawn_syntax() {
        let mut config = Config::default();
        let spawn = config
            .inner
            .keybindings
            .keys
            .iter_mut()
            .find(|binding| binding.function == "spawn")
            .unwrap();
        spawn.argument = ArgumentConfig::String("alacritty".into());
        config.inner.behavior.shadow_enabled = false;
        config.inner.behavior.shadow_radius = 0.0;
        config.inner.behavior.magnifier_enabled = false;
        config.inner.behavior.magnifier_radius = 0.0;
        config.inner.behavior.magnifier_zoom = 0.0;

        let diagnostics = config.diagnostics();
        assert!(!diagnostics.has_errors(), "{diagnostics}");
    }

    #[test]
    fn semantic_validation_keeps_degradable_features_non_blocking() {
        let mut config = Config::default();
        config.inner.status_bar.show_bar = false;
        config.inner.appearance.status_bar_height = 0;
        config.inner.behavior.vrr_enabled = false;
        config.inner.behavior.vrr_min_fps = 0;
        config.inner.behavior.vrr_max_fps = 0;
        config.inner.behavior.recording_fps = 0;
        config.inner.behavior.audio_recording_sample_rate = 0;
        config.inner.behavior.audio_recording_channels = 0;

        let diagnostics = config.diagnostics();
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert!(diagnostics.warning_count() >= 3, "{diagnostics}");
    }

    #[test]
    fn semantic_validation_bounds_compositor_effect_work_and_durations() {
        let mut config = Config::default();
        config.inner.behavior.fading = true;
        config.inner.behavior.ripple_on_open = true;
        config.inner.behavior.particle_effects = true;
        config.inner.behavior.genie_minimize = true;
        config.inner.behavior.fade_in_step = 0.0;
        config.inner.behavior.fade_out_step = 0.0;
        config.inner.behavior.ripple_duration = 0.0;
        config.inner.behavior.particle_lifetime = f32::NAN;
        config.inner.behavior.genie_duration_ms = 0;
        config.inner.behavior.wobbly_grid_size = u32::MAX;
        config.inner.behavior.motion_trail_frames = u32::MAX;
        config.inner.behavior.particle_count = u32::MAX;

        let diagnostics = config.diagnostics();
        for path in [
            "behavior.fade_in_step",
            "behavior.fade_out_step",
            "behavior.ripple_duration",
            "behavior.particle_lifetime",
            "behavior.genie_duration_ms",
        ] {
            assert!(
                diagnostics.issues().iter().any(|issue| {
                    issue.level == ConfigDiagnosticLevel::Error && issue.path == path
                }),
                "missing error for {path}: {diagnostics}"
            );
        }
        for path in [
            "behavior.wobbly_grid_size",
            "behavior.motion_trail_frames",
            "behavior.particle_count",
        ] {
            assert!(
                diagnostics.issues().iter().any(|issue| {
                    issue.level == ConfigDiagnosticLevel::Warning && issue.path == path
                }),
                "missing clamp warning for {path}: {diagnostics}"
            );
        }
    }

    #[test]
    fn disabled_compositor_effects_accept_zero_durations() {
        let mut config = Config::default();
        config.inner.behavior.particle_effects = false;
        config.inner.behavior.particle_lifetime = 0.0;
        config.inner.behavior.ripple_on_open = false;
        config.inner.behavior.ripple_duration = 0.0;
        config.inner.behavior.genie_minimize = false;
        config.inner.behavior.genie_duration_ms = 0;
        config.inner.behavior.focus_highlight = false;
        config.inner.behavior.focus_highlight_duration_ms = 0;
        config.inner.behavior.wallpaper_crossfade = false;
        config.inner.behavior.wallpaper_crossfade_duration_ms = 0;

        let diagnostics = config.diagnostics();
        assert!(!diagnostics.has_errors(), "{diagnostics}");
    }

    #[test]
    fn semantic_validation_matches_case_insensitive_runtime_choices() {
        let mut config = Config::default();
        config.inner.behavior.wallpaper_mode = "Fill".into();
        config
            .inner
            .behavior
            .wallpaper_monitors
            .push(WallpaperMonitorConfig {
                monitor: 0,
                path: String::new(),
                mode: "FIT".into(),
            });
        config
            .inner
            .behavior
            .wallpaper_tags
            .push(WallpaperTagConfig {
                tag: 0,
                monitor: -1,
                path: String::new(),
                mode: "Center".into(),
            });
        config
            .inner
            .behavior
            .gesture_swipe
            .push(GestureSwipeConfig {
                fingers: 3,
                direction: "Left".into(),
                function: "loopview".into(),
                argument: ArgumentConfig::Int(1),
            });

        let diagnostics = config.diagnostics();
        assert!(diagnostics.is_empty(), "{diagnostics}");
    }

    #[test]
    fn unknown_gesture_command_is_a_non_blocking_warning() {
        let mut config = Config::default();
        config
            .inner
            .behavior
            .gesture_swipe
            .push(GestureSwipeConfig {
                fingers: 3,
                direction: "left".into(),
                function: "typo_command".into(),
                argument: ArgumentConfig::Int(0),
            });

        let diagnostics = config.diagnostics();
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert!(diagnostics.issues().iter().any(|issue| {
            issue.path.ends_with(".function") && issue.message.contains("unknown IPC command")
        }));
    }

    #[test]
    fn unsafe_tag_count_is_a_validation_error() {
        let mut config = Config::default();
        config.inner.layout.tags_length = 32;

        let diagnostics = config.diagnostics();
        assert!(diagnostics.has_errors());
        assert!(
            diagnostics
                .issues()
                .iter()
                .any(|issue| issue.path == "layout.tags_length")
        );
    }

    fn layout_tag(tag: usize, monitor: i32, layout: &str) -> LayoutTagConfig {
        LayoutTagConfig {
            tag,
            monitor,
            layout: layout.to_owned(),
            alt: "tile".to_owned(),
            n_master: Some(2),
            m_fact: Some(0.62),
            gap: Some(8),
        }
    }

    /// The whole point of editing the file as text: a config full of the
    /// user's comments and ordering must come back byte for byte apart from
    /// the block JWM owns.
    #[test]
    fn saving_per_tag_layouts_leaves_the_rest_of_the_file_alone() {
        let path = temporary_config_path("layout-tags");
        let handwritten = "\
# my window manager
[layout]
m_fact = 0.55 # golden-ish
n_master = 1
tags_length = 9

[appearance]
border_px = 3
";
        std::fs::write(&path, handwritten).unwrap();

        let config = Config::default();
        config
            .persist_layout_tags_to(&path, &[layout_tag(1, 0, "monocle")])
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();

        assert!(written.starts_with(handwritten), "{written}");
        assert!(written.contains("m_fact = 0.55 # golden-ish"));
        assert!(written.contains("[[layout.tags]]"));
        assert!(written.contains("layout = \"monocle\""));
        std::fs::remove_file(path).unwrap();
    }

    /// Saving repeatedly must replace the block rather than stack copies of
    /// it, and what comes back has to parse into what went in.
    #[test]
    fn per_tag_layouts_are_replaced_not_appended_and_reload_intact() {
        let path = temporary_config_path("layout-tags-replace");
        Config::default().save_to_file(&path).unwrap();

        let config = Config::default();
        config
            .persist_layout_tags_to(
                &path,
                &[layout_tag(1, 0, "monocle"), layout_tag(2, 0, "grid")],
            )
            .unwrap();
        config
            .persist_layout_tags_to(&path, &[layout_tag(1, 0, "deck")])
            .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.matches("[[layout.tags]]").count(), 1, "{written}");
        // The default keybindings mention layouts by name too, so it is the
        // saved entry specifically that must be gone.
        assert!(!written.contains("layout = \"monocle\""), "{written}");

        let loaded = Config::load_from_file(&path).unwrap();
        assert_eq!(loaded.layout_tags(), [layout_tag(1, 0, "deck")]);
        // And the values survive the round trip untouched.
        let restored = loaded.layout_for_tag(0, 1).expect("entry for tag 1");
        assert_eq!(restored.m_fact, Some(0.62));
        assert_eq!(restored.n_master, Some(2));
        assert_eq!(restored.gap, Some(8));
        std::fs::remove_file(path).unwrap();
    }

    /// An empty entry list is how "stop persisting" ends up on disk; it must
    /// clear the block instead of leaving the last save behind.
    #[test]
    fn saving_no_per_tag_layouts_removes_the_block() {
        let path = temporary_config_path("layout-tags-empty");
        Config::default().save_to_file(&path).unwrap();
        let config = Config::default();
        config
            .persist_layout_tags_to(&path, &[layout_tag(1, 0, "deck")])
            .unwrap();
        config.persist_layout_tags_to(&path, &[]).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("[[layout.tags]]"), "{written}");
        // The comment header goes with it, or the file accumulates a heading
        // over nothing.
        assert!(
            !written.contains(Config::LAYOUT_TAGS_HEADER[0]),
            "{written}"
        );
        assert!(
            Config::load_from_file(&path)
                .unwrap()
                .layout_tags()
                .is_empty()
        );
        std::fs::remove_file(path).unwrap();
    }

    /// Saving the same layouts twice must produce the same file: this runs
    /// every couple of seconds of arranging windows, so any growth compounds.
    #[test]
    fn saving_the_same_per_tag_layouts_twice_is_a_no_op() {
        let path = temporary_config_path("layout-tags-stable");
        Config::default().save_to_file(&path).unwrap();
        let config = Config::default();
        let entries = [layout_tag(1, 0, "deck"), layout_tag(2, 0, "grid")];

        config.persist_layout_tags_to(&path, &entries).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        config.persist_layout_tags_to(&path, &entries).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first.matches(Config::LAYOUT_TAGS_HEADER[0]).count(),
            1,
            "{first}"
        );
        std::fs::remove_file(path).unwrap();
    }

    /// A hand-written block sitting in the middle of the file — with tables
    /// after it — has to be cut out without taking its neighbours along.
    #[test]
    fn a_block_in_the_middle_of_the_file_is_cut_out_cleanly() {
        let path = temporary_config_path("layout-tags-middle");
        std::fs::write(
            &path,
            "\
[layout]
m_fact = 0.55
n_master = 1
tags_length = 9

[[layout.tags]]
tag = 4
monitor = -1
layout = \"grid\"

[appearance]
border_px = 3
",
        )
        .unwrap();

        Config::default()
            .persist_layout_tags_to(&path, &[layout_tag(1, 0, "deck")])
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();

        assert!(!written.contains("tag = 4"), "{written}");
        assert!(written.contains("[appearance]\nborder_px = 3"), "{written}");
        assert_eq!(written.matches("[[layout.tags]]").count(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_missing_config_file_is_written_whole() {
        let path = temporary_config_path("layout-tags-missing");
        let _ = std::fs::remove_file(&path);
        Config::default()
            .persist_layout_tags_to(&path, &[layout_tag(3, 1, "bstack")])
            .unwrap();

        let loaded = Config::load_from_file(&path).unwrap();
        assert_eq!(loaded.layout_tags(), [layout_tag(3, 1, "bstack")]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_monitor_specific_entry_is_preferred_over_the_shared_one() {
        let mut config = Config::default();
        config.set_layout_tags(vec![layout_tag(1, -1, "grid"), layout_tag(1, 2, "deck")]);
        assert_eq!(config.layout_for_tag(2, 1).unwrap().layout, "deck");
        assert_eq!(config.layout_for_tag(0, 1).unwrap().layout, "grid");
        assert!(config.layout_for_tag(0, 7).is_none());
    }

    #[test]
    fn saved_configuration_roundtrips_through_atomic_writer() {
        let path = temporary_config_path("atomic-config");
        std::fs::write(&path, "incomplete = ").unwrap();

        let config = Config::default();
        config.save_to_file(&path).unwrap();
        let loaded = Config::load_from_file(&path).unwrap();

        assert_eq!(loaded.gap_px(), config.gap_px());
        assert!(loaded.diagnostics().is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn atomic_writer_preserves_existing_configuration_symlink() {
        let directory = temporary_config_path("symlink-config");
        std::fs::create_dir(&directory).unwrap();
        let target = directory.join("real.toml");
        let link = directory.join("config.toml");
        Config::default().save_to_file(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut updated = Config::default();
        updated.inner.appearance.gap_px = 17;
        updated.save_to_file(&link).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(Config::load_from_file(&target).unwrap().gap_px(), 17);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_writer_supports_dangling_relative_configuration_symlink() {
        let directory = temporary_config_path("dangling-symlink-config");
        let target_directory = directory.join("targets");
        std::fs::create_dir_all(&target_directory).unwrap();
        let target = target_directory.join("created.toml");
        let link = directory.join("config.toml");
        std::os::unix::fs::symlink("targets/created.toml", &link).unwrap();

        let mut config = Config::default();
        config.inner.appearance.gap_px = 23;
        config.save_to_file(&link).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(Config::load_from_file(&target).unwrap().gap_px(), 23);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_writer_preserves_chained_dangling_configuration_symlinks() {
        let directory = temporary_config_path("chained-dangling-symlink-config");
        let target_directory = directory.join("targets");
        std::fs::create_dir_all(&target_directory).unwrap();
        let target = target_directory.join("created.toml");
        let intermediate = directory.join("intermediate.toml");
        let link = directory.join("config.toml");
        std::os::unix::fs::symlink("targets/created.toml", &intermediate).unwrap();
        std::os::unix::fs::symlink("intermediate.toml", &link).unwrap();

        let mut config = Config::default();
        config.inner.appearance.gap_px = 29;
        config.save_to_file(&link).unwrap();

        for symlink in [&link, &intermediate] {
            assert!(
                std::fs::symlink_metadata(symlink)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        assert_eq!(Config::load_from_file(&target).unwrap().gap_px(), 29);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn loading_rejects_semantically_unsafe_configuration() {
        let path = temporary_config_path("invalid-config");
        let mut config = Config::default();
        config.inner.layout.tags_length = 32;
        config.save_to_file(&path).unwrap();

        let error = match Config::load_from_file(&path) {
            Ok(_) => panic!("unsafe configuration unexpectedly loaded"),
            Err(error) => error,
        };
        assert!(matches!(error, ConfigError::Validation(_)));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn key_repeat_policy_only_allows_incremental_actions() {
        assert!(key_function_is_repeatable("focusstack"));
        assert!(key_function_is_repeatable("setmfact"));
        assert!(key_function_is_repeatable("scrolling_focus_column"));

        assert!(!key_function_is_repeatable("spawn"));
        assert!(!key_function_is_repeatable("killclient"));
        assert!(!key_function_is_repeatable("take_screenshot"));
    }

    #[test]
    fn legacy_key_list_gets_non_conflicting_audio_recording_fallback() {
        let mut cfg = Config::default();
        cfg.inner
            .keybindings
            .keys
            .retain(|key| key.function != "toggle_audio_recording");

        let key_sym = cfg.parse_keysym("m").unwrap();
        let keys = cfg.get_keys();
        assert!(
            keys.iter()
                .any(|key| { key.key_sym == key_sym && key.mask == (Mods::ALT | Mods::CONTROL) })
        );
    }

    #[test]
    fn legacy_audio_fallback_does_not_override_occupied_chord() {
        let mut cfg = Config::default();
        cfg.inner
            .keybindings
            .keys
            .retain(|key| key.function != "toggle_audio_recording");
        cfg.inner.keybindings.keys.push(KeyConfig {
            modifier: vec!["Mod1".into(), "Control".into()],
            key: "m".into(),
            function: "spawn".into(),
            argument: ArgumentConfig::StringVec(vec!["true".into()]),
        });

        let key_sym = cfg.parse_keysym("m").unwrap();
        let matches = cfg
            .get_keys()
            .into_iter()
            .filter(|key| key.key_sym == key_sym && key.mask == (Mods::ALT | Mods::CONTROL))
            .count();
        assert_eq!(matches, 1);
    }

    #[test]
    fn legacy_key_list_gets_non_conflicting_monitor_layout_fallback() {
        let mut cfg = Config::default();
        cfg.inner
            .keybindings
            .keys
            .retain(|key| key.function != "monitor_layout");

        let key_sym = cfg.parse_keysym("o").unwrap();
        let keys = cfg.get_keys();
        assert!(
            keys.iter()
                .any(|key| { key.key_sym == key_sym && key.mask == (Mods::ALT | Mods::CONTROL) })
        );
    }

    #[test]
    fn legacy_monitor_layout_fallback_does_not_override_occupied_chord() {
        let mut cfg = Config::default();
        cfg.inner
            .keybindings
            .keys
            .retain(|key| key.function != "monitor_layout");
        cfg.inner.keybindings.keys.push(KeyConfig {
            modifier: vec!["Mod1".into(), "Control".into()],
            key: "o".into(),
            function: "spawn".into(),
            argument: ArgumentConfig::StringVec(vec!["true".into()]),
        });

        let key_sym = cfg.parse_keysym("o").unwrap();
        let matches = cfg
            .get_keys()
            .into_iter()
            .filter(|key| key.key_sym == key_sym && key.mask == (Mods::ALT | Mods::CONTROL))
            .count();
        assert_eq!(matches, 1);
    }

    #[test]
    fn legacy_duplicate_shortcuts_are_migrated_in_memory_when_targets_are_free() {
        let mut config = Config::default();
        config
            .inner
            .keybindings
            .keys
            .iter_mut()
            .find(|binding| binding.function == "togglescratchpad" && binding.key == "c")
            .unwrap()
            .modifier = vec!["Mod1".into(), "Shift".into()];
        config
            .inner
            .keybindings
            .keys
            .iter_mut()
            .find(|binding| binding.function == "togglesticky" && binding.key == "s")
            .unwrap()
            .modifier = vec!["Mod1".into(), "Shift".into()];

        let c = config.parse_keysym("c").unwrap();
        let s = config.parse_keysym("s").unwrap();
        let keys = config.get_keys();
        assert!(keys.iter().any(|binding| {
            binding.mask == (Mods::ALT | Mods::CONTROL)
                && binding.key_sym == c
                && matches!(
                    &binding.arg,
                    crate::jwm::WMArgEnum::StringVec(command)
                        if command.first().is_some_and(|name| name == "calc")
                )
        }));
        assert!(keys.iter().any(|binding| {
            binding.mask == (Mods::ALT | Mods::CONTROL) && binding.key_sym == s
        }));
    }

    #[test]
    fn set_values_applies_valid_batch_atomically() {
        let mut cfg = Config::default();
        let changes = vec![
            ("appearance.gap_px".to_string(), serde_json::json!(12)),
            ("layout.m_fact".to_string(), serde_json::json!(0.6)),
        ];

        cfg.set_values(&changes).unwrap();

        assert_eq!(cfg.gap_px(), 12);
        assert_eq!(cfg.m_fact(), 0.6);
    }

    #[test]
    fn set_values_rejects_invalid_batch_without_partial_apply() {
        let mut cfg = Config::default();
        let original_gap = cfg.gap_px();
        let original_m_fact = cfg.m_fact();
        let changes = vec![
            ("appearance.gap_px".to_string(), serde_json::json!(12)),
            ("layout.m_fact".to_string(), serde_json::json!(9.0)),
        ];

        assert!(cfg.set_values(&changes).is_err());
        assert_eq!(cfg.gap_px(), original_gap);
        assert_eq!(cfg.m_fact(), original_m_fact);
    }

    #[test]
    fn recording_fps_hot_override_is_range_checked() {
        let mut cfg = Config::default();
        cfg.set_value("behavior.recording_fps", &serde_json::json!(30))
            .unwrap();
        assert_eq!(cfg.behavior().recording_fps, 30);
        assert!(
            cfg.set_value("behavior.recording_fps", &serde_json::json!(0))
                .is_err()
        );
        assert_eq!(cfg.behavior().recording_fps, 30);
    }

    #[test]
    fn screenshot_freeze_is_hot_tunable() {
        let mut cfg = Config::default();
        cfg.set_value(
            "behavior.screenshot_freeze_enabled",
            &serde_json::json!(false),
        )
        .unwrap();
        assert!(!cfg.behavior().screenshot_freeze_enabled);
        assert!(
            cfg.set_value(
                "behavior.screenshot_freeze_enabled",
                &serde_json::json!("false"),
            )
            .is_err()
        );
    }

    #[test]
    fn recording_audio_hot_overrides_are_validated() {
        let mut cfg = Config::default();
        cfg.set_value(
            "behavior.recording_audio_enabled",
            &serde_json::json!(false),
        )
        .unwrap();
        cfg.set_value(
            "behavior.recording_audio_device",
            &serde_json::json!("hw:1,0"),
        )
        .unwrap();
        cfg.set_value(
            "behavior.recording_audio_bitrate",
            &serde_json::json!("160k"),
        )
        .unwrap();
        assert!(!cfg.behavior().recording_audio_enabled);
        assert_eq!(cfg.behavior().recording_audio_device, "hw:1,0");
        assert_eq!(cfg.behavior().recording_audio_bitrate, "160k");
        assert!(
            cfg.set_value("behavior.recording_audio_device", &serde_json::json!(""))
                .is_err()
        );
    }

    #[test]
    fn standalone_audio_backend_and_format_are_validated() {
        let mut cfg = Config::default();
        cfg.set_value(
            "behavior.audio_recording_backend",
            &serde_json::json!("ffmpeg"),
        )
        .unwrap();
        cfg.set_value(
            "behavior.audio_recording_format",
            &serde_json::json!("opus"),
        )
        .unwrap();
        assert_eq!(cfg.behavior().audio_recording_backend, "ffmpeg");
        assert_eq!(cfg.behavior().audio_recording_format, "opus");
        assert!(
            cfg.set_value(
                "behavior.audio_recording_backend",
                &serde_json::json!("unknown")
            )
            .is_err()
        );
        assert!(
            cfg.set_value("behavior.audio_recording_format", &serde_json::json!("aac"))
                .is_err()
        );
    }

    #[test]
    fn compositor_effect_hot_overrides_are_applied() {
        let mut cfg = Config::default();
        cfg.set_value("behavior.corner_radius", &serde_json::json!(18.0))
            .unwrap();
        cfg.set_value("behavior.fading", &serde_json::json!(true))
            .unwrap();
        cfg.set_value("behavior.wobbly_windows", &serde_json::json!(true))
            .unwrap();
        cfg.set_value("behavior.motion_trail", &serde_json::json!(true))
            .unwrap();
        assert_eq!(cfg.behavior().corner_radius, 18.0);
        assert!(
            cfg.behavior().fading && cfg.behavior().wobbly_windows && cfg.behavior().motion_trail
        );
        assert!(
            cfg.set_value("behavior.corner_radius", &serde_json::json!(65.0))
                .is_err()
        );
    }

    #[test]
    fn legacy_dmenu_font_deserializes_as_system_ui_font() {
        let cfg = Config::default();
        let modern = toml::to_string(&cfg.inner).unwrap();
        let legacy = modern.replacen("system_ui_font", "dmenu_font", 1);
        let parsed: TomlConfig = toml::from_str(&legacy).unwrap();
        assert_eq!(
            parsed.appearance.system_ui_font,
            "SauceCodePro Nerd Font Regular 11"
        );
    }

    #[test]
    fn cursor_settings_default_to_environment_sentinels() {
        let cfg = Config::default();
        // Out-of-the-box the config defers to the environment (empty/zero).
        assert_eq!(cfg.cursor_theme(), "");
        assert_eq!(cfg.cursor_size(), 0);
    }

    #[test]
    fn explicit_cursor_config_wins_over_environment() {
        let mut cfg = Config::default();
        cfg.inner.appearance.cursor_theme = "Bibata-Modern-Ice".into();
        cfg.inner.appearance.cursor_size = 48;
        // Configured values take precedence and never consult the environment.
        let (theme, size) = cfg.resolved_cursor();
        assert_eq!(theme, "Bibata-Modern-Ice");
        assert_eq!(size, 48);
    }

    #[test]
    fn cursor_size_resolution_never_overflows_signed_backend_dimensions() {
        use std::ffi::OsStr;

        assert_eq!(resolve_cursor_size(0, None), 24);
        assert_eq!(resolve_cursor_size(0, Some(OsStr::new(" 48 "))), 48);
        assert_eq!(resolve_cursor_size(0, Some(OsStr::new("0"))), 24);
        assert_eq!(resolve_cursor_size(0, Some(OsStr::new("4294967295"))), 24);
        assert_eq!(resolve_cursor_size(u32::MAX, None), MAX_CURSOR_SIZE);
    }

    #[test]
    fn ui_theme_defaults_to_glass_and_only_accepts_known_themes() {
        let mut cfg = Config::default();
        // An untouched config gets the flagship look.
        assert_eq!(cfg.ui_theme(), "glass");

        cfg.set_value("appearance.ui_theme", &serde_json::json!("Material"))
            .expect("a known theme is accepted, case-insensitively");
        assert_eq!(cfg.ui_theme(), "material");

        // Underscores normalize to the canonical hyphenated name.
        cfg.set_value("appearance.ui_theme", &serde_json::json!("Tokyo_Night"))
            .expect("a known theme is accepted with underscores");
        assert_eq!(cfg.ui_theme(), "tokyo-night");

        cfg.set_value("appearance.ui_theme", &serde_json::json!("neumorphic"))
            .expect_err("an unknown theme is rejected instead of silently applied");
        // The rejected write leaves the previous theme in place.
        assert_eq!(cfg.ui_theme(), "tokyo-night");
    }

    #[test]
    fn config_files_without_a_ui_theme_key_still_parse() {
        // Configs written before the key existed must still load.
        let cfg = Config::default();
        let serialized = toml::to_string(&cfg.inner).unwrap();
        let stripped = serialized
            .lines()
            .filter(|l| !l.trim_start().starts_with("ui_theme"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: TomlConfig = toml::from_str(&stripped).unwrap();
        assert_eq!(parsed.appearance.ui_theme, "glass");
    }

    #[test]
    fn new_clients_become_master_by_default() {
        let cfg = Config::default();
        assert_eq!(cfg.new_client_position(), NewClientPosition::Master);

        // Configs written before the key existed must still load as master.
        let serialized = toml::to_string(&cfg.inner).unwrap();
        let stripped = serialized
            .lines()
            .filter(|l| !l.trim_start().starts_with("new_client_position"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: TomlConfig = toml::from_str(&stripped).unwrap();
        assert_eq!(parsed.behavior.new_client_position, "master");
    }

    #[test]
    fn client_moveresize_defaults_to_floating_only() {
        let cfg = Config::default();
        assert_eq!(cfg.client_moveresize(), ClientMoveResize::FloatingOnly);
        assert_eq!(cfg.drag_threshold_px(), 12);

        // Configs written before the keys existed must still load with the
        // safe defaults.
        let serialized = toml::to_string(&cfg.inner).unwrap();
        let stripped = serialized
            .lines()
            .filter(|l| {
                let l = l.trim_start();
                !l.starts_with("client_moveresize") && !l.starts_with("drag_threshold_px")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: TomlConfig = toml::from_str(&stripped).unwrap();
        assert_eq!(parsed.behavior.client_moveresize, "floating-only");
        assert_eq!(parsed.behavior.drag_threshold_px, 12);
    }

    #[test]
    fn client_moveresize_parses_its_choices() {
        assert_eq!(
            ClientMoveResize::from_str("always"),
            ClientMoveResize::Always
        );
        assert_eq!(ClientMoveResize::from_str("Never"), ClientMoveResize::Never);
        assert_eq!(
            ClientMoveResize::from_str(" floating-only "),
            ClientMoveResize::FloatingOnly
        );
        // Anything unknown falls back to the safe default instead of failing.
        assert_eq!(
            ClientMoveResize::from_str("nonsense"),
            ClientMoveResize::FloatingOnly
        );
    }

    #[test]
    fn client_moveresize_is_hot_tunable_via_set_value() {
        let mut cfg = Config::default();
        cfg.set_value("behavior.client_moveresize", &serde_json::json!("always"))
            .unwrap();
        assert_eq!(cfg.client_moveresize(), ClientMoveResize::Always);
        assert!(
            cfg.set_value("behavior.client_moveresize", &serde_json::json!("maybe"))
                .is_err()
        );
        cfg.set_value("behavior.drag_threshold_px", &serde_json::json!(24))
            .unwrap();
        assert_eq!(cfg.drag_threshold_px(), 24);
    }

    #[test]
    fn new_client_position_parses_its_choices() {
        assert_eq!(NewClientPosition::from_str("tail"), NewClientPosition::Tail);
        assert_eq!(NewClientPosition::from_str("Tail"), NewClientPosition::Tail);
        assert_eq!(
            NewClientPosition::from_str(" after_focused "),
            NewClientPosition::AfterFocused
        );
        // Anything unknown falls back to the default instead of failing.
        assert_eq!(
            NewClientPosition::from_str("nonsense"),
            NewClientPosition::Master
        );
    }

    #[test]
    fn new_client_position_is_hot_tunable_via_set_value() {
        let mut cfg = Config::default();
        cfg.set_value("behavior.new_client_position", &serde_json::json!("Tail"))
            .unwrap();
        assert_eq!(cfg.new_client_position(), NewClientPosition::Tail);
        assert!(
            cfg.set_value("behavior.new_client_position", &serde_json::json!("middle"))
                .is_err()
        );
    }

    #[test]
    fn config_files_without_cursor_keys_still_parse() {
        // Older configs predate the cursor keys; serde defaults must fill them.
        let cfg = Config::default();
        let mut serialized = toml::to_string(&cfg.inner).unwrap();
        serialized = serialized
            .lines()
            .filter(|l| !l.trim_start().starts_with("cursor_"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: TomlConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.appearance.cursor_theme, "");
        assert_eq!(parsed.appearance.cursor_size, 0);
    }

    #[test]
    fn cursor_settings_are_hot_tunable_via_set_value() {
        let mut cfg = Config::default();
        cfg.set_value("appearance.cursor_theme", &serde_json::json!("macOS"))
            .unwrap();
        cfg.set_value("appearance.cursor_size", &serde_json::json!(32))
            .unwrap();
        assert_eq!(cfg.cursor_theme(), "macOS");
        assert_eq!(cfg.cursor_size(), 32);
        assert!(
            cfg.set_value("appearance.cursor_size", &serde_json::json!(9999))
                .is_err()
        );
    }
}
