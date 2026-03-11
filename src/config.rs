use arc_swap::ArcSwap;
use cfg_if::cfg_if;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use std::fmt;
use std::rc::Rc;

use crate::core::animation::Easing;
use crate::core::layout::LayoutEnum;
use crate::jwm::WMFuncType;
use crate::jwm::{self, Jwm, WMButton, WMClickType, WMKey, WMRule};
use crate::terminal_prober::ADVANCED_TERMINAL_PROBER;
use std::time::Duration;

use crate::backend::common_define::keys as k;
use crate::backend::common_define::{KeySym, Mods, MouseButton};

pub const LOAD_LOCAL_CONFIG: bool = true;

macro_rules! status_bar_config {
    ($($feature:literal => $name:literal),* $(,)?) => {
        cfg_if! {
            $(
                if #[cfg(feature = $feature)] {
                    pub const STATUS_BAR_NAME: &str = $name;
                } else
            )*
            {
                pub const STATUS_BAR_NAME: &str = "tao_softbuffer_bar";
            }
        }
    };
}
status_bar_config!(
    "dioxus_bar" => "dioxus_bar",
    "egui_bar" => "egui_bar",
    "iced_bar" => "iced_bar",
    "gtk_bar" => "gtk_bar",
    "relm_bar" => "relm_bar",
    "tauri_react_bar" => "tauri_react_bar",
    "tauri_vue_bar" => "tauri_vue_bar",
    "x11rb_bar" => "x11rb_bar",
    "xcb_bar" => "xcb_bar",
    "winit_softbuffer_bar" => "winit_softbuffer_bar",
    "tao_softbuffer_bar" => "tao_softbuffer_bar",
    "winit_pixels_bar" => "winit_pixels_bar",
    "tao_pixels_bar" => "tao_pixels_bar",
    "winit_wgpu_bar" => "winit_wgpu_bar",
    "tao_wgpu_bar" => "tao_wgpu_bar",
);

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
    pub dmenu_font: String,
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
    /// Window classes to exclude from rounded corners.
    #[serde(default)]
    pub rounded_corners_exclude: Vec<String>,
    /// Detect windows that manage their own opacity (skip forced opacity).
    #[serde(default)]
    pub detect_client_opacity: bool,
    /// Unredirect fullscreen windows for direct scanout (better perf).
    #[serde(default = "default_true")]
    pub fullscreen_unredirect: bool,

    // --- Feature 1: Window borders ---
    /// Enable window border/outline rendering.
    #[serde(default)]
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

    // --- Feature 11: Performance debug HUD ---
    /// Show FPS / frame time debug overlay.
    #[serde(default)]
    pub debug_hud: bool,

    // --- Feature 13: Blur mask / frame extents ---
    /// Exclude window frame/title area from blur (use _NET_FRAME_EXTENTS).
    #[serde(default)]
    pub blur_use_frame_extents: bool,

    // --- Feature 14: Shadow shape / non-uniform offset ---
    /// Extra shadow offset for bottom edge (heavier shadow below).
    #[serde(default = "default_shadow_bottom_extra")]
    pub shadow_bottom_extra: f32,

    // --- Tag-switch transition mode ---
    /// Workspace switch transition mode: "slide" (default) or "cube".
    #[serde(default = "default_transition_mode")]
    pub transition_mode: String,

    // --- Window open/close scale animation ---
    #[serde(default = "default_true")]
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
}

