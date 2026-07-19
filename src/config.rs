use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
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
use crate::terminal_prober::ADVANCED_TERMINAL_PROBER;
use std::time::Duration;

use crate::backend::common_define::keys as k;
use crate::backend::common_define::{KeySym, Mods, MouseButton};

mod validation;

pub use validation::{ConfigDiagnostic, ConfigDiagnosticLevel, ConfigDiagnostics};

pub const LOAD_LOCAL_CONFIG: bool = true;

static CONFIG_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
pub const STATUS_BAR_NAME: &str = "gpui_component_bar";

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorConfig {
    pub focus_follows_new_window: bool,
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
    /// Opacity for unfocused windows (0.0..1.0). 1.0 = fully opaque (no dim).
    #[serde(default = "default_inactive_opacity")]
    pub inactive_opacity: f32,
    /// Opacity for active/focused windows (0.0..1.0). 1.0 = fully opaque.
    #[serde(default = "default_active_opacity")]
    pub active_opacity: f32,
    /// Enable background blur behind translucent windows.
    #[serde(default)]
    pub blur_enabled: bool,
    /// Blur strength / number of passes (1..5). Higher = more blur.
    #[serde(default = "default_blur_strength")]
    pub blur_strength: u32,
    /// Automatically reduce blur quality during animations/transitions for better performance.
    #[serde(default = "default_true")]
    pub blur_quality_auto: bool,
    /// Enable fade-in/fade-out when windows map/unmap.
    #[serde(default)]
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
    /// changes are always honored — only the modeset is gated, because a bad
    /// mode can leave the output black with no in-protocol confirmation path.
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
    #[serde(default)]
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
    /// Enable HDR (High Dynamic Range) output with tone mapping.
    #[serde(default)]
    pub hdr_enabled: bool,
    /// Target display peak luminance in nits (400=HDR400, 600=HDR600, 1000=HDR1000).
    #[serde(default = "default_hdr_peak_nits")]
    pub hdr_peak_nits: f32,
    /// Tone mapping method: "none", "reinhard", "aces".
    #[serde(default = "default_tone_mapping_method")]
    pub tone_mapping_method: String,
    /// Apply per-surface wp-color-management transforms (decode →
    /// gamut-matrix → encode) in the window shader. Default off — the
    /// render-path math has unit-test coverage but no HW visual
    /// verification yet. When this flag is off the gate-on path is
    /// dead-stripped at runtime and pixels render identically to the
    /// pre-color-management pipeline.
    #[serde(default)]
    pub color_management_render_path: bool,

    /// SOTA #2: composite in scene-linear space (FP16 scene/blur FBOs,
    /// window shader decode-only, final encode at output). When this flag
    /// is off, compositing happens in display-encoded space (the historical
    /// path; gamut-correct only for sRGB-on-sRGB). When on, blending and
    /// blur become physically correct across mixed-gamut surfaces, at the
    /// cost of doubled scene/blur FBO memory bandwidth. Requires
    /// `color_management_render_path` to also be on; ignored otherwise.
    /// Default off pending HW visual verification.
    #[serde(default)]
    pub scene_linear_compositing: bool,

    /// Offload the final sRGB OETF encode to the CRTC's `GAMMA_LUT` hardware
    /// pipeline. Active only when every connected, DPMS-on output exposes
    /// `GAMMA_LUT` with size ≥ 256; otherwise the shader encode runs. The
    /// offload is all-or-nothing per frame to keep multi-output sessions
    /// consistent (never half-encoded across screens). Bit-identical to
    /// gate-off when no output supports it. Default off pending HW visual A/B.
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

    // --- Screen edge glow ---
    #[serde(default)]
    pub edge_glow: bool,
    #[serde(default = "default_edge_glow_color")]
    pub edge_glow_color: [f32; 4],
    #[serde(default = "default_edge_glow_width")]
    pub edge_glow_width: f32,

    // --- Attention animation (urgent pulse) ---
    #[serde(default)]
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
    #[serde(default)]
    pub window_tabs: bool,
    #[serde(default = "default_tab_bar_height")]
    pub tab_bar_height: f32,
    #[serde(default = "default_tab_bar_color")]
    pub tab_bar_color: [f32; 4],
    #[serde(default = "default_tab_active_color")]
    pub tab_active_color: [f32; 4],

    // --- Motion trail (drag ghosting) ---
    /// Enable motion trail ghost copies when dragging windows.
    #[serde(default)]
    pub motion_trail: bool,
    /// Number of ghost frames in the motion trail.
    #[serde(default = "default_motion_trail_frames")]
    pub motion_trail_frames: u32,
    /// Base opacity for motion trail ghosts (0.0..1.0).
    #[serde(default = "default_motion_trail_opacity")]
    pub motion_trail_opacity: f32,

    // --- Genie minimize animation ---
    /// Enable genie/magic lamp minimize animation.
    #[serde(default)]
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
    #[serde(default)]
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

    // --- Wallpaper ---
    /// Path to wallpaper image file (empty = solid black background).
    /// Used as the default wallpaper for all monitors unless overridden by wallpaper_monitors.
    #[serde(default)]
    pub wallpaper: String,
    /// Wallpaper display mode: "fill" (crop to fill), "fit" (letterbox), "stretch", "center".
    #[serde(default = "default_wallpaper_mode")]
    pub wallpaper_mode: String,
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
    #[serde(default)]
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
fn default_one() -> f32 {
    1.0
}
fn default_shadow_bottom_extra() -> f32 {
    4.0
}
fn default_transition_mode() -> String {
    "none".to_string()
}
fn default_compositor_api() -> String {
    "egl".to_string()
}
fn default_vsync_method() -> String {
    "global".to_string()
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
fn default_tab_bar_color() -> [f32; 4] {
    [0.15, 0.15, 0.18, 0.9]
}
fn default_tab_active_color() -> [f32; 4] {
    [0.3, 0.5, 0.9, 0.9]
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
            | "cyclelayout"
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
                },
                behavior: BehaviorConfig {
                    focus_follows_new_window: false,
                    resize_hints: true,
                    lock_fullscreen: true,
                    compositor: true,
                    compositor_api: default_compositor_api(),
                    corner_radius: default_corner_radius(),
                    shadow_enabled: default_true(),
                    shadow_radius: default_shadow_radius(),
                    shadow_offset: default_shadow_offset(),
                    shadow_color: default_shadow_color(),
                    inactive_opacity: default_inactive_opacity(),
                    active_opacity: default_active_opacity(),
                    blur_enabled: false,
                    blur_strength: default_blur_strength(),
                    blur_quality_auto: true,
                    blur_temporal_enabled: default_true(),
                    blur_temporal_mix_ratio: default_temporal_blur_ratio(),
                    blur_strength_by_hz: default_blur_strength_by_hz(),
                    blur_quality_by_monitor: default_blur_quality_by_monitor(),
                    fading: false,
                    fade_in_step: default_fade_step(),
                    fade_out_step: default_fade_step(),
                    shadow_exclude: Vec::new(),
                    opacity_rules: Vec::new(),
                    blur_exclude: Vec::new(),
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
                    border_glow_enabled: false,
                    border_glow_focused_only: true,
                    border_glow_radius: default_border_glow_radius(),
                    border_glow_intensity: default_border_glow_intensity(),
                    border_glow_color: default_border_glow_color(),
                    border_glow_include: Vec::new(),
                    border_glow_exclude: Vec::new(),
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
                    edge_glow: false,
                    edge_glow_color: default_edge_glow_color(),
                    edge_glow_width: default_edge_glow_width(),
                    attention_animation: false,
                    attention_color: default_attention_color(),
                    pip_border_color: default_pip_border_color(),
                    pip_border_width: default_pip_border_width(),
                    night_light: false,
                    night_light_temp: default_night_light_temp(),
                    night_light_start: default_night_light_start(),
                    night_light_end: default_night_light_end(),
                    night_light_transition_mins: default_night_light_transition(),
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
                    window_tabs: false,
                    tab_bar_height: default_tab_bar_height(),
                    tab_bar_color: default_tab_bar_color(),
                    tab_active_color: default_tab_active_color(),
                    // Phase 3: Visual effects
                    motion_trail: false,
                    motion_trail_frames: default_motion_trail_frames(),
                    motion_trail_opacity: default_motion_trail_opacity(),
                    genie_minimize: false,
                    genie_duration_ms: default_genie_duration(),
                    ripple_on_open: false,
                    ripple_duration: default_ripple_duration(),
                    ripple_amplitude: default_ripple_amplitude(),
                    focus_highlight: false,
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
                    swallow_enabled: false,
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
        if let Ok(cmd) = std::env::var("JWM_TERMINAL") {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                return vec![cmd.to_string()];
            }
        }
        ADVANCED_TERMINAL_PROBER
            .get_available_terminal()
            .map_or_else(
                || {
                    log::warn!("no supported terminal found; falling back to jterm4");
                    vec!["jterm4".to_string()]
                },
                |config| vec![config.command.clone()],
            )
    }

    pub fn get_scratchpad_termcmd() -> Vec<String> {
        // Check for scratchpad-specific terminal environment variable
        if let Ok(cmd) = std::env::var("JWM_SCRATCHPAD_TERMINAL") {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                return vec![cmd.to_string()];
            }
        }
        // Prefer jterm4 for scratchpad
        ADVANCED_TERMINAL_PROBER
            .get_available_terminal_with_priority(Some("jterm4"))
            .map_or_else(
                || {
                    log::warn!("no supported scratchpad terminal found; falling back to jterm4");
                    vec!["jterm4".to_string()]
                },
                |config| vec![config.command.clone()],
            )
    }

    fn convert_button_config(&self, btn_config: &ButtonConfig) -> Option<WMButton> {
        let click_type = self.parse_click_type(&btn_config.click_type)?;
        let modifiers = self.parse_modifiers(&btn_config.modifier);
        let button = MouseButton::from_u8(btn_config.button as u8);
        let function = self.parse_function(&btn_config.function)?;
        let arg = self.convert_argument(&btn_config.argument);

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
            "zoom" => Some(Jwm::zoom),

            "setlayout" => Some(Jwm::setlayout),
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
            "togglesticky" => Some(Jwm::togglesticky),
            "togglescratchpad" => Some(Jwm::togglescratchpad),
            "togglepip" => Some(Jwm::togglepip),
            "togglecompositor" => Some(Jwm::togglecompositor),
            "togglepartialdamage" => Some(Jwm::togglepartialdamage),
            "toggle_debug_hud" => Some(Jwm::toggle_debug_hud),
            "toggle_waterlily" => Some(Jwm::toggle_waterlily),
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
            ArgumentConfig::String(s) => match s.as_str() {
                "tile" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::TILE)),
                "float" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::FLOAT)),
                "monocle" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::MONOCLE)),
                "fibonacci" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::FIBONACCI)),
                "centeredmaster" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::CENTERED_MASTER)),
                "bstack" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::BSTACK)),
                "grid" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::GRID)),
                "deck" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::DECK)),
                "threecol" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::THREE_COL)),
                "tatami" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::TATAMI)),
                "fullscreen" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::FULLSCREEN)),
                "scrolling" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::SCROLLING)),
                "vstack" => jwm::WMArgEnum::Layout(Rc::new(LayoutEnum::VSTACK)),
                _ => jwm::WMArgEnum::StringVec(vec![s.clone()]),
            },
        }
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
        let arg = self.convert_argument(&key_config.argument);

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
                result.push_str("# available: slide, cube, fade, flip, zoom, stack, blinds, coverflow, helix, portal\n");
            }
            // wallpaper_mode (in [behavior])
            else if section == "behavior" && trimmed.starts_with("wallpaper_mode") {
                result.push_str("# available: fill, fit, stretch, center\n");
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
        match key {
            "appearance.border_px" => self.inner.appearance.border_px = as_u32()?,
            "appearance.gap_px" => self.inner.appearance.gap_px = as_u32()?,
            "appearance.snap" => self.inner.appearance.snap = as_u32()?,
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
        ArgumentConfig, CONFIG_WRITE_COUNTER, Config, ConfigDiagnosticLevel, ConfigError,
        GestureSwipeConfig, KeyConfig, Mods, Ordering, STATUS_BAR_NAME, TomlConfig,
        WallpaperMonitorConfig, WallpaperTagConfig, key_function_is_repeatable,
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
}
