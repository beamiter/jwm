use crate::backend::api::{Backend, Geometry, ResizeEdge};
use crate::backend::common_define::WindowId;
use crate::backend::common_define::{KeySym, Mods, MouseButton};
use crate::core::layout::LayoutEnum;
use crate::core::models::ClientKey;
use shared_structures::SharedRingBuffer;
use std::process::Child;
use std::rc::Rc;
use std::time::Instant;

pub const WITHDRAWN_STATE: u8 = 0;
pub const STEXT_MAX_LEN: usize = 512;
pub const NORMAL_STATE: u8 = 1;
pub const ICONIC_STATE: u8 = 2;

pub type WMFuncType = fn(
    &mut crate::jwm::Jwm,
    &mut dyn Backend,
    &WMArgEnum,
) -> Result<(), Box<dyn std::error::Error>>;

pub type MonitorIndex = i32;

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
}

impl WMKey {
    pub fn new(mod0: Mods, keysym: KeySym, func: Option<WMFuncType>, arg: WMArgEnum) -> Self {
        Self {
            mask: mod0,
            key_sym: keysym,
            func_opt: func,
            arg,
        }
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

#[derive(Debug, Clone, Copy)]
pub enum InteractionAction {
    Move,
    Resize(ResizeEdge),
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