fn default_corner_radius() -> f32 { 10.0 }
fn default_true() -> bool { true }
fn default_shadow_radius() -> f32 { 24.0 }
fn default_shadow_offset() -> [f32; 2] { [4.0, 4.0] }
fn default_shadow_color() -> [f32; 4] { [0.0, 0.0, 0.0, 0.5] }
fn default_inactive_opacity() -> f32 { 0.92 }
fn default_active_opacity() -> f32 { 1.0 }
fn default_blur_strength() -> u32 { 3 }
fn default_fade_step() -> f32 { 0.03 }
fn default_border_width() -> f32 { 2.0 }
fn default_border_color_focused() -> [f32; 4] { [0.4, 0.6, 0.9, 1.0] }
fn default_border_color_unfocused() -> [f32; 4] { [0.3, 0.3, 0.3, 0.6] }
fn default_one() -> f32 { 1.0 }
fn default_shadow_bottom_extra() -> f32 { 4.0 }
fn default_transition_mode() -> String { "slide".to_string() }
fn default_window_animation_scale() -> f32 { 0.85 }
fn default_edge_glow_color() -> [f32; 4] { [0.3, 0.5, 1.0, 0.6] }
fn default_edge_glow_width() -> f32 { 50.0 }
fn default_attention_color() -> [f32; 4] { [1.0, 0.4, 0.1, 1.0] }
fn default_pip_border_color() -> [f32; 4] { [0.0, 0.8, 1.0, 0.8] }
fn default_pip_border_width() -> f32 { 3.0 }
fn default_night_light_temp() -> f32 { 0.4 }
fn default_night_light_start() -> String { "20:00".to_string() }
fn default_night_light_end() -> String { "06:00".to_string() }
fn default_night_light_transition() -> u32 { 30 }
fn default_magnifier_radius() -> f32 { 100.0 }
fn default_magnifier_zoom() -> f32 { 2.0 }
fn default_tilt_amount() -> f32 { 5.0 }
fn default_frosted_glass_strength() -> u32 { 2 }
fn default_overview_gap() -> f32 { 20.0 }
fn default_wobbly_stiffness() -> f32 { 10.0 }
fn default_wobbly_damping() -> f32 { 5.0 }
fn default_wobbly_grid_size() -> u32 { 8 }
fn default_particle_count() -> u32 { 150 }
fn default_particle_lifetime() -> f32 { 0.8 }
fn default_particle_gravity() -> f32 { 800.0 }

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
}

