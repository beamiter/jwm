pub use crate::backend::api::InteractionAction;
use crate::backend::api::{Backend, Geometry};
use crate::backend::common_define::WindowId;
use crate::backend::common_define::{KeySym, Mods, MouseButton};
use crate::core::layout::LayoutEnum;
use crate::core::models::ClientKey;
use std::process::Child;
use std::rc::Rc;
use std::time::Instant;
use xbar_core::shared_structures::SharedRingBuffer;

pub const WITHDRAWN_STATE: u8 = crate::backend::api::ICCCM_WITHDRAWN_STATE;
pub const STEXT_MAX_LEN: usize = 512;
pub const NORMAL_STATE: u8 = crate::backend::api::ICCCM_NORMAL_STATE;
/// ICCCM `WM_STATE` value for an iconified (minimized) client.
pub const ICONIC_STATE: u8 = crate::backend::api::ICCCM_ICONIC_STATE;

/// Keep the ICCCM state written by JWM in lockstep with its internal hidden
/// bit. Wayland property backends intentionally accept the same values as a
/// no-op, so callers do not need a backend-family branch.
#[must_use]
pub const fn wm_state_for_minimized(minimized: bool) -> u8 {
    if minimized {
        ICONIC_STATE
    } else {
        NORMAL_STATE
    }
}

/// Interpret only ICCCM IconicState as minimized. Withdrawn and the reserved
/// value `2` are deliberately not treated as restorable Dock entries.
#[must_use]
pub const fn wm_state_is_minimized(state: i64) -> bool {
    state == ICONIC_STATE as i64
}

/// Adopt minimized state written by both current and older JWM versions.
/// Older releases only persisted `_NET_WM_STATE_HIDDEN`; current releases
/// normalize either signal back to both EWMH and ICCCM during management.
#[must_use]
pub const fn wm_state_or_ewmh_is_minimized(state: i64, ewmh_hidden: bool) -> bool {
    wm_state_is_minimized(state) || ewmh_hidden
}

pub type WMFuncType = fn(
    &mut crate::jwm::Jwm,
    &mut dyn Backend,
    &WMArgEnum,
) -> Result<(), Box<dyn std::error::Error>>;

pub type MonitorIndex = i32;

#[cfg(test)]
mod wm_state_tests {
    use super::*;

    #[test]
    fn minimized_state_mapping_matches_icccm() {
        assert_eq!(WITHDRAWN_STATE, 0);
        assert_eq!(wm_state_for_minimized(false), NORMAL_STATE);
        assert_eq!(wm_state_for_minimized(true), ICONIC_STATE);
        assert!(!wm_state_is_minimized(-1));
        assert!(!wm_state_is_minimized(i64::from(WITHDRAWN_STATE)));
        assert!(!wm_state_is_minimized(i64::from(NORMAL_STATE)));
        assert!(!wm_state_is_minimized(2));
        assert!(wm_state_is_minimized(i64::from(ICONIC_STATE)));
        assert!(!wm_state_or_ewmh_is_minimized(
            i64::from(NORMAL_STATE),
            false
        ));
        assert!(wm_state_or_ewmh_is_minimized(i64::from(NORMAL_STATE), true));
        assert!(wm_state_or_ewmh_is_minimized(
            i64::from(ICONIC_STATE),
            false
        ));
    }
}

#[derive(Debug, Clone, Default)]
pub struct WMWindowGeom {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WMClickType {
    ClickClientWin,
    ClickRootWin,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WMArgEnum {
    Int(i32),
    UInt(u32),
    UInt64(u64),
    Float(f32),
    StringVec(Vec<String>),
    Layout(Rc<LayoutEnum>),
}

#[derive(Debug, Clone)]
pub struct WMButton {
    pub click_type: WMClickType,
    pub mask: Mods,
    pub button: MouseButton,
    pub func: Option<WMFuncType>,
    pub arg: WMArgEnum,
}

impl WMButton {
    pub fn new(
        click_type: WMClickType,
        mask: Mods,
        button: MouseButton,
        func: Option<WMFuncType>,
        arg_enum: WMArgEnum,
    ) -> Self {
        Self {
            click_type,
            mask,
            button,
            func,
            arg: arg_enum,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WMKey {
    pub mask: Mods,
    pub key_sym: KeySym,
    pub func_opt: Option<WMFuncType>,
    pub arg: WMArgEnum,
    /// Whether holding this binding may safely trigger it repeatedly.
    pub repeatable: bool,
}

impl WMKey {
    pub fn new(mod0: Mods, keysym: KeySym, func: Option<WMFuncType>, arg: WMArgEnum) -> Self {
        Self {
            mask: mod0,
            key_sym: keysym,
            func_opt: func,
            arg,
            repeatable: false,
        }
    }

    #[must_use]
    pub fn with_repeatable(mut self, repeatable: bool) -> Self {
        self.repeatable = repeatable;
        self
    }
}

#[derive(Debug, Clone)]
pub struct WMRule {
    pub class: String,
    pub instance: String,
    pub name: String,
    pub tags: usize,
    pub is_floating: bool,
    pub monitor: i32,
}

impl WMRule {
    pub fn new(
        class: String,
        instance: String,
        name: String,
        tags: usize,
        is_floating: bool,
        monitor: i32,
    ) -> Self {
        WMRule {
            class,
            instance,
            name,
            tags,
            is_floating,
            monitor,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InteractionState {
    pub client_key: ClientKey,
    pub action: InteractionAction,
    pub start_win_geom: Geometry,
    pub start_mouse_x: i32,
    pub start_mouse_y: i32,
    pub last_update_time: Instant,
}

#[allow(dead_code)]
pub struct SecondaryBarInstance {
    pub monitor_id: i32,
    pub shmem: SharedRingBuffer,
    pub child: Child,
    pub pid: u32,
    pub client_key: Option<ClientKey>,
    pub window: Option<WindowId>,
    pub has_focus: bool,
    pub last_spawn: Instant,
}