impl AnimationConfig {
    pub fn default_value() -> Self {
        Self {
            enabled: true,
            duration_ms: 150,
            easing: "ease-out".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyBindingsConfig {
    pub modkey: String, // "Mod1", "Mod4", etc.
    pub keys: Vec<KeyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyConfig {
    pub modifier: Vec<String>, // ["Mod1", "Shift"]
    pub key: String,           // "Return", "j", "k", etc.
    pub function: String,      // "spawn", "focusstack", etc.
    pub argument: ArgumentConfig,
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
                    dmenu_font: "SauceCodePro Nerd Font Regular 11".to_string(),
                    status_bar_padding: 5,
                    status_bar_height: 42,
                },
                behavior: BehaviorConfig {
                    focus_follows_new_window: false,
                    resize_hints: true,
                    lock_fullscreen: true,
                    compositor: true,
                    corner_radius: default_corner_radius(),
                    shadow_enabled: default_true(),
                    shadow_radius: default_shadow_radius(),
                    shadow_offset: default_shadow_offset(),
                    shadow_color: default_shadow_color(),
                    inactive_opacity: default_inactive_opacity(),
                    active_opacity: default_active_opacity(),
                    blur_enabled: true,
                    blur_strength: default_blur_strength(),
                    fading: false,
                    fade_in_step: default_fade_step(),
                    fade_out_step: default_fade_step(),
                    shadow_exclude: Vec::new(),
                    opacity_rules: Vec::new(),
                    blur_exclude: Vec::new(),
                    rounded_corners_exclude: Vec::new(),
                    detect_client_opacity: false,
                    fullscreen_unredirect: true,
                    border_enabled: false,
                    border_width: default_border_width(),
                    border_color_focused: default_border_color_focused(),
                    border_color_unfocused: default_border_color_unfocused(),
                    corner_radius_rules: Vec::new(),
                    scale_rules: Vec::new(),
                    color_temperature: 0.0,
                    saturation: default_one(),
                    brightness: default_one(),
                    contrast: default_one(),
                    invert_colors: false,
                    grayscale: false,
                    debug_hud: false,
                    blur_use_frame_extents: false,
                    shadow_bottom_extra: default_shadow_bottom_extra(),
                    transition_mode: default_transition_mode(),
                    window_animation: default_true(),
                    window_animation_scale: default_window_animation_scale(),
                    inactive_dim: default_one(),
                    edge_glow: false,
                    edge_glow_color: default_edge_glow_color(),
                    edge_glow_width: default_edge_glow_width(),
                    attention_animation: default_true(),
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
                    frosted_glass_rules: Vec::new(),
                    frosted_glass_strength: default_frosted_glass_strength(),
                    overview_enabled: default_true(),
                    overview_thumbnail_gap: default_overview_gap(),
                    wobbly_windows: false,
                    wobbly_stiffness: default_wobbly_stiffness(),
                    wobbly_damping: default_wobbly_damping(),
                    wobbly_grid_size: default_wobbly_grid_size(),
                    particle_effects: false,
                    particle_count: default_particle_count(),
                    particle_lifetime: default_particle_lifetime(),
                    particle_gravity: default_particle_gravity(),
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
        let is_x11 = matches!(
            std::env::var("JWM_BACKEND").as_deref(),
            Err(_) | Ok("x11")
        );

        let dmenu_cmd = if is_x11 {
            vec!["dmenu_run".to_string()]
        } else {
            vec![
                "fuzzel".to_string(),
                "--font=SauceCodePro Nerd Font Regular:size=11".to_string(),
                "--background=2e3440ff".to_string(),
                "--text-color=d8dee9ff".to_string(),
                "--selection-color=81a1c1ff".to_string(),
                "--selection-text-color=eceff4ff".to_string(),
            ]
        };

        vec![
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "e".to_string(),
                function: "spawn".to_string(),
                argument: ArgumentConfig::StringVec(dmenu_cmd.clone()),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string()],
                key: "r".to_string(),
                function: "spawn".to_string(),
                argument: ArgumentConfig::StringVec(dmenu_cmd),
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
                modifier: vec!["Mod1".to_string()],
                key: "Tab".to_string(),
                function: "toggle_overview".to_string(),
                argument: ArgumentConfig::Int(1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "Tab".to_string(),
                function: "toggle_overview".to_string(),
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
                argument: ArgumentConfig::StringVec(vec!["music".to_string(), "spotify".to_string()]),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
                key: "c".to_string(),
                function: "togglescratchpad".to_string(),
                argument: ArgumentConfig::StringVec(vec!["calc".to_string(), "qalculate-gtk".to_string()]),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Shift".to_string()],
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
            // Scrolling layout: consume/expel
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
                modifier: vec!["Mod1".to_string(), "Control".to_string(), "Shift".to_string()],
                key: "h".to_string(),
                function: "scrolling_expel".to_string(),
                argument: ArgumentConfig::Int(-1),
            },
            KeyConfig {
                modifier: vec!["Mod1".to_string(), "Control".to_string(), "Shift".to_string()],
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
        vec![
            // RuleConfig {
            //     name: "Feishu Meetings".to_string(),
            //     class: "".to_string(),
            //     instance: "".to_string(),
            //     tags: 0,
            //     is_floating: true,
            //     monitor: -1,
            // },
            // RuleConfig {
            //     name: "飞书会议".to_string(),
            //     class: "".to_string(),
            //     instance: "".to_string(),
            //     tags: 0,
            //     is_floating: true,
            //     monitor: -1,
            // },
        ]
    }

    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let config: TomlConfig = toml::from_str(&content)?;
        Ok(Self { inner: config })
    }

    pub fn load_default() -> Self {
        // 如果配置文件不存在，使用默认配置
        let default_config_path = dirs::config_dir()
            .unwrap_or_else(|| std::env::current_dir().unwrap())
            .join("jwm")
            .join("config.toml");

        Self::load_from_file(&default_config_path).unwrap_or_else(|_| Self::default())
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

    pub fn dmenu_font(&self) -> &str {
        &self.inner.appearance.dmenu_font
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
        self.inner.layout.tags_length
    }

    pub fn tagmask(&self) -> u32 {
        (1 << self.tags_length()) - 1
    }

    pub fn animation_enabled(&self) -> bool {
        self.inner.animation.enabled
    }

    pub fn animation_duration(&self) -> Duration {
        Duration::from_millis(self.inner.animation.duration_ms)
    }

    pub fn animation_easing(&self) -> Easing {
        Easing::from_str(&self.inner.animation.easing)
    }

    pub fn get_keys(&self) -> Vec<WMKey> {
        let mut keys = Vec::new();

        for key_config in &self.inner.keybindings.keys {
            if let Some(key) = self.convert_key_config(key_config) {
                keys.push(key);
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

    pub fn get_dmenucmd(&self) -> Vec<String> {
        self.inner
            .keybindings
            .keys
            .iter()
            .find(|k| k.function == "spawn" && (k.key == "e" || k.key == "r"))
            .and_then(|k| match &k.argument {
                ArgumentConfig::StringVec(cmd) => Some(cmd.clone()),
                _ => None,
            })
            .unwrap_or_else(|| {
                let is_x11 = matches!(
                    std::env::var("JWM_BACKEND").as_deref(),
                    Err(_) | Ok("x11")
                );
                if is_x11 {
                    vec!["dmenu_run".to_string()]
                } else {
                    vec!["fuzzel".to_string()]
                }
            })
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
            .map(|config| vec![config.command.clone()])
            .unwrap_or_else(|| {
                println!("terminator fallback");
                vec!["x-terminal-emulator".to_string()]
            })
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
            "focusstack" => Some(Jwm::focusstack),
            "focusmon" => Some(Jwm::focusmon),
            "take_screenshot" => Some(Jwm::take_screenshot),
            "quit" => Some(Jwm::quit),
            "restart" => Some(Jwm::restart),
            "killclient" => Some(Jwm::killclient),
            "zoom" => Some(Jwm::zoom),

            "setlayout" => Some(Jwm::setlayout),
            "togglefloating" => Some(Jwm::togglefloating),
            "togglebar" => Some(Jwm::togglebar),
            "setmfact" => Some(Jwm::setmfact),
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
            "toggle_overview" => Some(Jwm::toggle_overview),
            "cycle_overview" => Some(Jwm::cycle_overview),
            "toggle_magnifier" => Some(Jwm::toggle_magnifier),

            "scrolling_focus_column" => Some(Jwm::scrolling_focus_column),
            "scrolling_move_column" => Some(Jwm::scrolling_move_column),
            "scrolling_consume" => Some(Jwm::scrolling_consume),
            "scrolling_expel" => Some(Jwm::scrolling_expel),
            "scrolling_focus_window" => Some(Jwm::scrolling_focus_window),

            _ => {
                eprintln!("Unknown function: {}", func_name);
                None
            }
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
            _ => {
                eprintln!("Unknown key: {}", key);
                return None;
            }
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

        Some(WMKey::new(modifiers, keysym, Some(function), arg))
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

        let modkey = self.parse_modifiers(&[self.inner.keybindings.modkey.clone()]);
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
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, toml_string)?;
        Ok(())
    }

    pub fn save_default(&self) -> Result<(), ConfigError> {
        let config_path = Self::get_default_config_path();
        self.save_to_file(config_path)
    }

    pub fn get_default_config_path() -> std::path::PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| std::env::current_dir().unwrap())
            .join("jwm")
            .join("config.toml")
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

    pub fn validate_config_file<P: AsRef<Path>>(path: P) -> Result<(), ConfigError> {
        let content = fs::read_to_string(path)?;
        let _config: TomlConfig = toml::from_str(&content)?;
        Ok(())
    }

    pub fn merge_config(&mut self, other: TomlConfig) {
        self.inner = other;
    }

    pub fn reload(&mut self) -> Result<(), ConfigError> {
        let config_path = Self::get_default_config_path();
        if config_path.exists() {
            let new_config = Self::load_from_file(&config_path)?;
            self.inner = new_config.inner;
        }
        Ok(())
    }

    pub fn config_exists() -> bool {
        Self::get_default_config_path().exists()
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
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(err) => write!(f, "IO error: {}", err),
            ConfigError::Parse(err) => write!(f, "Parse error: {}", err),
            ConfigError::Serialize(err) => write!(f, "Serialize error: {}", err),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(err) => Some(err),
            ConfigError::Parse(err) => Some(err),
            ConfigError::Serialize(err) => Some(err),
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

pub static CONFIG: Lazy<ArcSwap<Config>> = Lazy::new(|| {
    let config = if !LOAD_LOCAL_CONFIG {
        Config::default()
    } else {
        if !Config::config_exists() {
            Config::generate_template(Config::get_default_config_path()).unwrap();
            println!(
                "Generated default config file at: {:?}",
                Config::get_default_config_path()
            );
        }
        let config = Config::load_default();
        println!("Configuration loaded!");
        config
    };
    ArcSwap::from_pointee(config)
});

/// Reload the global CONFIG from disk. Returns Ok on success.
pub fn reload_global() -> Result<(), ConfigError> {
    let new_config = Config::load_from_file(Config::get_default_config_path())?;
    CONFIG.store(Arc::new(new_config));
    Ok(())
}
