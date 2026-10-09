//! 会话保存 / 恢复（Session save / restore）
//!
//! 将当前所有客户端的标签（tags）与浮动布局快照写入磁盘，便于在重启
//! 窗口管理器（或重新启动应用）后，把窗口重新归位到原来的标签 / 浮动状态。
//!
//! 由于重启后窗口是全新的 `WindowId`，恢复通过 class + instance 匹配实现：
//! `save_session` 写出快照，`restore_session` 读取快照并把保存的状态套用到
//! 当前已打开、且 class/instance 匹配的客户端上。
//!
//! v3 起，每个显示器额外按 `monitor_clients` 的顺序记录窗口身份列表；
//! `restore_session` 在套用完全部标签 / 浮动状态后，按保存顺序重排每个
//! 显示器的客户端列表再统一 `arrange`，从而保留用户手工调整过的平铺顺序
//! （master/stack 排列与拖拽换序的结果）。
//!
//! v4 起，最大化状态（轴、恢复矩形、是否从平铺提升）一并写入快照；恢复时
//! 先落到休息态几何，再按保存的 `maximize.promoted` 经
//! `adopt_client_maximized` 重新最大化（与 seamless 重启同源）。
//!
//! v5 起，`SessionEntry` 与 `SessionMonitorOrder` 额外记录输出的 connector /
//! `stable_key`；恢复时经 `output_map` + `enumerate_outputs` 解析到当前
//! `monitor_num`，热插拔 hole-fill 重编号后仍落到同一面板；缺/失联回退旧
//! `monitor_num`。
//!
//! v6 起，Sticky（`_NET_WM_STATE_STICKY`）一并写入快照；恢复时经
//! `set_client_sticky` 套用。缺省 / 旧版本快照为 `false`。
//!
//! v7 起，Above / Below（`_NET_WM_STATE_ABOVE` / `_BELOW`）一并写入快照；
//! 恢复时经 `apply_external_stacking_request` 套用（两者皆真时 Above 胜出）。
//! 缺省 / 旧版本快照为 `false`。
//!
//! v8 起，Minimized（语义隐藏 / Dock）一并写入快照；恢复时经
//! `set_client_minimized` 套用。缺省 / 旧版本快照为 `false`。
//!
//! v9 起，Fullscreen / PiP 一并写入快照；恢复时经 `setfullscreen` /
//! `set_client_pip` 在 stacking 之后、minimized 之前套用（两者皆真时
//! Fullscreen 胜出，与运行时互斥一致）。缺省 / 旧版本快照为 `false`。
//!
//! v10 起，`client_fact`、手浮（`is_drag_floating`）与 `no_decorations`
//! 一并写入快照；恢复时在休息态放置阶段套用。缺省 / 旧版本快照为
//! `client_fact = 1.0`、两个布尔为 `false`。最大化再套用改为按保存的
//! `maximize.promoted` 走 `adopt_client_maximized`（与 seamless 重启同源）。
//!
//! v11 起，`is_urgent` / `demands_attention` / `skip_taskbar` / `skip_pager` /
//! `is_fixed` 与可选 `border_w` 一并写入快照；恢复时在休息态放置阶段套用。
//! 缺省 / 旧版本快照布尔为 `false`、`border_w` 为 `None`（不改写边框）。
//! v12 起，`never_focus` / `old_state` / `pip_restore_sticky` /
//! `remembers_closed_placement` 一并写入；缺省 / 旧版本为 `false`。
//! v13 起，可选 `old_border_w`（fullscreen 返回边框）一并写入；缺省为 `None`。
//! v14 起，可选 `minimized_order`（Dock 恢复序）一并写入；缺省为 `None`。
//! v15 起，可选 `hidden_restore`（最小化/离视图停放几何）一并写入；缺省为 `None`。
//! v16 起，可选 `old_geometry`（fullscreen/布局返回矩形）一并写入；缺省为 `None`。
//! v17 起，可选 `hidden_x`（最小化/离视图停放 x）一并写入；缺省为 `None`。
//! 该坐标依赖输出拓扑，恢复时由当前桌面边界重新计算。

use crate::backend::api::{Backend, MaximizeAxes, NetWmAction, NetWmState};
use crate::config::CONFIG;
use crate::core::maximize::MaximizeOrigin;
use crate::core::models::{ClientKey, WMClient};
use crate::core::state::WMState;
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::closed_placement::resolve_monitor_num_by_connector;
use crate::jwm::event_dispatcher::apply_external_stacking_request;
use crate::jwm::geometry::GeometryConstraints;
use crate::jwm::maximize::resting_order;
use crate::jwm::types::WMArgEnum;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const SESSION_VERSION: u32 = 17;
const MIN_SUPPORTED_SESSION_VERSION: u32 = 1;
const MAX_SESSION_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SESSION_CLIENTS: usize = 16_384;
const MAX_SESSION_MONITORS: usize = 64;
const MAX_SESSION_IDENTITY_FIELD_BYTES: usize = 65_536;
/// Temporaries are `<prefix><pid>-<sequence>`: unique per writer, so a crash
/// between create and rename leaves one behind that nothing would ever reuse
/// or delete without the sweep in `atomic_write_session`.
const SESSION_TEMPORARY_PREFIX: &str = ".session.json.tmp-";
/// The state directory holds the snapshot and a handful of temporaries at
/// most; a sweep never walks further than this.
const MAX_SESSION_SWEEP_ENTRIES: usize = 1024;
const MAX_SESSION_TEMPORARY_CREATE_ATTEMPTS: usize = 128;
static SESSION_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 单个客户端的会话条目（按 class/instance 匹配，不持久化 WindowId）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub class: String,
    pub instance: String,
    pub name: String,
    pub tags: u32,
    pub is_floating: bool,
    pub monitor_num: u32,
    /// v5：输出身份键（[`crate::backend::api::OutputIdentity::stable_key`]，
    /// 无 EDID 时为 connector 名）。缺省 / 旧版本快照为 `None`，恢复时回退
    /// `monitor_num`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    /// 浮动几何 (x, y, w, h)；仅当窗口为浮动时记录。
    pub floating: Option<(i32, i32, i32, i32)>,
    /// v4：最大化轴与恢复矩形。缺省 / 旧版本快照为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximize: Option<SessionMaximize>,
    /// v6：`_NET_WM_STATE_STICKY`。缺省 / 旧版本快照为 `false`。
    #[serde(default)]
    pub is_sticky: bool,
    /// v7：`_NET_WM_STATE_ABOVE`。缺省 / 旧版本快照为 `false`。
    /// 与 `is_below` 同时为真时恢复侧 Above 胜出。
    #[serde(default)]
    pub is_above: bool,
    /// v7：`_NET_WM_STATE_BELOW`。缺省 / 旧版本快照为 `false`。
    #[serde(default)]
    pub is_below: bool,
    /// v8：语义 minimized（Dock / Iconic）。缺省 / 旧版本快照为 `false`。
    #[serde(default)]
    pub is_minimized: bool,
    /// v9：fullscreen。缺省 / 旧版本快照为 `false`。与 `is_pip` 同时为真时
    /// 恢复侧 Fullscreen 胜出。
    #[serde(default)]
    pub is_fullscreen: bool,
    /// v9：picture-in-picture。缺省 / 旧版本快照为 `false`。
    #[serde(default)]
    pub is_pip: bool,
    /// v10：per-window tiled share (`ClientState::client_fact`)。缺省 / 旧版本
    /// 快照为 `1.0`。
    #[serde(default = "default_client_fact")]
    pub client_fact: f32,
    /// v10：用户拖出平铺的手浮（`is_drag_floating`）。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub is_drag_floating: bool,
    /// v10：无装饰请求（`no_decorations`）。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub no_decorations: bool,
    /// v11：urgency。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub is_urgent: bool,
    /// v11：`_NET_WM_STATE_DEMANDS_ATTENTION`。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub demands_attention: bool,
    /// v11：`_NET_WM_STATE_SKIP_TASKBAR`。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub skip_taskbar: bool,
    /// v11：`_NET_WM_STATE_SKIP_PAGER`。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub skip_pager: bool,
    /// v11：size-hints fixed。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub is_fixed: bool,
    /// v11：drawn border width。缺省 / 旧版本为 `None`（恢复时不改写）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_w: Option<i32>,
    /// v12：`WM_HINTS` input / never-focus chrome。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub never_focus: bool,
    /// v12：fullscreen / PiP 下记住的 floating bit（`old_state`）。缺省为 `false`。
    #[serde(default)]
    pub old_state: bool,
    /// v12：离开 PiP 时恢复的 sticky bit。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub pip_restore_sticky: bool,
    /// v12：关闭放置记忆资格。缺省 / 旧版本为 `false`。
    #[serde(default)]
    pub remembers_closed_placement: bool,
    /// v13：fullscreen 返回边框宽度（`old_border_w`）。缺省 / 旧版本为 `None`
    ///（恢复时不改写）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_border_w: Option<i32>,
    /// v14：Dock / iconic 恢复序（`minimized_order`）。缺省 / 旧版本为 `None`
    ///（恢复时不改写顺序）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimized_order: Option<u64>,
    /// v15：最小化 / 离视图停放几何（`hidden_restore_rect`）。缺省 / 旧版本为
    /// `None`（恢复时不改写）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_restore: Option<(i32, i32, i32, i32)>,
    /// v16：fullscreen / 布局返回矩形（`old_x/y/w/h`）。缺省 / 旧版本为
    /// `None`（恢复时不改写）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_geometry: Option<(i32, i32, i32, i32)>,
    /// v17：最小化 / 离视图停放 x（`hidden_x`）。缺省 / 旧版本为 `None`。
    /// 这是捕获时的诊断状态；恢复必须按当前输出拓扑重新计算停放位置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_x: Option<i32>,
}

fn default_client_fact() -> f32 {
    1.0
}

fn sanitize_client_fact(fact: f32) -> f32 {
    if !fact.is_finite() {
        return 1.0;
    }
    fact.clamp(0.25, 4.0)
}

/// 会话里保存的最大化状态（v4）。休息态几何仍写在 `is_floating` /
/// `floating`；本结构在恢复末尾再套一层 maximize。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMaximize {
    pub vert: bool,
    pub horz: bool,
    /// 取消最大化后回到的内容矩形；缺省时由事务用休息态推算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<(i32, i32, i32, i32)>,
    /// `maximize_restore_tiled`：用户从平铺提升。
    #[serde(default)]
    pub promoted: bool,
}

impl SessionMaximize {
    fn from_client(client: &WMClient) -> Option<Self> {
        let axes = client.state.maximized_axes();
        if !axes.any() {
            return None;
        }
        let restore = client
            .geometry
            .maximize_restore_rect
            .filter(|rect| rect.w > 0 && rect.h > 0)
            .map(|rect| (rect.x, rect.y, rect.w, rect.h));
        Some(Self {
            vert: axes.vert,
            horz: axes.horz,
            restore,
            promoted: client.state.maximize_restore_tiled,
        })
    }

    fn axes(&self) -> MaximizeAxes {
        MaximizeAxes::new(self.vert, self.horz)
    }

    fn restore_hint(&self) -> Option<Rect> {
        self.restore
            .filter(|&(_, _, w, h)| w > 0 && h > 0)
            .map(|(x, y, w, h)| Rect::new(x, y, w, h))
    }
}

/// 恢复匹配用的窗口身份（class + instance；重启后 `WindowId` 失效，不持久化）。
///
/// 同时是 `clients` 条目与每显示器顺序列表共用的匹配键。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionWindowIdentity {
    pub class: String,
    pub instance: String,
}

impl SessionWindowIdentity {
    /// 顺序恢复的身份匹配：class 与 instance 都忽略大小写相等。
    ///
    /// 刻意比 `plan_restore` 的标签匹配更严格（不使用空 instance 回退）：
    /// 身份不确定时把窗口当作新窗口追加，而不是把它猜进一个可能错误的
    /// 位置。
    fn matches(&self, other: &Self) -> bool {
        self.class.eq_ignore_ascii_case(&other.class)
            && self.instance.eq_ignore_ascii_case(&other.instance)
    }
}

/// 单个显示器的平铺顺序快照（v3 新增）：`monitor_clients` 顺序导出的
/// 窗口身份列表，组内（平铺 / 浮动）相对序即用户看到的排列。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMonitorOrder {
    pub monitor_num: u32,
    /// v5：该显示器对应输出的 connector / `stable_key`。缺省时回退
    /// `monitor_num`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    pub clients: Vec<SessionWindowIdentity>,
}

/// 整个会话快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub clients: Vec<SessionEntry>,
    /// v3 新增：每个显示器按平铺顺序记录的窗口身份。v1/v2 迁移而来时为空，
    /// 恢复行为与旧版本一致（不重排）。
    pub monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本探测：只读取 `version` 字段，用来选择迁移入口。
#[derive(Deserialize)]
struct SessionVersionProbe {
    version: u32,
}

/// 版本 1 条目的宽容表示。
///
/// 除匹配所需的 class/instance 外全部可缺省：历史 v1 构建可能缺少后来
/// 加入的字段，而迁移的职责是尽量保留可用状态，而不是拒绝整个快照。
#[derive(Deserialize)]
struct SessionEntryV1 {
    class: String,
    instance: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    tags: u32,
    #[serde(default)]
    is_floating: bool,
    #[serde(default)]
    monitor_num: u32,
    #[serde(default)]
    floating: Option<(i32, i32, i32, i32)>,
}

#[derive(Deserialize)]
struct SessionSnapshotV1 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    clients: Vec<SessionEntryV1>,
}

/// 版本 2 快照：条目与当前同形（`maximize` 缺省为 None），但还没有每显示器顺序列表。
///
/// 保持严格（字段无缺省）：v2 是直系前身，缺字段说明写入方出了
/// 问题，迁移不做静默补全。
#[derive(Deserialize)]
struct SessionSnapshotV2 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
}

/// 版本 3 快照：已有 monitor_orders，尚无 maximize / connector 字段
/// （反序列化时缺省 None）。
#[derive(Deserialize)]
struct SessionSnapshotV3 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本 4 快照：已有 maximize，尚无 connector（反序列化时缺省 None）。
#[derive(Deserialize)]
struct SessionSnapshotV4 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本 5 快照：已有 connector，尚无 is_sticky（反序列化时缺省 false）。
#[derive(Deserialize)]
struct SessionSnapshotV5 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本 6 快照：已有 is_sticky，尚无 is_above / is_below（反序列化时缺省 false）。
#[derive(Deserialize)]
struct SessionSnapshotV6 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本 7 快照：已有 is_above / is_below，尚无 is_minimized（反序列化时缺省 false）。
#[derive(Deserialize)]
struct SessionSnapshotV7 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本 8 快照：已有 is_minimized，尚无 is_fullscreen / is_pip（反序列化时缺省 false）。
#[derive(Deserialize)]
struct SessionSnapshotV8 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本 9 快照：已有 is_fullscreen / is_pip，尚无 client_fact / hand-float /
/// no_decorations（反序列化时缺省）。
#[derive(Deserialize)]
struct SessionSnapshotV9 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本 10 快照：已有 client_fact / hand-float / no_decorations，尚无
/// urgency / attention / skip_* / is_fixed / border_w（反序列化时缺省）。
#[derive(Deserialize)]
struct SessionSnapshotV10 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本 11 快照：已有 urgency / attention / skip_* / is_fixed / border_w，
/// 尚无 never_focus / old_state / pip_restore_sticky / remembers_closed_placement
/// （反序列化时缺省）。
#[derive(Deserialize)]
struct SessionSnapshotV11 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 版本 12 快照：已有 never_focus / old_state / pip_restore_sticky /
/// remembers_closed_placement，尚无 old_border_w（反序列化时缺省）。
#[derive(Deserialize)]
struct SessionSnapshotV12 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// v13：含 old_border_w，尚无 minimized_order（反序列化时缺省）。
#[derive(Deserialize)]
struct SessionSnapshotV13 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// v14：含 minimized_order，尚无 hidden_restore（反序列化时缺省）。
#[derive(Deserialize)]
struct SessionSnapshotV14 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// v15：含 hidden_restore，尚无 old_geometry（反序列化时缺省）。
#[derive(Deserialize)]
struct SessionSnapshotV15 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// v16：含 old_geometry，尚无 hidden_x（反序列化时缺省）。
#[derive(Deserialize)]
struct SessionSnapshotV16 {
    #[allow(dead_code)]
    version: u32,
    clients: Vec<SessionEntry>,
    monitor_orders: Vec<SessionMonitorOrder>,
}

/// 把任一受支持版本的会话 JSON 迁移为当前版本的快照。
///
/// 崩溃安全约定：迁移是纯内存操作，绝不改写磁盘上的旧快照；升级后的
/// 状态只有在下一次 `save_session` 原子写成功时才落盘。不认识的版本
/// 返回错误而不产生部分状态，磁盘文件保持原样以便回滚到旧版本 JWM。
///
/// # Errors
///
/// JSON 无法解析、版本不受支持、或迁移结果未通过快照校验时返回错误。
pub fn migrate_session_json(json: &str) -> Result<SessionSnapshot, String> {
    let probe: SessionVersionProbe = serde_json::from_str(json)
        .map_err(|error| format!("session snapshot has no readable version: {error}"))?;
    let snapshot = match probe.version {
        1 => {
            let v1: SessionSnapshotV1 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 1 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(migrate_snapshot_v9(migrate_snapshot_v8(
                        migrate_snapshot_v7(migrate_snapshot_v6(migrate_snapshot_v5(
                            migrate_snapshot_v4(migrate_snapshot_v3(migrate_snapshot_v2(
                                migrate_snapshot_v1(v1),
                            ))),
                        ))),
                    ))),
                ))),
            )))
        }
        2 => {
            let v2: SessionSnapshotV2 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 2 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(migrate_snapshot_v9(migrate_snapshot_v8(
                        migrate_snapshot_v7(migrate_snapshot_v6(migrate_snapshot_v5(
                            migrate_snapshot_v4(migrate_snapshot_v3(migrate_snapshot_v2(v2))),
                        ))),
                    ))),
                ))),
            )))
        }
        3 => {
            let v3: SessionSnapshotV3 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 3 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(migrate_snapshot_v9(migrate_snapshot_v8(
                        migrate_snapshot_v7(migrate_snapshot_v6(migrate_snapshot_v5(
                            migrate_snapshot_v4(migrate_snapshot_v3(v3)),
                        ))),
                    ))),
                ))),
            )))
        }
        4 => {
            let v4: SessionSnapshotV4 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 4 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(migrate_snapshot_v9(migrate_snapshot_v8(
                        migrate_snapshot_v7(migrate_snapshot_v6(migrate_snapshot_v5(
                            migrate_snapshot_v4(v4),
                        ))),
                    ))),
                ))),
            )))
        }
        5 => {
            let v5: SessionSnapshotV5 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 5 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(migrate_snapshot_v9(migrate_snapshot_v8(
                        migrate_snapshot_v7(migrate_snapshot_v6(migrate_snapshot_v5(v5))),
                    ))),
                ))),
            )))
        }
        6 => {
            let v6: SessionSnapshotV6 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 6 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(migrate_snapshot_v9(migrate_snapshot_v8(
                        migrate_snapshot_v7(migrate_snapshot_v6(v6)),
                    ))),
                ))),
            )))
        }
        7 => {
            let v7: SessionSnapshotV7 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 7 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(migrate_snapshot_v9(migrate_snapshot_v8(
                        migrate_snapshot_v7(v7),
                    ))),
                ))),
            )))
        }
        8 => {
            let v8: SessionSnapshotV8 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 8 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(migrate_snapshot_v9(migrate_snapshot_v8(v8))),
                ))),
            )))
        }
        9 => {
            let v9: SessionSnapshotV9 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 9 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(migrate_snapshot_v9(v9)),
                ))),
            )))
        }
        10 => {
            let v10: SessionSnapshotV10 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 10 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(
                    migrate_snapshot_v10(v10),
                ))),
            )))
        }
        11 => {
            let v11: SessionSnapshotV11 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 11 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(migrate_snapshot_v11(v11))),
            )))
        }
        12 => {
            let v12: SessionSnapshotV12 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 12 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(migrate_snapshot_v12(v12)),
            )))
        }
        13 => {
            let v13: SessionSnapshotV13 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 13 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(
                migrate_snapshot_v13(v13),
            )))
        }
        14 => {
            let v14: SessionSnapshotV14 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 14 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(migrate_snapshot_v14(v14)))
        }
        15 => {
            let v15: SessionSnapshotV15 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 15 session snapshot: {error}"))?;
            migrate_snapshot_v16(migrate_snapshot_v15(v15))
        }
        16 => {
            let v16: SessionSnapshotV16 = serde_json::from_str(json)
                .map_err(|error| format!("cannot parse version 16 session snapshot: {error}"))?;
            migrate_snapshot_v16(v16)
        }
        SESSION_VERSION => SessionSnapshot::from_json(json)
            .map_err(|error| format!("cannot parse session snapshot: {error}"))?,
        other => {
            return Err(format!(
                "unsupported session version {other}; supported versions are \
                 {MIN_SUPPORTED_SESSION_VERSION}..={SESSION_VERSION}"
            ));
        }
    };

    snapshot.validate()?;
    Ok(snapshot)
}

/// v1 -> v2：磁盘字段相同，但 v1 从未校验浮动几何。归一化不合法的
/// 浮动状态（非浮动窗口或非正尺寸），而不是拒绝整个快照。
fn migrate_snapshot_v1(v1: SessionSnapshotV1) -> SessionSnapshotV2 {
    let clients = v1
        .clients
        .into_iter()
        .map(|entry| {
            let floating = entry
                .floating
                .filter(|&(_, _, width, height)| entry.is_floating && width > 0 && height > 0);
            SessionEntry {
                class: entry.class,
                instance: entry.instance,
                name: entry.name,
                tags: entry.tags,
                is_floating: entry.is_floating,
                monitor_num: entry.monitor_num,
                connector: None,
                floating,
                maximize: None,
                is_sticky: false,
                is_above: false,
                is_below: false,
                is_minimized: false,
                is_fullscreen: false,
                is_pip: false,
                client_fact: 1.0,
                is_drag_floating: false,
                no_decorations: false,
                is_urgent: false,
                demands_attention: false,
                skip_taskbar: false,
                skip_pager: false,
                is_fixed: false,
                border_w: None,
                never_focus: false,
                old_state: false,
                pip_restore_sticky: false,
                remembers_closed_placement: false,
                old_border_w: None,
                minimized_order: None,
                hidden_restore: None,
                old_geometry: None,
                hidden_x: None,
            }
        })
        .collect();
    SessionSnapshotV2 {
        version: 2,
        clients,
    }
}

/// v2 -> v3：v2 没有每显示器顺序列表，补空列表即可；恢复时空列表意味着
/// 不重排，行为与 v2 完全一致。
fn migrate_snapshot_v2(v2: SessionSnapshotV2) -> SessionSnapshotV3 {
    SessionSnapshotV3 {
        version: 3,
        clients: v2.clients,
        monitor_orders: Vec::new(),
    }
}

/// v3 -> v4：maximize 字段在反序列化时已缺省为 None；只抬版本号。
fn migrate_snapshot_v3(v3: SessionSnapshotV3) -> SessionSnapshotV4 {
    SessionSnapshotV4 {
        version: 4,
        clients: v3.clients,
        monitor_orders: v3.monitor_orders,
    }
}

/// v4 -> v5：connector 字段在反序列化时已缺省为 None；只抬版本号。
fn migrate_snapshot_v4(v4: SessionSnapshotV4) -> SessionSnapshotV5 {
    SessionSnapshotV5 {
        version: 5,
        clients: v4.clients,
        monitor_orders: v4.monitor_orders,
    }
}

/// v5 -> v6：is_sticky 字段在反序列化时已缺省为 false；只抬版本号。
fn migrate_snapshot_v5(v5: SessionSnapshotV5) -> SessionSnapshotV6 {
    SessionSnapshotV6 {
        version: 6,
        clients: v5.clients,
        monitor_orders: v5.monitor_orders,
    }
}

/// v6 -> v7：is_above / is_below 字段在反序列化时已缺省为 false；只抬版本号。
fn migrate_snapshot_v6(v6: SessionSnapshotV6) -> SessionSnapshotV7 {
    SessionSnapshotV7 {
        version: 7,
        clients: v6.clients,
        monitor_orders: v6.monitor_orders,
    }
}

/// v7 -> v8：is_minimized 字段在反序列化时已缺省为 false；只抬版本号。
fn migrate_snapshot_v7(v7: SessionSnapshotV7) -> SessionSnapshotV8 {
    SessionSnapshotV8 {
        version: 8,
        clients: v7.clients,
        monitor_orders: v7.monitor_orders,
    }
}

/// v8 -> v9：is_fullscreen / is_pip 字段在反序列化时已缺省为 false；只抬版本号。
fn migrate_snapshot_v8(v8: SessionSnapshotV8) -> SessionSnapshotV9 {
    SessionSnapshotV9 {
        version: 9,
        clients: v8.clients,
        monitor_orders: v8.monitor_orders,
    }
}

/// v9 -> v10：client_fact / hand-float / no_decorations 在反序列化时已缺省；只抬版本号。
fn migrate_snapshot_v9(v9: SessionSnapshotV9) -> SessionSnapshotV10 {
    SessionSnapshotV10 {
        version: 10,
        clients: v9.clients,
        monitor_orders: v9.monitor_orders,
    }
}

/// v10 -> v11：urgency / attention / skip_* / is_fixed / border_w 在反序列化时已缺省；只抬版本号。
fn migrate_snapshot_v10(v10: SessionSnapshotV10) -> SessionSnapshotV11 {
    SessionSnapshotV11 {
        version: 11,
        clients: v10.clients,
        monitor_orders: v10.monitor_orders,
    }
}

/// v11 -> v12：never_focus / old_state / pip_restore_sticky /
/// remembers_closed_placement 在反序列化时已缺省；只抬版本号。
fn migrate_snapshot_v11(v11: SessionSnapshotV11) -> SessionSnapshotV12 {
    SessionSnapshotV12 {
        version: 12,
        clients: v11.clients,
        monitor_orders: v11.monitor_orders,
    }
}

/// v12 -> v13：old_border_w 在反序列化时已缺省；只抬版本号。
fn migrate_snapshot_v12(v12: SessionSnapshotV12) -> SessionSnapshotV13 {
    SessionSnapshotV13 {
        version: 13,
        clients: v12.clients,
        monitor_orders: v12.monitor_orders,
    }
}

/// v13 -> v14：minimized_order 在反序列化时已缺省；只抬版本号。
fn migrate_snapshot_v13(v13: SessionSnapshotV13) -> SessionSnapshotV14 {
    SessionSnapshotV14 {
        version: 14,
        clients: v13.clients,
        monitor_orders: v13.monitor_orders,
    }
}

/// v14 -> v15：hidden_restore 在反序列化时已缺省；只抬版本号。
fn migrate_snapshot_v14(v14: SessionSnapshotV14) -> SessionSnapshotV15 {
    SessionSnapshotV15 {
        version: 15,
        clients: v14.clients,
        monitor_orders: v14.monitor_orders,
    }
}

/// v15 -> v16：old_geometry 在反序列化时已缺省；只抬版本号。
fn migrate_snapshot_v15(v15: SessionSnapshotV15) -> SessionSnapshotV16 {
    SessionSnapshotV16 {
        version: 16,
        clients: v15.clients,
        monitor_orders: v15.monitor_orders,
    }
}

/// v16 -> v17：hidden_x 在反序列化时已缺省；只抬版本号。
fn migrate_snapshot_v16(v16: SessionSnapshotV16) -> SessionSnapshot {
    SessionSnapshot {
        version: SESSION_VERSION,
        clients: v16.clients,
        monitor_orders: v16.monitor_orders,
    }
}

/// 恢复时套用到某个客户端的计划（纯数据，便于单元测试）。
#[derive(Debug, Clone, PartialEq)]
pub struct RestorePlan {
    pub tags: u32,
    pub is_floating: bool,
    pub floating: Option<(i32, i32, i32, i32)>,
}

/// Extra persisted placement used by JWM itself. Keeping this separate from
/// the public `RestorePlan` preserves its existing source-compatible shape.
#[derive(Debug, Clone, PartialEq)]
struct DetailedRestorePlan {
    restore: RestorePlan,
    monitor_num: u32,
    connector: Option<String>,
    maximize: Option<SessionMaximize>,
    is_sticky: bool,
    is_above: bool,
    is_below: bool,
    is_minimized: bool,
    is_fullscreen: bool,
    is_pip: bool,
    client_fact: f32,
    is_drag_floating: bool,
    no_decorations: bool,
    is_urgent: bool,
    demands_attention: bool,
    skip_taskbar: bool,
    skip_pager: bool,
    is_fixed: bool,
    border_w: Option<i32>,
    never_focus: bool,
    old_state: bool,
    pip_restore_sticky: bool,
    remembers_closed_placement: bool,
    old_border_w: Option<i32>,
    minimized_order: Option<u64>,
    hidden_restore: Option<(i32, i32, i32, i32)>,
    old_geometry: Option<(i32, i32, i32, i32)>,
}

impl SessionSnapshot {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    fn validate(&self) -> Result<(), String> {
        if !(MIN_SUPPORTED_SESSION_VERSION..=SESSION_VERSION).contains(&self.version) {
            return Err(format!(
                "unsupported session version {}; supported versions are {}..={}",
                self.version, MIN_SUPPORTED_SESSION_VERSION, SESSION_VERSION
            ));
        }
        if self.clients.len() > MAX_SESSION_CLIENTS {
            return Err(format!(
                "session contains {} clients, exceeding the limit of {MAX_SESSION_CLIENTS}",
                self.clients.len()
            ));
        }
        for (index, entry) in self.clients.iter().enumerate() {
            if entry.class.len() > MAX_SESSION_IDENTITY_FIELD_BYTES
                || entry.instance.len() > MAX_SESSION_IDENTITY_FIELD_BYTES
                || entry.name.len() > MAX_SESSION_IDENTITY_FIELD_BYTES
            {
                return Err(format!(
                    "session client {index} contains oversized text fields"
                ));
            }
            if entry
                .connector
                .as_ref()
                .is_some_and(|connector| connector.len() > MAX_SESSION_IDENTITY_FIELD_BYTES)
            {
                return Err(format!("session client {index} has an oversized connector"));
            }
            if let Some((_, _, width, height)) = entry.floating
                && (width <= 0 || height <= 0)
            {
                return Err(format!(
                    "session client {index} has invalid floating size {width}x{height}"
                ));
            }
            if !entry.is_floating && entry.floating.is_some() {
                return Err(format!(
                    "session client {index} has floating geometry but is not floating"
                ));
            }
            if let Some(maximize) = &entry.maximize {
                if !maximize.vert && !maximize.horz {
                    return Err(format!(
                        "session client {index} has a maximize entry with no axes"
                    ));
                }
                if let Some((_, _, width, height)) = maximize.restore
                    && (width <= 0 || height <= 0)
                {
                    return Err(format!(
                        "session client {index} has invalid maximize restore size \
                         {width}x{height}"
                    ));
                }
            }
            if let Some((_, _, width, height)) = entry.hidden_restore
                && (width <= 0 || height <= 0)
            {
                return Err(format!(
                    "session client {index} has invalid hidden restore size {width}x{height}"
                ));
            }
            if let Some((_, _, width, height)) = entry.old_geometry
                && !((width == 0 && height == 0) || (width > 0 && height > 0))
            {
                return Err(format!(
                    "session client {index} has invalid old geometry size {width}x{height}"
                ));
            }
        }
        if self.monitor_orders.len() > MAX_SESSION_MONITORS {
            return Err(format!(
                "session contains {} monitor order lists, exceeding the limit of \
                 {MAX_SESSION_MONITORS}",
                self.monitor_orders.len()
            ));
        }
        let mut ordered_identities = 0usize;
        for (index, order) in self.monitor_orders.iter().enumerate() {
            ordered_identities = ordered_identities.saturating_add(order.clients.len());
            if ordered_identities > MAX_SESSION_CLIENTS {
                return Err(format!(
                    "session monitor order lists contain more than {MAX_SESSION_CLIENTS} \
                     window identities"
                ));
            }
            if order
                .connector
                .as_ref()
                .is_some_and(|connector| connector.len() > MAX_SESSION_IDENTITY_FIELD_BYTES)
            {
                return Err(format!(
                    "session monitor order {index} has an oversized connector"
                ));
            }
            if order.clients.iter().any(|identity| {
                identity.class.len() > MAX_SESSION_IDENTITY_FIELD_BYTES
                    || identity.instance.len() > MAX_SESSION_IDENTITY_FIELD_BYTES
            }) {
                return Err(format!(
                    "session monitor order {index} contains oversized text fields"
                ));
            }
        }
        Ok(())
    }
}

/// 会话是需要跨重启保留的用户状态，优先写入 XDG state 目录；没有可用的
/// home/state 目录时使用按 uid 隔离的私有临时目录。
pub fn session_file_path() -> PathBuf {
    if let Some(path) = absolute_env_path("XDG_STATE_HOME") {
        return path.join("jwm").join("session.json");
    }
    if let Some(home) = absolute_env_path("HOME") {
        return home
            .join(".local")
            .join("state")
            .join("jwm")
            .join("session.json");
    }
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/tmp/jwm-session-{uid}")).join("session.json")
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn legacy_session_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(cache) = absolute_env_path("XDG_CACHE_HOME") {
        paths.push(cache.join("jwm").join("session.json"));
    }
    if let Some(home) = absolute_env_path("HOME") {
        paths.push(home.join(".cache").join("jwm").join("session.json"));
    }
    // JWM 0.1 used this global fallback when no cache/home directory was
    // configured. The secure loader below accepts it only when it is a regular,
    // current-user-owned file that is not writable by group or other users.
    paths.push(PathBuf::from("/tmp/jwm-session.json"));
    paths
}

fn session_read_path() -> PathBuf {
    let current = session_file_path();
    if path_entry_exists(&current) {
        return current;
    }
    legacy_session_paths()
        .into_iter()
        .find(|path| path_entry_exists(path))
        .unwrap_or(current)
}

/// Treat errors other than `NotFound` as an existing entry so the subsequent
/// secure loader reports them instead of silently falling back to older state.
fn path_entry_exists(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

/// Ensure `path` is a real, current-user-owned directory with mode `0700`,
/// creating it when missing. Shared with other XDG-state writers that live
/// beside `session.json`.
pub(crate) fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "session directory is not a real directory: {}",
                        path.display()
                    ),
                ));
            }
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "session directory is owned by another user: {}",
                        path.display()
                    ),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(error) => return Err(error),
    }

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("session directory is unsafe: {}", path.display()),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Whether a leftover temporary in the state directory belongs to a writer
/// that is gone. The name carries the writer's pid: a living writer's file
/// is mid-rename and left alone, and so is this process's own.
fn orphaned_session_temporary(
    name: &str,
    own_pid: u32,
    process_alive: impl Fn(u32) -> bool,
) -> bool {
    let Some(rest) = name.strip_prefix(SESSION_TEMPORARY_PREFIX) else {
        return false;
    };
    let Some((pid, _sequence)) = rest.split_once('-') else {
        return false;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    pid != own_pid && !process_alive(pid)
}

fn process_alive(pid: u32) -> bool {
    // A pid outside the signal range counts as alive: a malformed name must
    // never turn into `kill(-1, 0)`, which asks about every process.
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Remove temporaries left by writers that died between create and rename.
/// Best effort: a sweep that cannot read the directory or unlink a file has
/// nothing to add to the write that follows.
fn sweep_orphaned_session_temporaries(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let own_pid = std::process::id();
    for entry in entries.flatten().take(MAX_SESSION_SWEEP_ENTRIES) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !orphaned_session_temporary(name, own_pid, process_alive) {
            continue;
        }
        let path = entry.path();
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            let _ = fs::remove_file(&path);
        }
    }
}

fn atomic_write_session(path: &Path, contents: &[u8]) -> io::Result<()> {
    atomic_write_session_with_sync(path, contents, |parent, _temporary| {
        fs::File::open(parent)?.sync_all()
    })
}

fn atomic_write_session_with_sync(
    path: &Path,
    contents: &[u8],
    sync_directory: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if contents.len() as u64 > MAX_SESSION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session snapshot exceeds the 4 MiB limit",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_directory(parent)?;
    sweep_orphaned_session_temporaries(parent);

    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to replace session symlink: {}", path.display()),
        ));
    }

    let (temporary, mut file) = create_session_temporary(parent, std::process::id(), || {
        SESSION_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
    })?;
    let mut renamed = false;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        renamed = true;
        sync_directory(parent, &temporary)?;
        Ok(())
    })();
    // After rename this pathname is free for another writer to own.
    if result.is_err() && !renamed {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_session_temporary(
    parent: &Path,
    pid: u32,
    mut next_sequence: impl FnMut() -> u64,
) -> io::Result<(PathBuf, fs::File)> {
    for _ in 0..MAX_SESSION_TEMPORARY_CREATE_ATTEMPTS {
        let temporary = parent.join(format!(
            "{SESSION_TEMPORARY_PREFIX}{pid}-{}",
            next_sequence()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not create a unique session temporary after {MAX_SESSION_TEMPORARY_CREATE_ATTEMPTS} attempts"
        ),
    ))
}

fn load_session_snapshot(path: &Path) -> Result<SessionSnapshot, Box<dyn std::error::Error>> {
    // Open once without following a final symlink, then validate and read that
    // same inode. Reopening by path after validation would let a concurrent
    // replacement bypass the ownership, mode and size checks below.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    load_open_session_snapshot(file, path)
}

fn load_open_session_snapshot(
    file: fs::File,
    path: &Path,
) -> Result<SessionSnapshot, Box<dyn std::error::Error>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(format!("session path is not a regular file: {}", path.display()).into());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!("session file is owned by another user: {}", path.display()).into());
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(format!(
            "session file is writable by another user or group: {}",
            path.display()
        )
        .into());
    }
    if metadata.len() > MAX_SESSION_BYTES {
        return Err(format!("session file exceeds the 4 MiB limit: {}", path.display()).into());
    }
    // The inode may grow after metadata(); cap the descriptor itself and read
    // one sentinel byte so an exact-limit file remains distinguishable from
    // an oversized one.
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_SESSION_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SESSION_BYTES {
        return Err(format!("session file exceeds the 4 MiB limit: {}", path.display()).into());
    }
    let json = String::from_utf8(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("session file is not valid UTF-8: {error}"),
        )
    })?;
    let snapshot = migrate_session_json(&json)
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    Ok(snapshot)
}

/// The floating rect a floating client is saved with: its resting
/// `floating_*` slot, or the live rect when that slot was never filled.
fn captured_floating_rect(c: &WMClient) -> Option<(i32, i32, i32, i32)> {
    if c.state.is_floating && c.geometry.floating_w > 0 && c.geometry.floating_h > 0 {
        Some((
            c.geometry.floating_x,
            c.geometry.floating_y,
            c.geometry.floating_w,
            c.geometry.floating_h,
        ))
    } else if c.state.is_floating && c.geometry.w > 0 && c.geometry.h > 0 {
        Some((c.geometry.x, c.geometry.y, c.geometry.w, c.geometry.h))
    } else {
        None
    }
}

/// 从窗口状态构建快照，跳过状态栏与 dock。
///
/// A window parked on no tag (a hidden scratchpad) is skipped too; see
/// [`capture_snapshot_excluding`].
pub fn capture_snapshot(state: &WMState, status_bar_name: &str) -> SessionSnapshot {
    capture_snapshot_excluding(state, status_bar_name, &HashSet::new())
}

/// [`capture_snapshot`], also leaving out `excluded` — the scratchpads, when
/// JWM saves its own session — and any window parked off every tag.
///
/// A scratchpad is not a place in the layout: it is summoned and dismissed by
/// its toggle, and a saved entry for it could only be applied to it (revealing
/// a hidden one) or claimed by an ordinary window of the same class, which
/// would take the scratchpad's placement for its own. A window on no tag is a
/// hidden scratchpad too, the one state a restore could only undo.
pub fn capture_snapshot_excluding(
    state: &WMState,
    status_bar_name: &str,
    excluded: &HashSet<ClientKey>,
) -> SessionSnapshot {
    let skipped = |key: ClientKey, c: &WMClient| {
        c.state.is_dock
            || c.is_status_bar(status_bar_name)
            || c.state.tags == 0
            || excluded.contains(&key)
    };
    let mut clients = Vec::new();
    for key in &state.client_order {
        let Some(c) = state.clients.get(*key) else {
            continue;
        };
        if skipped(*key, c) {
            continue;
        }
        // Resting placement first: a maximized client is saved in the state
        // it returns to (tiled if promoted, else floating at the pre-maximize
        // rect). Maximize axes / restore / promoted travel separately in
        // `maximize` so restore can re-apply them after arrange.
        let (is_floating, floating) = if c.state.maximized_axes().any() {
            if c.state.maximize_restore_tiled {
                (false, None)
            } else {
                let restore = c
                    .geometry
                    .maximize_restore_rect
                    .filter(|rect| rect.w > 0 && rect.h > 0)
                    .map(|rect| (rect.x, rect.y, rect.w, rect.h));
                (true, restore.or_else(|| captured_floating_rect(c)))
            }
        } else {
            (c.state.is_floating, captured_floating_rect(c))
        };
        let monitor_num = c
            .mon
            .and_then(|monitor_key| state.monitors.get(monitor_key))
            .and_then(|monitor| u32::try_from(monitor.num).ok())
            .unwrap_or(0);
        clients.push(SessionEntry {
            class: c.class.clone(),
            instance: c.instance.clone(),
            name: c.name.clone(),
            tags: c.state.tags,
            is_floating,
            monitor_num,
            connector: None,
            floating,
            maximize: SessionMaximize::from_client(c),
            is_sticky: c.state.is_sticky,
            is_above: c.state.is_above,
            is_below: c.state.is_below,
            is_minimized: c.state.is_hidden,
            is_fullscreen: c.state.is_fullscreen,
            is_pip: c.state.is_pip,
            client_fact: c.state.client_fact,
            is_drag_floating: is_floating && c.state.is_drag_floating,
            no_decorations: c.state.no_decorations,
            is_urgent: c.state.is_urgent,
            demands_attention: c.state.demands_attention,
            skip_taskbar: c.state.skip_taskbar,
            skip_pager: c.state.skip_pager,
            is_fixed: c.state.is_fixed,
            border_w: Some(c.geometry.border_w),
            never_focus: c.state.never_focus,
            old_state: c.state.old_state,
            pip_restore_sticky: c.state.pip_restore_sticky,
            remembers_closed_placement: c.state.remembers_closed_placement,
            old_border_w: Some(c.geometry.old_border_w),
            minimized_order: (c.state.is_hidden && c.state.minimized_order > 0)
                .then_some(c.state.minimized_order),
            hidden_restore: c
                .geometry
                .hidden_restore_rect
                .filter(|rect| rect.w > 0 && rect.h > 0)
                .map(|r| (r.x, r.y, r.w, r.h)),
            old_geometry: (c.geometry.old_w > 0 && c.geometry.old_h > 0).then_some((
                c.geometry.old_x,
                c.geometry.old_y,
                c.geometry.old_w,
                c.geometry.old_h,
            )),
            hidden_x: c.geometry.hidden_x,
        });
    }
    let monitor_orders = state
        .monitor_order
        .iter()
        .filter_map(|&monitor_key| {
            let monitor = state.monitors.get(monitor_key)?;
            let monitor_num = u32::try_from(monitor.num).ok()?;
            // A window maximize pulled out of the tiles is saved tiled, so it
            // is saved in its tile slot, not at the floating tail.
            let clients = state
                .monitor_clients
                .get(monitor_key)
                .map(|keys| {
                    resting_order(state, keys)
                        .into_iter()
                        .filter_map(|client_key| {
                            let c = state.clients.get(client_key)?;
                            if skipped(client_key, c) {
                                return None;
                            }
                            Some(SessionWindowIdentity {
                                class: c.class.clone(),
                                instance: c.instance.clone(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(SessionMonitorOrder {
                monitor_num,
                connector: None,
                clients,
            })
        })
        .collect();
    SessionSnapshot {
        version: SESSION_VERSION,
        clients,
        monitor_orders,
    }
}

/// 把快照匹配到当前客户端，生成恢复计划。
///
/// 匹配规则：class 必须忽略大小写相等；若双方都有 instance，则 instance 也需
/// 相等。每个保存条目最多匹配一个客户端（已用过的条目不再匹配），从而让同一
/// 应用的多个实例尽量映射到不同的保存条目。具有精确 instance 匹配的条目优先。
///
/// An entry saved on no tag (a hidden scratchpad in a snapshot an older JWM
/// wrote) never matches: applied to anything, it could only move that window
/// onto the current view.
pub fn plan_restore<'a, I>(snapshot: &SessionSnapshot, clients: I) -> Vec<(ClientKey, RestorePlan)>
where
    I: IntoIterator<Item = (ClientKey, &'a str, &'a str)>,
{
    plan_restore_detailed(snapshot, clients)
        .into_iter()
        .map(|(key, plan)| (key, plan.restore))
        .collect()
}

fn plan_restore_detailed<'a, I>(
    snapshot: &SessionSnapshot,
    clients: I,
) -> Vec<(ClientKey, DetailedRestorePlan)>
where
    I: IntoIterator<Item = (ClientKey, &'a str, &'a str)>,
{
    let mut used = vec![false; snapshot.clients.len()];
    let mut out = Vec::new();

    for (key, class, instance) in clients {
        let mut fallback: Option<usize> = None;
        let mut exact: Option<usize> = None;

        for (i, e) in snapshot.clients.iter().enumerate() {
            // An entry on no tag is a hidden scratchpad an older JWM saved.
            // Applied to anything it could only move that window onto the
            // current view, so it is never a candidate.
            if used[i] || e.tags == 0 || !e.class.eq_ignore_ascii_case(class) {
                continue;
            }
            let both_have_instance = !e.instance.is_empty() && !instance.is_empty();
            if both_have_instance {
                if e.instance.eq_ignore_ascii_case(instance) {
                    exact = Some(i);
                    break;
                }
                // class matches but instance differs — not a candidate.
                continue;
            }
            if fallback.is_none() {
                fallback = Some(i);
            }
        }

        if let Some(i) = exact.or(fallback) {
            used[i] = true;
            let e = &snapshot.clients[i];
            out.push((
                key,
                DetailedRestorePlan {
                    restore: RestorePlan {
                        tags: e.tags,
                        is_floating: e.is_floating,
                        floating: e.floating,
                    },
                    monitor_num: e.monitor_num,
                    connector: e.connector.clone(),
                    maximize: e.maximize.clone(),
                    is_sticky: e.is_sticky,
                    is_above: e.is_above,
                    is_below: e.is_below,
                    is_minimized: e.is_minimized,
                    is_fullscreen: e.is_fullscreen,
                    is_pip: e.is_pip,
                    client_fact: e.client_fact,
                    is_drag_floating: e.is_drag_floating,
                    no_decorations: e.no_decorations,
                    is_urgent: e.is_urgent,
                    demands_attention: e.demands_attention,
                    skip_taskbar: e.skip_taskbar,
                    skip_pager: e.skip_pager,
                    is_fixed: e.is_fixed,
                    border_w: e.border_w,
                    never_focus: e.never_focus,
                    old_state: e.old_state,
                    pip_restore_sticky: e.pip_restore_sticky,
                    remembers_closed_placement: e.remembers_closed_placement,
                    old_border_w: e.old_border_w,
                    minimized_order: e.minimized_order,
                    hidden_restore: e.hidden_restore,
                    old_geometry: e
                        .old_geometry
                        .filter(|&(_, _, width, height)| width > 0 && height > 0),
                },
            ));
        }
    }

    out
}

/// 按保存顺序重排当前客户端（纯函数，便于单元测试）。
///
/// 返回包含 `current` 全部 key 的新顺序：
/// - 保存列表中仍存在的身份按保存序排在前；匹配是出现次数感知的——保存
///   列表里第 k 个相同身份消费当前列表中第 k 个尚未使用的匹配，同一应用
///   的多个实例因此保持各自的相对位置；
/// - 保存列表之外的新窗口保持当前相对顺序追加在后；
/// - 保存列表中已消失的身份被忽略；空保存列表原样返回当前顺序。
pub fn plan_order_restore<K: Clone>(
    saved: &[SessionWindowIdentity],
    current: &[(K, SessionWindowIdentity)],
) -> Vec<K> {
    let mut used = vec![false; current.len()];
    let mut ordered = Vec::with_capacity(current.len());
    for saved_identity in saved {
        if let Some(index) = current
            .iter()
            .enumerate()
            .find_map(|(index, (_, identity))| {
                (!used[index] && identity.matches(saved_identity)).then_some(index)
            })
        {
            used[index] = true;
            ordered.push(current[index].0.clone());
        }
    }
    for (index, (key, _)) in current.iter().enumerate() {
        if !used[index] {
            ordered.push(key.clone());
        }
    }
    ordered
}

impl Jwm {
    /// 保存当前会话（窗口标签 / 浮动布局）到磁盘。
    pub fn save_session(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let status_bar_name = CONFIG.load().status_bar_name().to_string();
        let scratchpads: HashSet<ClientKey> = self.scratchpads.values().copied().collect();
        let mut snapshot = capture_snapshot_excluding(&self.state, &status_bar_name, &scratchpads);
        self.attach_session_connectors(backend, &mut snapshot);
        snapshot
            .validate()
            .map_err(|error| format!("cannot save invalid session: {error}"))?;
        let json = snapshot.to_json()?;

        let path = session_file_path();
        atomic_write_session(&path, json.as_bytes())?;
        log::info!(
            "session saved: {} clients -> {}",
            snapshot.clients.len(),
            path.display()
        );
        Ok(())
    }

    /// Fill `connector` on every entry / monitor order from the live output
    /// map so a later restore survives hole-fill renumbering.
    fn attach_session_connectors(&self, backend: &dyn Backend, snapshot: &mut SessionSnapshot) {
        let mut by_num: HashMap<u32, String> = HashMap::new();
        for &mon_key in &self.state.monitor_order {
            let Some(monitor) = self.state.monitors.get(mon_key) else {
                continue;
            };
            let Ok(num) = u32::try_from(monitor.num) else {
                continue;
            };
            if let Some(key) = self.output_key_for_monitor(backend, mon_key) {
                by_num.insert(num, key);
            }
        }
        for entry in &mut snapshot.clients {
            entry.connector = by_num.get(&entry.monitor_num).cloned();
        }
        for order in &mut snapshot.monitor_orders {
            order.connector = by_num.get(&order.monitor_num).cloned();
        }
    }

    /// Resolve a saved connector / `stable_key` to the monitor number that
    /// currently owns that output; fall back to the saved `monitor_num`.
    fn resolve_session_monitor_num(
        &self,
        backend: &dyn Backend,
        remembered_monitor_num: u32,
        remembered_connector: Option<&str>,
    ) -> u32 {
        let live = self.live_monitor_identities(backend);
        let live_refs: Vec<(i32, &str, &str)> = live
            .iter()
            .map(|(num, connector, stable_key)| (*num, connector.as_str(), stable_key.as_str()))
            .collect();
        let remembered_i32 = i32::try_from(remembered_monitor_num).unwrap_or(0);
        let resolved =
            resolve_monitor_num_by_connector(remembered_i32, remembered_connector, &live_refs);
        u32::try_from(resolved).unwrap_or(remembered_monitor_num)
    }

    /// 从磁盘恢复会话：把保存的标签 / 浮动状态套用到当前匹配的客户端。
    pub fn restore_session(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path = session_read_path();
        let snapshot = match load_session_snapshot(&path) {
            Ok(snapshot) => snapshot,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                log::info!("session restore skipped: no snapshot at {}", path.display());
                return Ok(());
            }
            Err(error) => {
                return Err(format!("cannot restore session {}: {error}", path.display()).into());
            }
        };
        let matched = self.apply_session_snapshot(backend, &snapshot);
        log::info!("session restored: {matched} clients matched");
        self.broadcast_monitor_bar_all_monitors(backend);
        Ok(())
    }

    /// Apply a loaded snapshot to the current clients and return how many
    /// matched.
    fn apply_session_snapshot(
        &mut self,
        backend: &mut dyn Backend,
        snapshot: &SessionSnapshot,
    ) -> usize {
        // Build (key, class, instance) view of current clients. Scratchpads
        // are left out: their visibility belongs to their toggle, and a hidden
        // one matched to any entry would be retagged onto a shown tag.
        let scratchpads: HashSet<ClientKey> = self.scratchpads.values().copied().collect();
        let current: Vec<(ClientKey, String, String)> = self
            .state
            .client_order
            .iter()
            .filter(|k| !scratchpads.contains(k))
            .filter_map(|k| {
                self.state
                    .clients
                    .get(*k)
                    .map(|c| (*k, c.class.clone(), c.instance.clone()))
            })
            .collect();

        let plans = plan_restore_detailed(
            snapshot,
            current.iter().map(|(k, c, i)| (*k, c.as_str(), i.as_str())),
        );

        for (key, plan) in &plans {
            let monitor_num = self.resolve_session_monitor_num(
                backend,
                plan.monitor_num,
                plan.connector.as_deref(),
            );
            let target_monitor = self
                .state
                .monitor_order
                .iter()
                .copied()
                .find(|monitor_key| {
                    self.state
                        .monitors
                        .get(*monitor_key)
                        .is_some_and(|monitor| u32::try_from(monitor.num) == Ok(monitor_num))
                });
            if let Some(target_monitor) = target_monitor
                && self.state.clients.get(*key).and_then(|client| client.mon)
                    != Some(target_monitor)
            {
                self.sendmon(backend, Some(*key), Some(target_monitor));
            }
        }

        // Leave fullscreen / PiP before resting placement so tag / float /
        // maximize can own the slot; modes are re-applied below after
        // stacking from the saved flags.
        for (key, _) in &plans {
            if self
                .state
                .clients
                .get(*key)
                .is_some_and(|client| client.state.is_fullscreen)
                && let Err(error) = self.setfullscreen(backend, *key, false)
            {
                log::warn!(
                    "session restore could not leave fullscreen for a matched client: {error}"
                );
            }
            if self
                .state
                .clients
                .get(*key)
                .is_some_and(|client| client.state.is_pip)
                && let Err(error) = self.set_client_pip(backend, *key, false)
            {
                log::warn!("session restore could not leave PiP for a matched client: {error}");
            }
        }

        // Leave maximize through the shared transaction first so resting
        // tags / float / geometry own the slot; axes are re-applied below
        // after arrange from the saved `maximize` entry.
        for (key, _) in &plans {
            if self
                .state
                .clients
                .get(*key)
                .is_some_and(|client| client.state.maximized_axes().any())
                && let Err(error) = self.set_client_maximized(
                    backend,
                    *key,
                    MaximizeAxes::NONE,
                    MaximizeOrigin::User,
                )
            {
                log::warn!("session restore could not unmaximize a matched client: {error}");
            }
        }

        let tagmask = CONFIG.load().tagmask();
        let mut floats: Vec<(ClientKey, (i32, i32, i32, i32))> = Vec::new();
        for (key, plan) in &plans {
            let restore = &plan.restore;
            let monitor_key = self.state.clients.get(*key).and_then(|client| client.mon);
            let fallback_tags = monitor_key
                .and_then(|key| self.state.monitors.get(key))
                .map(|monitor| monitor.get_active_tags() & tagmask)
                .filter(|tags| *tags != 0)
                .unwrap_or(1);
            let restored_tags = sanitize_tags(restore.tags, tagmask, fallback_tags);
            let floating = restore.floating.and_then(|floating| {
                monitor_key
                    .and_then(|key| self.monitor_work_area(key))
                    .map(|area| clamp_floating_rect(floating, area))
            });
            if let Some(c) = self.state.clients.get_mut(*key) {
                c.state.tags = restored_tags;
                c.state.is_floating = restore.is_floating;
                c.state.is_drag_floating = restore.is_floating && plan.is_drag_floating;
                c.state.client_fact = sanitize_client_fact(plan.client_fact);
                c.state.no_decorations = plan.no_decorations;
                c.state.is_urgent = plan.is_urgent;
                c.state.demands_attention = plan.demands_attention;
                c.state.skip_taskbar = plan.skip_taskbar;
                c.state.skip_pager = plan.skip_pager;
                c.state.is_fixed = plan.is_fixed;
                c.state.never_focus = plan.never_focus;
                c.state.old_state = plan.old_state;
                c.state.pip_restore_sticky = plan.pip_restore_sticky;
                c.state.remembers_closed_placement = plan.remembers_closed_placement;
                if plan.no_decorations {
                    c.geometry.border_w = 0;
                } else if let Some(border_w) = plan.border_w {
                    c.geometry.border_w = border_w.max(0);
                }
                if let Some(old_border_w) = plan.old_border_w {
                    c.geometry.old_border_w = old_border_w.max(0);
                }
                if let Some((x, y, w, h)) = floating {
                    c.geometry.floating_x = x;
                    c.geometry.floating_y = y;
                    c.geometry.floating_w = w;
                    c.geometry.floating_h = h;
                    floats.push((*key, (x, y, w, h)));
                }
            }
            self.reorder_client_in_monitor_groups(*key);
            if let Err(error) = self.setclienttagprop(backend, *key) {
                log::warn!("session restore could not update client metadata: {error}");
            }
        }

        for (key, (x, y, w, h)) in floats {
            self.resize_client(backend, key, x, y, w, h, false);
        }

        // 全部窗口状态（tag / 浮动 / 所在显示器）恢复完成后，最后按快照
        // 重排每个显示器的平铺顺序，再统一 arrange。v1/v2 快照的
        // monitor_orders 为空，这里自然成为无操作。
        self.restore_monitor_client_order(backend, snapshot);

        let monitor_keys: Vec<_> = self.state.monitor_order.clone();
        for mk in monitor_keys {
            self.arrange(backend, Some(mk));
        }

        // Re-apply persisted maximize after resting placement and arrange so
        // User admission promotes into the restored tile order, and floating
        // maximize keeps the saved restore hint. Clamp absolute restore coords
        // into the destination work area so a connector remap cannot leave
        // the hint off-screen.
        for (key, plan) in &plans {
            let Some(maximize) = &plan.maximize else {
                continue;
            };
            let axes = maximize.axes();
            if !axes.any() {
                continue;
            }
            let restore_hint = maximize.restore_hint().map(|hint| {
                let area = self
                    .state
                    .clients
                    .get(*key)
                    .and_then(|client| client.mon)
                    .and_then(|monitor_key| self.monitor_work_area(monitor_key));
                match area {
                    Some(area) => {
                        let (x, y, w, h) =
                            clamp_floating_rect((hint.x, hint.y, hint.w, hint.h), area);
                        Rect::new(x, y, w, h)
                    }
                    None => hint,
                }
            });
            if let Err(error) =
                self.adopt_client_maximized(backend, *key, axes, restore_hint, maximize.promoted)
            {
                log::warn!("session restore could not re-maximize a matched client: {error}");
            }
        }

        // Sticky after placement: set_client_sticky adopts the monitor's
        // current tags when turning sticky on, and mirrors the EWMH atom for
        // pagers.
        for (key, plan) in &plans {
            self.set_client_sticky(backend, *key, plan.is_sticky);
        }

        // Above / Below: share the EWMH stacking path so restack and
        // property writes stay consistent with client requests. Above wins
        // when a snapshot somehow recorded both.
        for (key, plan) in &plans {
            let want_above = plan.is_above;
            let want_below = plan.is_below && !plan.is_above;
            let (have_above, have_below) = self
                .state
                .clients
                .get(*key)
                .map(|client| (client.state.is_above, client.state.is_below))
                .unwrap_or((false, false));
            if want_above == have_above && want_below == have_below {
                continue;
            }
            let (action, flag) = if want_above {
                (NetWmAction::Add, NetWmState::Above)
            } else if want_below {
                (NetWmAction::Add, NetWmState::Below)
            } else if have_above {
                (NetWmAction::Remove, NetWmState::Above)
            } else {
                (NetWmAction::Remove, NetWmState::Below)
            };
            if let Err(error) = apply_external_stacking_request(self, backend, *key, action, flag) {
                log::warn!(
                    "session restore could not re-apply Above/Below for a matched client: {error}"
                );
            }
        }

        // Fullscreen / PiP after stacking and before minimized: both modes
        // own floating geometry the same way maximize does under them, and
        // minimize parks last so Dock sees the resting snapshot. Fullscreen
        // wins when a snapshot somehow recorded both (runtime mutual exclusion).
        for (key, plan) in &plans {
            if plan.is_fullscreen {
                if let Err(error) = self.setfullscreen(backend, *key, true) {
                    log::warn!(
                        "session restore could not re-apply fullscreen for a matched client: {error}"
                    );
                }
            } else if plan.is_pip {
                if let Err(error) = self.set_client_pip(backend, *key, true) {
                    log::warn!(
                        "session restore could not re-apply PiP for a matched client: {error}"
                    );
                }
            }
        }

        // Minimized last: park after placement / maximize / stacking / FS·PiP
        // so the Dock sees the resting state the snapshot intended.
        for (key, plan) in &plans {
            if let Err(error) = self.set_client_minimized(backend, *key, plan.is_minimized) {
                log::warn!(
                    "session restore could not re-apply minimized for a matched client: {error}"
                );
            }
            if plan.is_minimized {
                // Fullscreen already staged the current output rectangle.
                // Its visible target includes the bar/strut area; clamping
                // the old parking snapshot to the work area here shifts a
                // later fullscreen restore down below the bar.
                let hidden_restore = plan
                    .hidden_restore
                    .filter(|_| {
                        self.state
                            .clients
                            .get(*key)
                            .is_some_and(|client| !client.state.is_fullscreen)
                    })
                    .map(|restore| {
                        let area = self
                            .state
                            .clients
                            .get(*key)
                            .and_then(|client| client.mon)
                            .and_then(|monitor_key| self.monitor_work_area(monitor_key));
                        area.map_or(restore, |area| clamp_floating_rect(restore, area))
                    });
                if let Some(order) = plan.minimized_order.filter(|&o| o > 0) {
                    if let Some(c) = self.state.clients.get_mut(*key) {
                        c.state.minimized_order = order;
                    }
                    let _ = super::window_state::observe_minimized_order(order);
                }
                if let Some((x, y, w, h)) = hidden_restore {
                    if let Some(c) = self.state.clients.get_mut(*key) {
                        c.geometry.hidden_restore_rect =
                            Some(crate::core::types::Rect::new(x, y, w, h));
                    }
                }
            }
        }

        // Chrome flags that `updatesizehints` / arrange may have recomputed:
        // re-assert the snapshot's urgency / attention / skip_* / fixed /
        // border after the resting geometry pass.
        for (key, plan) in &plans {
            let old_geometry = plan.old_geometry.map(|restore| {
                let area = self
                    .state
                    .clients
                    .get(*key)
                    .and_then(|client| client.mon)
                    .and_then(|monitor_key| self.monitor_work_area(monitor_key));
                area.map_or(restore, |area| clamp_floating_rect(restore, area))
            });
            if let Some(c) = self.state.clients.get_mut(*key) {
                c.state.is_urgent = plan.is_urgent;
                c.state.demands_attention = plan.demands_attention;
                c.state.skip_taskbar = plan.skip_taskbar;
                c.state.skip_pager = plan.skip_pager;
                c.state.is_fixed = plan.is_fixed;
                c.state.never_focus = plan.never_focus;
                c.state.old_state = plan.old_state;
                c.state.pip_restore_sticky = plan.pip_restore_sticky;
                c.state.remembers_closed_placement = plan.remembers_closed_placement;
                if plan.no_decorations {
                    c.geometry.border_w = 0;
                } else if let Some(border_w) = plan.border_w {
                    c.geometry.border_w = border_w.max(0);
                }
                if let Some(old_border_w) = plan.old_border_w {
                    c.geometry.old_border_w = old_border_w.max(0);
                }
                if let Some((x, y, w, h)) = old_geometry {
                    c.geometry.old_x = x;
                    c.geometry.old_y = y;
                    c.geometry.old_w = w;
                    c.geometry.old_h = h;
                }
            }
        }

        plans.len()
    }

    /// 按快照中保存的顺序重排每个显示器的 `monitor_clients`。
    ///
    /// 保存列表中的窗口按保存序在前（保持相对序），列表外的新窗口按现有
    /// 相对顺序追加；`monitor_clients` 的「平铺组在前、浮动组在后」不变量
    /// 优先于保存顺序——保存后浮动状态发生变化的窗口回到它当前所属的组。
    fn restore_monitor_client_order(&mut self, backend: &dyn Backend, snapshot: &SessionSnapshot) {
        let scratchpads: HashSet<ClientKey> = self.scratchpads.values().copied().collect();
        for saved in &snapshot.monitor_orders {
            let monitor_num = self.resolve_session_monitor_num(
                backend,
                saved.monitor_num,
                saved.connector.as_deref(),
            );
            let Some(monitor_key) = self
                .state
                .monitor_order
                .iter()
                .copied()
                .find(|monitor_key| {
                    self.state
                        .monitors
                        .get(*monitor_key)
                        .is_some_and(|monitor| u32::try_from(monitor.num) == Ok(monitor_num))
                })
            else {
                continue;
            };
            let Some(current_keys) = self.state.monitor_clients.get(monitor_key) else {
                continue;
            };
            let current: Vec<(ClientKey, SessionWindowIdentity)> = current_keys
                .iter()
                .filter_map(|&key| {
                    self.state.clients.get(key).map(|client| {
                        (
                            key,
                            SessionWindowIdentity {
                                class: client.class.clone(),
                                instance: client.instance.clone(),
                            },
                        )
                    })
                })
                .collect();
            if current.len() != current_keys.len() {
                // 列表与客户端表不一致时不重排，避免覆盖丢失 key。
                continue;
            }
            // Scratchpads are never saved, so they claim no saved slot: one
            // matching an ordinary window's identity would push that window
            // out of its place. They keep their relative order after the rest.
            let (held_back, current): (Vec<_>, Vec<_>) = current
                .into_iter()
                .partition(|(key, _)| scratchpads.contains(key));
            let mut planned = plan_order_restore(&saved.clients, &current);
            planned.extend(held_back.into_iter().map(|(key, _)| key));
            let mut tiled: Vec<ClientKey> = Vec::with_capacity(planned.len());
            let mut floating: Vec<ClientKey> = Vec::new();
            for key in planned {
                let is_floating = self
                    .state
                    .clients
                    .get(key)
                    .is_some_and(|client| client.state.is_floating);
                if is_floating {
                    floating.push(key);
                } else {
                    tiled.push(key);
                }
            }
            tiled.extend(floating);
            if let Some(client_list) = self.state.monitor_clients.get_mut(monitor_key) {
                *client_list = tiled;
            }
        }
    }
}

fn clamp_floating_rect(
    (mut x, mut y, width, height): (i32, i32, i32, i32),
    boundary: Rect,
) -> (i32, i32, i32, i32) {
    let width = width.clamp(1, boundary.w.max(1));
    let height = height.clamp(1, boundary.h.max(1));
    GeometryConstraints::clamp_rect_to_boundary(&mut x, &mut y, width, height, &boundary);
    (x, y, width, height)
}

fn sanitize_tags(saved: u32, tagmask: u32, fallback: u32) -> u32 {
    let saved = saved & tagmask;
    if saved != 0 {
        return saved;
    }
    let fallback = fallback & tagmask;
    if fallback != 0 { fallback } else { tagmask & 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::common_define::WindowId;
    use crate::core::models::{WMClient, WMMonitor};
    use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};
    use slotmap::SlotMap;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let sequence = SESSION_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jwm-session-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(class: &str, instance: &str, tags: u32) -> SessionEntry {
        SessionEntry {
            class: class.to_string(),
            instance: instance.to_string(),
            name: String::new(),
            tags,
            is_floating: false,
            monitor_num: 0,
            connector: None,
            floating: None,
            maximize: None,
            is_sticky: false,
            is_above: false,
            is_below: false,
            is_minimized: false,
            is_fullscreen: false,
            is_pip: false,
            client_fact: 1.0,
            is_drag_floating: false,
            no_decorations: false,
            is_urgent: false,
            demands_attention: false,
            skip_taskbar: false,
            skip_pager: false,
            is_fixed: false,
            border_w: None,
            never_focus: false,
            old_state: false,
            pip_restore_sticky: false,
            remembers_closed_placement: false,
            old_border_w: None,
            minimized_order: None,
            hidden_restore: None,
            old_geometry: None,
            hidden_x: None,
        }
    }

    fn identity(class: &str, instance: &str) -> SessionWindowIdentity {
        SessionWindowIdentity {
            class: class.to_string(),
            instance: instance.to_string(),
        }
    }

    fn snapshot_with_clients(clients: Vec<SessionEntry>) -> SessionSnapshot {
        SessionSnapshot {
            version: SESSION_VERSION,
            clients,
            monitor_orders: Vec::new(),
        }
    }

    fn keys(n: usize) -> Vec<ClientKey> {
        let mut sm: SlotMap<ClientKey, ()> = SlotMap::new();
        (0..n).map(|_| sm.insert(())).collect()
    }

    #[test]
    fn rename_failure_removes_unpublished_temporary() {
        let dir = TestDir::new("rename-failure");
        let destination = dir.0.join("existing-directory");
        fs::create_dir(&destination).unwrap();
        assert!(
            atomic_write_session_with_sync(&destination, b"snapshot", |_, _| {
                panic!("directory sync must follow a successful rename")
            })
            .is_err()
        );
        assert!(destination.is_dir());
        assert!(fs::read_dir(&dir.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(SESSION_TEMPORARY_PREFIX)
        }));
    }

    #[test]
    fn directory_sync_failure_preserves_reused_temporary_path() {
        let dir = TestDir::new("post-rename-sync");
        let path = dir.0.join("snapshot.json");
        let mut replacement = None;
        let error = atomic_write_session_with_sync(&path, b"published", |_parent, temporary| {
            fs::write(temporary, b"new owner")?;
            replacement = Some(temporary.to_path_buf());
            Err(io::Error::other("directory sync failed"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"published");
        assert_eq!(fs::read(replacement.unwrap()).unwrap(), b"new owner");
    }

    #[test]
    fn json_round_trip_is_lossless() {
        let snap = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![
                SessionEntry {
                    class: "Firefox".into(),
                    instance: "Navigator".into(),
                    name: "title".into(),
                    tags: 0b101,
                    is_floating: true,
                    monitor_num: 1,
                    connector: Some("HDMI-A-1".into()),
                    floating: Some((10, 20, 800, 600)),
                    maximize: Some(SessionMaximize {
                        vert: true,
                        horz: true,
                        restore: Some((40, 50, 700, 500)),
                        promoted: false,
                    }),
                    is_sticky: true,
                    is_above: true,
                    is_below: false,
                    is_minimized: true,
                    is_fullscreen: true,
                    is_pip: false,
                    client_fact: 1.0,
                    is_drag_floating: false,
                    no_decorations: false,
                    is_urgent: true,
                    demands_attention: true,
                    skip_taskbar: true,
                    skip_pager: false,
                    is_fixed: false,
                    border_w: Some(3),
                    never_focus: false,
                    old_state: false,
                    pip_restore_sticky: false,
                    remembers_closed_placement: false,
                    old_border_w: None,
                    minimized_order: None,
                    hidden_restore: None,
                    old_geometry: None,
                    hidden_x: None,
                },
                entry("Alacritty", "alacritty", 0b1),
            ],
            monitor_orders: vec![
                SessionMonitorOrder {
                    monitor_num: 0,
                    connector: Some("eDP-1".into()),
                    clients: vec![identity("Alacritty", "alacritty")],
                },
                SessionMonitorOrder {
                    monitor_num: 1,
                    connector: Some("HDMI-A-1".into()),
                    clients: vec![identity("Firefox", "Navigator")],
                },
            ],
        };
        let json = snap.to_json().unwrap();
        assert!(
            json.contains("\"connector\": \"HDMI-A-1\""),
            "connector must be persisted: {json}"
        );
        let back = SessionSnapshot::from_json(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn validation_accepts_v1_and_rejects_unknown_or_invalid_snapshots() {
        let mut snapshot = SessionSnapshot {
            version: 1,
            clients: vec![entry("Term", "kitty", 1)],
            monitor_orders: Vec::new(),
        };
        assert!(snapshot.validate().is_ok());

        snapshot.version = SESSION_VERSION + 1;
        assert!(snapshot.validate().unwrap_err().contains("unsupported"));

        snapshot.version = SESSION_VERSION;
        snapshot.clients[0].floating = Some((0, 0, -1, 100));
        assert!(snapshot.validate().unwrap_err().contains("floating size"));

        snapshot.clients[0].floating = None;
        snapshot.clients[0].hidden_restore = Some((10, 20, 0, 100));
        assert!(
            snapshot
                .validate()
                .unwrap_err()
                .contains("hidden restore size")
        );

        snapshot.clients[0].hidden_restore = None;
        snapshot.clients[0].old_geometry = Some((10, 20, 100, -1));
        assert!(
            snapshot
                .validate()
                .unwrap_err()
                .contains("old geometry size")
        );

        snapshot.clients[0].old_geometry = Some((0, 0, 0, 0));
        assert!(snapshot.validate().is_ok());
    }

    #[test]
    fn capture_and_legacy_zero_old_geometry_round_trip_without_overwriting_live_slots() {
        let mut state = WMState::new();
        let mut tiled = WMClient::new(WindowId::from_raw(0x410));
        tiled.class = "Tiled".into();
        tiled.instance = "tiled".into();
        tiled.state.tags = 1;
        let tiled_key = state.clients.insert(tiled);
        state.client_order.push(tiled_key);

        let mut floating = WMClient::new(WindowId::from_raw(0x411));
        floating.class = "Floating".into();
        floating.instance = "floating".into();
        floating.state.tags = 1;
        floating.state.is_floating = true;
        floating.geometry.floating_x = 40;
        floating.geometry.floating_y = 50;
        floating.geometry.floating_w = 600;
        floating.geometry.floating_h = 400;
        let floating_key = state.clients.insert(floating);
        state.client_order.push(floating_key);

        let captured = capture_snapshot(&state, "status-bar");
        assert!(
            captured
                .clients
                .iter()
                .all(|entry| entry.old_geometry.is_none())
        );
        let loaded = migrate_session_json(&captured.to_json().unwrap()).unwrap();
        assert!(
            loaded
                .clients
                .iter()
                .all(|entry| entry.old_geometry.is_none())
        );

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let mut live = WMClient::new(WindowId::from_raw(0x412));
        live.class = "Legacy".into();
        live.instance = "legacy".into();
        live.state.tags = 1;
        live.mon = Some(monitor);
        live.geometry.old_x = 31;
        live.geometry.old_y = 41;
        live.geometry.old_w = 701;
        live.geometry.old_h = 501;
        live.geometry.x = 31;
        live.geometry.y = 41;
        live.geometry.w = 701;
        live.geometry.h = 501;
        let live_key = jwm.insert_client(live);
        jwm.attach_to_monitor(live_key, monitor);

        let mut legacy_entry = entry("Legacy", "legacy", 1);
        legacy_entry.old_geometry = Some((0, 0, 0, 0));
        let legacy_json = snapshot_with_clients(vec![legacy_entry]).to_json().unwrap();
        let legacy = migrate_session_json(&legacy_json).expect("legacy zero slot loads");
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &legacy), 1);
        let geometry = &jwm.state.clients[live_key].geometry;
        assert_eq!(
            (
                geometry.old_x,
                geometry.old_y,
                geometry.old_w,
                geometry.old_h
            ),
            (31, 41, 701, 501)
        );
    }

    #[test]
    fn validation_bounds_monitor_order_lists() {
        let mut snapshot = snapshot_with_clients(Vec::new());
        snapshot.monitor_orders = vec![
            SessionMonitorOrder {
                monitor_num: 0,
                connector: None,
                clients: Vec::new(),
            };
            MAX_SESSION_MONITORS + 1
        ];
        assert!(
            snapshot
                .validate()
                .unwrap_err()
                .contains("monitor order lists")
        );

        let mut snapshot = snapshot_with_clients(Vec::new());
        snapshot.monitor_orders = vec![SessionMonitorOrder {
            monitor_num: 0,
            connector: None,
            clients: vec![SessionWindowIdentity {
                class: "x".repeat(65_537),
                instance: String::new(),
            }],
        }];
        assert!(
            snapshot
                .validate()
                .unwrap_err()
                .contains("oversized text fields")
        );

        let mut snapshot = snapshot_with_clients(Vec::new());
        snapshot.monitor_orders = vec![SessionMonitorOrder {
            monitor_num: 0,
            connector: None,
            clients: vec![identity("Term", "kitty"); MAX_SESSION_CLIENTS + 1],
        }];
        assert!(
            snapshot
                .validate()
                .unwrap_err()
                .contains("window identities")
        );
    }

    #[test]
    fn atomic_store_is_private_roundtrips_and_rejects_symlinks() {
        let root = TestDir::new("atomic");
        let path = root.0.join("state").join("session.json");
        atomic_write_session(&path, br#"{"version":2,"clients":[]}"#).unwrap();

        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            load_session_snapshot(&path).unwrap().version,
            SESSION_VERSION
        );

        let victim = root.0.join("victim");
        fs::write(&victim, "unchanged").unwrap();
        let link = root.0.join("state").join("linked.json");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(atomic_write_session(&link, b"replacement").is_err());
        assert_eq!(fs::read_to_string(victim).unwrap(), "unchanged");
    }

    #[test]
    fn session_temporary_collision_keeps_existing_file_and_uses_next_name() {
        let root = TestDir::new("temporary-collision");
        let collision = root.0.join(format!("{SESSION_TEMPORARY_PREFIX}4242-7"));
        fs::write(&collision, "other writer").unwrap();
        let mut sequences = [7, 8].into_iter();

        let (temporary, file) = create_session_temporary(&root.0, 4242, || {
            sequences.next().expect("a fresh sequence")
        })
        .unwrap();
        drop(file);

        assert_eq!(fs::read_to_string(&collision).unwrap(), "other writer");
        assert_eq!(
            temporary.file_name().and_then(|name| name.to_str()),
            Some(".session.json.tmp-4242-8")
        );

        let exhausted = create_session_temporary(&root.0, 4242, || 7).unwrap_err();
        assert_eq!(exhausted.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(collision).unwrap(), "other writer");
    }

    #[test]
    fn a_successful_write_sweeps_temporaries_left_by_dead_writers() {
        // pid_max never reaches i32::MAX, so this writer is certainly gone.
        let dead_pid = i32::MAX as u32;
        assert!(!process_alive(dead_pid));
        assert!(process_alive(std::process::id()));
        // Out of range is "alive": never `kill(-1, 0)`.
        assert!(process_alive(u32::MAX));

        assert!(orphaned_session_temporary(
            ".session.json.tmp-4242-7",
            1,
            |_| false
        ));
        // A living writer, this process, and anything not a temporary stay.
        assert!(!orphaned_session_temporary(
            ".session.json.tmp-4242-7",
            1,
            |_| true
        ));
        assert!(!orphaned_session_temporary(
            ".session.json.tmp-4242-7",
            4242,
            |_| false
        ));
        assert!(!orphaned_session_temporary("session.json", 1, |_| false));
        assert!(!orphaned_session_temporary(
            ".session.json.tmp-abc-7",
            1,
            |_| false
        ));

        let root = TestDir::new("sweep");
        let dir = root.0.join("state");
        fs::create_dir_all(&dir).unwrap();
        let orphan = dir.join(format!("{SESSION_TEMPORARY_PREFIX}{dead_pid}-0"));
        fs::write(&orphan, "half-written").unwrap();
        let own = dir.join(format!(
            "{SESSION_TEMPORARY_PREFIX}{}-999999",
            std::process::id()
        ));
        fs::write(&own, "in flight").unwrap();

        let path = dir.join("session.json");
        atomic_write_session(&path, br#"{"version":2,"clients":[]}"#).unwrap();

        assert!(!orphan.exists(), "the dead writer's temporary was kept");
        assert!(
            own.exists(),
            "a temporary this process may still be renaming was removed"
        );
        assert_eq!(
            load_session_snapshot(&path).unwrap().version,
            SESSION_VERSION
        );
    }

    #[test]
    fn session_loader_keeps_using_the_inode_it_opened() {
        let root = TestDir::new("read-inode");
        let path = root.0.join("state").join("session.json");
        atomic_write_session(&path, br#"{"version":2,"clients":[]}"#).unwrap();

        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .unwrap();
        let original = root.0.join("original.json");
        fs::rename(&path, &original).unwrap();
        fs::write(&path, "not JSON").unwrap();

        let snapshot = load_open_session_snapshot(opened, &path).unwrap();
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert!(load_session_snapshot(&path).is_err());
    }

    #[test]
    fn missing_snapshot_remains_distinguishable_from_an_invalid_snapshot() {
        let root = TestDir::new("missing");
        let missing = root.0.join("missing.json");
        let error = load_session_snapshot(&missing).unwrap_err();
        assert!(
            error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
        );

        let invalid = root.0.join("invalid.json");
        fs::write(&invalid, "not JSON").unwrap();
        let error = load_session_snapshot(&invalid).unwrap_err();
        assert!(error.downcast_ref::<io::Error>().is_none());
    }

    #[test]
    fn session_loader_rejects_a_fifo_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt as _;

        let root = TestDir::new("fifo-load");
        let path = root.0.join("session.json");
        let path_bytes = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);

        let error = load_session_snapshot(&path).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn capture_uses_live_monitor_relation() {
        let mut state = WMState::new();
        let mut monitor = WMMonitor::new();
        monitor.num = 7;
        let monitor_key = state.monitors.insert(monitor);
        state.monitor_order.push(monitor_key);

        let mut client = WMClient::new(WindowId::from_raw(42));
        client.class = "Term".into();
        client.instance = "kitty".into();
        client.state.tags = 1;
        client.mon = Some(monitor_key);
        let client_key = state.clients.insert(client);
        state.client_order.push(client_key);

        let snapshot = capture_snapshot(&state, "status-bar");
        assert_eq!(snapshot.clients[0].monitor_num, 7);
    }

    #[test]
    fn restored_tags_are_masked_and_have_a_safe_fallback() {
        assert_eq!(sanitize_tags(0b1_0000, 0b1111, 0b0100), 0b0100);
        assert_eq!(sanitize_tags(0b1010, 0b0111, 0b0001), 0b0010);
        assert_eq!(sanitize_tags(0, 0b1111, 0), 1);
    }

    #[test]
    fn floating_geometry_is_resized_and_clamped_to_monitor() {
        assert_eq!(
            clamp_floating_rect((-500, 900, 2000, 0), Rect::new(100, 200, 800, 600)),
            (100, 799, 800, 1)
        );
    }

    #[test]
    fn plan_restore_matches_by_class_and_instance() {
        let snap = snapshot_with_clients(vec![entry("Firefox", "Navigator", 0b100)]);
        let k = keys(1);
        let plans = plan_restore(&snap, vec![(k[0], "firefox", "navigator")]);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].0, k[0]);
        assert_eq!(plans[0].1.tags, 0b100);
        assert_eq!(
            plan_restore_detailed(&snap, vec![(k[0], "firefox", "navigator")])[0]
                .1
                .monitor_num,
            0
        );
    }

    #[test]
    fn plan_restore_no_match_returns_empty() {
        let snap = snapshot_with_clients(vec![entry("Firefox", "Navigator", 0b100)]);
        let k = keys(1);
        let plans = plan_restore(&snap, vec![(k[0], "Alacritty", "alacritty")]);
        assert!(plans.is_empty());
    }

    #[test]
    fn plan_restore_each_entry_used_at_most_once() {
        // Two terminals saved on different tags; two open terminals should map
        // to distinct entries rather than both matching the first.
        let snap = snapshot_with_clients(vec![entry("Term", "", 0b1), entry("Term", "", 0b10)]);
        let k = keys(2);
        let plans = plan_restore(&snap, vec![(k[0], "Term", ""), (k[1], "Term", "")]);
        assert_eq!(plans.len(), 2);
        let tags: Vec<u32> = plans.iter().map(|(_, p)| p.tags).collect();
        assert!(tags.contains(&0b1) && tags.contains(&0b10));
    }

    #[test]
    fn plan_restore_prefers_exact_instance_over_fallback() {
        // Entry 0: class matches, no instance (fallback). Entry 1: exact instance.
        let snap =
            snapshot_with_clients(vec![entry("Term", "", 0b1), entry("Term", "kitty", 0b1000)]);
        let k = keys(1);
        let plans = plan_restore(&snap, vec![(k[0], "Term", "kitty")]);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].1.tags, 0b1000, "should pick exact-instance entry");
    }

    const SESSION_V1_FIXTURE: &str = include_str!("../../tests/fixtures/session_v1.json");
    const SESSION_V2_FIXTURE: &str = include_str!("../../tests/fixtures/session_v2.json");
    const SESSION_V3_FIXTURE: &str = include_str!("../../tests/fixtures/session_v3.json");
    const SESSION_V4_FIXTURE: &str = include_str!("../../tests/fixtures/session_v4.json");

    #[test]
    fn recorded_v1_snapshot_migrates_tolerantly_and_normalizes_floating_state() {
        let snapshot = migrate_session_json(SESSION_V1_FIXTURE).unwrap();

        assert_eq!(snapshot.version, SESSION_VERSION);
        assert_eq!(snapshot.clients.len(), 3);

        // 完整的 v1 条目原样保留。
        assert_eq!(snapshot.clients[0].floating, Some((40, 60, 1024, 768)));
        assert_eq!(snapshot.clients[0].monitor_num, 1);

        // 缺失的 name / monitor_num 使用缺省值；非浮动条目上的浮动几何
        // （v1 从未拒绝过这种文件）被归一化掉，而不是让恢复整体失败。
        assert_eq!(snapshot.clients[1].name, "");
        assert_eq!(snapshot.clients[1].monitor_num, 0);
        assert_eq!(snapshot.clients[1].floating, None);

        // 浮动但尺寸非法的条目保留浮动标志、丢弃几何。
        assert!(snapshot.clients[2].is_floating);
        assert_eq!(snapshot.clients[2].floating, None);

        // v1 没有每显示器顺序列表，迁移结果为空列表（恢复时不重排）。
        assert!(snapshot.monitor_orders.is_empty());
    }

    #[test]
    fn recorded_v2_snapshot_loads_unchanged() {
        let snapshot = migrate_session_json(SESSION_V2_FIXTURE).unwrap();
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert_eq!(snapshot.clients.len(), 2);
        assert_eq!(snapshot.clients[0].floating, Some((40, 60, 1024, 768)));
        assert_eq!(snapshot.clients[1].floating, None);
        // v2 同样没有顺序数据。
        assert!(snapshot.monitor_orders.is_empty());
    }

    #[test]
    fn recorded_v3_snapshot_loads_with_monitor_order() {
        let snapshot = migrate_session_json(SESSION_V3_FIXTURE).unwrap();
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert_eq!(snapshot.clients.len(), 2);
        assert_eq!(snapshot.monitor_orders.len(), 2);
        assert_eq!(snapshot.monitor_orders[0].monitor_num, 0);
        assert_eq!(
            snapshot.monitor_orders[0].clients,
            vec![identity("Alacritty", "alacritty")]
        );
        assert_eq!(snapshot.monitor_orders[1].monitor_num, 1);
        assert_eq!(
            snapshot.monitor_orders[1].clients,
            vec![identity("Firefox", "Navigator")]
        );
        assert!(
            snapshot
                .clients
                .iter()
                .all(|entry| entry.maximize.is_none()),
            "a v3 file migrates with no maximize state"
        );
        assert!(
            snapshot
                .clients
                .iter()
                .all(|entry| entry.connector.is_none())
                && snapshot
                    .monitor_orders
                    .iter()
                    .all(|order| order.connector.is_none()),
            "a v3 file migrates with no connector"
        );

        // `capture_snapshot` derives both lists from the same monitor
        // relation, so the frozen file must agree with itself: every ordered
        // identity is an entry that says it lives on that monitor.
        for order in &snapshot.monitor_orders {
            for identity in &order.clients {
                assert!(
                    snapshot.clients.iter().any(|entry| {
                        entry.class == identity.class
                            && entry.instance == identity.instance
                            && entry.monitor_num == order.monitor_num
                    }),
                    "{}/{} is ordered on monitor {} but no entry lives there",
                    identity.class,
                    identity.instance,
                    order.monitor_num
                );
            }
        }
    }

    #[test]
    fn recorded_v4_snapshot_loads_without_connector() {
        let snapshot = migrate_session_json(SESSION_V4_FIXTURE).unwrap();
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert_eq!(snapshot.clients.len(), 2);
        assert_eq!(snapshot.monitor_orders.len(), 2);
        assert!(
            snapshot
                .clients
                .iter()
                .all(|entry| entry.connector.is_none())
                && snapshot
                    .monitor_orders
                    .iter()
                    .all(|order| order.connector.is_none()),
            "a v4 file without connector still loads"
        );
        assert_eq!(
            snapshot.clients[0].maximize,
            Some(SessionMaximize {
                vert: true,
                horz: true,
                restore: Some((40, 60, 1024, 768)),
                promoted: false,
            })
        );
        assert_eq!(snapshot.clients[0].monitor_num, 1);
        assert_eq!(snapshot.monitor_orders[1].monitor_num, 1);
    }

    /// The recorded fixture above agrees with itself because
    /// `capture_snapshot` derives both lists from one monitor relation. A
    /// file on disk need not: it can be hand-edited, or written before a
    /// monitor was unplugged. Ordering an identity on a monitor its own
    /// entry does not claim is not a reason to throw the whole session away
    /// — `restore_monitor_client_order` intersects the saved order with what
    /// is actually on the monitor, so the mismatch simply orders nothing.
    #[test]
    fn a_monitor_order_that_disagrees_with_its_entries_still_loads() {
        let document = r#"{
            "version": 3,
            "clients": [
                {
                    "class": "Firefox",
                    "instance": "Navigator",
                    "name": "Reference page",
                    "tags": 5,
                    "is_floating": false,
                    "monitor_num": 1,
                    "floating": null
                }
            ],
            "monitor_orders": [
                {
                    "monitor_num": 0,
                    "clients": [{ "class": "Firefox", "instance": "Navigator" }]
                }
            ]
        }"#;

        let snapshot = migrate_session_json(document).expect("a disagreeing v3 file still loads");
        snapshot
            .validate()
            .expect("a cross-monitor disagreement is not a validation failure");
        assert_eq!(snapshot.clients[0].monitor_num, 1);
        assert_eq!(snapshot.monitor_orders[0].monitor_num, 0);
        assert_eq!(
            snapshot.monitor_orders[0].clients,
            vec![identity("Firefox", "Navigator")]
        );
    }

    #[test]
    fn migration_refuses_future_versions_and_unreadable_documents() {
        let future = format!(
            r#"{{"version":{},"clients":[],"monitor_orders":[]}}"#,
            SESSION_VERSION + 1
        );
        let error = migrate_session_json(&future).unwrap_err();
        assert!(
            error.contains(&format!(
                "unsupported session version {}",
                SESSION_VERSION + 1
            )),
            "{error}"
        );

        let error = migrate_session_json("not JSON").unwrap_err();
        assert!(error.contains("no readable version"));

        // v2 保持严格：缺字段说明写入方出了问题，迁移不做静默补全。
        let error =
            migrate_session_json(r#"{"version":2,"clients":[{"class":"A","instance":"a"}]}"#)
                .unwrap_err();
        assert!(error.contains("cannot parse version 2 session snapshot"));

        // v3 保持严格：缺 monitor_orders 字段直接拒绝。
        let error = migrate_session_json(r#"{"version":3,"clients":[]}"#).unwrap_err();
        assert!(error.contains("cannot parse version 3 session snapshot"));

        // v4 保持严格：缺 monitor_orders 字段直接拒绝。
        let error = migrate_session_json(r#"{"version":4,"clients":[]}"#).unwrap_err();
        assert!(error.contains("cannot parse version 4 session snapshot"));

        // v5 保持严格：缺 monitor_orders 字段直接拒绝。
        let error = migrate_session_json(r#"{"version":5,"clients":[]}"#).unwrap_err();
        assert!(error.contains("cannot parse version 5 session snapshot"));

        // v6 保持严格：缺 monitor_orders 字段直接拒绝。
        let error = migrate_session_json(r#"{"version":6,"clients":[]}"#).unwrap_err();
        assert!(error.contains("cannot parse version 6 session snapshot"));

        // v7 保持严格：缺 monitor_orders 字段直接拒绝。
        let error = migrate_session_json(r#"{"version":7,"clients":[]}"#).unwrap_err();
        assert!(error.contains("cannot parse version 7 session snapshot"));

        // v8 保持严格：缺 monitor_orders 字段直接拒绝。
        let error = migrate_session_json(r#"{"version":8,"clients":[]}"#).unwrap_err();
        assert!(error.contains("cannot parse version 8 session snapshot"));

        // v9 保持严格：缺 monitor_orders 字段直接拒绝。
        let error = migrate_session_json(r#"{"version":9,"clients":[]}"#).unwrap_err();
        assert!(error.contains("cannot parse version 9 session snapshot"));

        // v10 保持严格：缺 monitor_orders 字段直接拒绝。
        let error = migrate_session_json(r#"{"version":10,"clients":[]}"#).unwrap_err();
        assert!(error.contains("cannot parse version 10 session snapshot"));

        // 当前版本同样严格：缺 monitor_orders 字段直接拒绝。
        let error =
            migrate_session_json(&format!(r#"{{"version":{SESSION_VERSION},"clients":[]}}"#))
                .unwrap_err();
        assert!(error.contains("cannot parse session snapshot"));
    }

    #[test]
    fn v5_snapshot_without_sticky_migrates_to_false() {
        let snapshot = migrate_session_json(
            r#"{"version":5,"clients":[{"class":"A","instance":"a","name":"","tags":1,"is_floating":false,"monitor_num":0,"floating":null}],"monitor_orders":[]}"#,
        )
        .expect("v5 without is_sticky still loads");
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert!(!snapshot.clients[0].is_sticky);
        assert!(!snapshot.clients[0].is_above);
        assert!(!snapshot.clients[0].is_below);
        assert!(!snapshot.clients[0].is_minimized);
        assert!(!snapshot.clients[0].is_fullscreen);
        assert!(!snapshot.clients[0].is_pip);
    }

    #[test]
    fn v6_snapshot_without_stacking_migrates_to_false() {
        let snapshot = migrate_session_json(
            r#"{"version":6,"clients":[{"class":"A","instance":"a","name":"","tags":1,"is_floating":false,"monitor_num":0,"floating":null,"is_sticky":true}],"monitor_orders":[]}"#,
        )
        .expect("v6 without is_above/is_below still loads");
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert!(snapshot.clients[0].is_sticky);
        assert!(!snapshot.clients[0].is_above);
        assert!(!snapshot.clients[0].is_below);
        assert!(!snapshot.clients[0].is_minimized);
        assert!(!snapshot.clients[0].is_fullscreen);
        assert!(!snapshot.clients[0].is_pip);
    }

    #[test]
    fn v7_snapshot_without_minimized_migrates_to_false() {
        let snapshot = migrate_session_json(
            r#"{"version":7,"clients":[{"class":"A","instance":"a","name":"","tags":1,"is_floating":false,"monitor_num":0,"floating":null,"is_sticky":false,"is_above":false,"is_below":false}],"monitor_orders":[]}"#,
        )
        .expect("v7 without is_minimized still loads");
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert!(!snapshot.clients[0].is_minimized);
        assert!(!snapshot.clients[0].is_fullscreen);
        assert!(!snapshot.clients[0].is_pip);
    }

    #[test]
    fn v8_snapshot_without_fullscreen_or_pip_migrates_to_false() {
        let snapshot = migrate_session_json(
            r#"{"version":8,"clients":[{"class":"A","instance":"a","name":"","tags":1,"is_floating":false,"monitor_num":0,"floating":null,"is_sticky":false,"is_above":false,"is_below":false,"is_minimized":true}],"monitor_orders":[]}"#,
        )
        .expect("v8 without is_fullscreen/is_pip still loads");
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert!(snapshot.clients[0].is_minimized);
        assert!(!snapshot.clients[0].is_fullscreen);
        assert!(!snapshot.clients[0].is_pip);
        assert!((snapshot.clients[0].client_fact - 1.0).abs() < f32::EPSILON);
        assert!(!snapshot.clients[0].is_drag_floating);
        assert!(!snapshot.clients[0].no_decorations);
    }

    #[test]
    fn v9_snapshot_without_client_fact_migrates_to_defaults() {
        let snapshot = migrate_session_json(
            r#"{"version":9,"clients":[{"class":"A","instance":"a","name":"","tags":1,"is_floating":false,"monitor_num":0,"floating":null,"is_sticky":false,"is_above":false,"is_below":false,"is_minimized":false,"is_fullscreen":false,"is_pip":false}],"monitor_orders":[]}"#,
        )
        .expect("v9 without client_fact still loads");
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert!((snapshot.clients[0].client_fact - 1.0).abs() < f32::EPSILON);
        assert!(!snapshot.clients[0].is_drag_floating);
        assert!(!snapshot.clients[0].no_decorations);
        assert!(!snapshot.clients[0].is_urgent);
        assert!(snapshot.clients[0].border_w.is_none());
    }

    #[test]
    fn v10_snapshot_without_urgency_fields_migrates_to_defaults() {
        let snapshot = migrate_session_json(
            r#"{"version":10,"clients":[{"class":"A","instance":"a","name":"","tags":1,"is_floating":false,"monitor_num":0,"floating":null,"is_sticky":false,"is_above":false,"is_below":false,"is_minimized":false,"is_fullscreen":false,"is_pip":false,"client_fact":1.25,"is_drag_floating":false,"no_decorations":false}],"monitor_orders":[]}"#,
        )
        .expect("v10 without urgency fields still loads");
        assert_eq!(snapshot.version, SESSION_VERSION);
        assert!((snapshot.clients[0].client_fact - 1.25).abs() < f32::EPSILON);
        assert!(!snapshot.clients[0].is_urgent);
        assert!(!snapshot.clients[0].demands_attention);
        assert!(!snapshot.clients[0].skip_taskbar);
        assert!(!snapshot.clients[0].skip_pager);
        assert!(!snapshot.clients[0].is_fixed);
        assert!(snapshot.clients[0].border_w.is_none());
    }

    #[test]
    fn session_captures_and_restores_client_fact_hand_float_and_decorations() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let tags = jwm.state.monitors[monitor].get_active_tags();

        let mut client = WMClient::new(WindowId::from_raw(0x81));
        client.class = "FactApp".into();
        client.instance = "factapp".into();
        client.mon = Some(monitor);
        client.state.tags = tags;
        client.state.is_floating = true;
        client.state.is_drag_floating = true;
        client.state.client_fact = 1.5;
        client.state.no_decorations = true;
        client.geometry.border_w = 0;
        client.geometry.x = 40;
        client.geometry.y = 50;
        client.geometry.w = 400;
        client.geometry.h = 300;
        client.geometry.floating_x = 40;
        client.geometry.floating_y = 50;
        client.geometry.floating_w = 400;
        client.geometry.floating_h = 300;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);

        let snap = capture_snapshot(&jwm.state, "status-bar");
        let entry = snap
            .clients
            .iter()
            .find(|e| e.class == "FactApp")
            .expect("FactApp");
        assert!((entry.client_fact - 1.5).abs() < f32::EPSILON);
        assert!(entry.is_drag_floating);
        assert!(entry.no_decorations);

        jwm.state.clients[key].state.client_fact = 1.0;
        jwm.state.clients[key].state.is_drag_floating = false;
        jwm.state.clients[key].state.no_decorations = false;
        jwm.state.clients[key].geometry.border_w = 2;
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snap), 1);
        let restored = &jwm.state.clients[key].state;
        assert!((restored.client_fact - 1.5).abs() < f32::EPSILON);
        assert!(restored.is_drag_floating);
        assert!(restored.no_decorations);
        assert_eq!(jwm.state.clients[key].geometry.border_w, 0);
    }

    #[test]
    fn session_captures_and_restores_urgency_attention_skip_fixed_and_border() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let tags = jwm.state.monitors[monitor].get_active_tags();

        let mut client = WMClient::new(WindowId::from_raw(0x82));
        client.class = "UrgentApp".into();
        client.instance = "urgent".into();
        client.mon = Some(monitor);
        client.state.tags = tags;
        client.state.is_urgent = true;
        client.state.demands_attention = true;
        client.state.skip_taskbar = true;
        client.state.skip_pager = true;
        client.state.is_fixed = true;
        client.geometry.border_w = 4;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);

        let snap = capture_snapshot(&jwm.state, "status-bar");
        let entry = snap
            .clients
            .iter()
            .find(|e| e.class == "UrgentApp")
            .expect("UrgentApp");
        assert!(entry.is_urgent);
        assert!(entry.demands_attention);
        assert!(entry.skip_taskbar);
        assert!(entry.skip_pager);
        assert!(entry.is_fixed);
        assert_eq!(entry.border_w, Some(4));
        assert_eq!(snap.version, SESSION_VERSION);

        jwm.state.clients[key].state.is_urgent = false;
        jwm.state.clients[key].state.demands_attention = false;
        jwm.state.clients[key].state.skip_taskbar = false;
        jwm.state.clients[key].state.skip_pager = false;
        jwm.state.clients[key].state.is_fixed = false;
        jwm.state.clients[key].geometry.border_w = 2;
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snap), 1);
        let restored = &jwm.state.clients[key];
        assert!(restored.state.is_urgent);
        assert!(restored.state.demands_attention);
        assert!(restored.state.skip_taskbar);
        assert!(restored.state.skip_pager);
        assert!(restored.state.is_fixed);
        assert_eq!(restored.geometry.border_w, 4);
    }

    #[test]
    fn session_restore_reapplies_maximize_promoted_via_adopt() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let tags = jwm.state.monitors[monitor].get_active_tags();

        let mut tiled = WMClient::new(WindowId::from_raw(0x91));
        tiled.class = "TileApp".into();
        tiled.instance = "tileapp".into();
        tiled.mon = Some(monitor);
        tiled.state.tags = tags;
        tiled.geometry.x = 0;
        tiled.geometry.y = 30;
        tiled.geometry.w = 900;
        tiled.geometry.h = 700;
        tiled.geometry.border_w = 2;
        let key = jwm.insert_client(tiled);
        jwm.attach_to_monitor(key, monitor);

        jwm.set_client_maximized(&mut backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
            .expect("promote maximize");
        assert!(jwm.state.clients[key].state.maximize_restore_tiled);

        let snap = capture_snapshot(&jwm.state, "status-bar");
        let entry = snap
            .clients
            .iter()
            .find(|e| e.class == "TileApp")
            .expect("TileApp");
        assert_eq!(
            entry.maximize.as_ref().map(|m| m.promoted),
            Some(true),
            "capture must record promoted"
        );

        jwm.set_client_maximized(&mut backend, key, MaximizeAxes::NONE, MaximizeOrigin::User)
            .expect("clear maximize");
        assert!(!jwm.state.clients[key].state.maximize_restore_tiled);

        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snap), 1);
        assert!(
            jwm.state.clients[key].state.maximize_restore_tiled,
            "restore must re-promote via adopt_client_maximized"
        );
        assert!(jwm.state.clients[key].state.maximized_axes().any());
    }

    #[test]
    fn loading_an_old_snapshot_never_rewrites_the_on_disk_file() {
        let root = TestDir::new("migrate");
        let path = root.0.join("state").join("session.json");
        let v1_bytes = SESSION_V1_FIXTURE.as_bytes();
        atomic_write_session(&path, v1_bytes).unwrap();

        let snapshot = load_session_snapshot(&path).unwrap();
        assert_eq!(snapshot.version, SESSION_VERSION);

        // 迁移必须是纯内存升级：旧文件逐字节保持原样，直到下一次
        // save_session 原子写成功，崩溃或降级安装随时可以回滚。
        assert_eq!(fs::read(&path).unwrap(), v1_bytes);
    }

    #[test]
    fn plan_restore_class_match_but_instance_differs_is_skipped_when_both_present() {
        let snap = snapshot_with_clients(vec![entry("Term", "kitty", 0b1)]);
        let k = keys(1);
        // Both have instance, but they differ -> no match.
        let plans = plan_restore(&snap, vec![(k[0], "Term", "alacritty")]);
        assert!(plans.is_empty());
    }

    #[test]
    fn capture_snapshot_records_the_pre_maximize_state() {
        let mut state = WMState::new();
        let mut monitor = WMMonitor::new();
        monitor.num = 0;
        let monitor_key = state.monitors.insert(monitor);
        state.monitor_order.push(monitor_key);

        let restore = Rect::new(300, 200, 640, 480);
        for (raw, class, promoted) in [(1, "Editor", false), (2, "Browser", true)] {
            let mut client = WMClient::new(WindowId::from_raw(raw));
            client.class = class.into();
            client.instance = class.to_lowercase();
            client.state.tags = 1;
            client.mon = Some(monitor_key);
            client.state.is_floating = true;
            client.state.set_maximized_axes(MaximizeAxes::BOTH);
            client.state.maximize_restore_tiled = promoted;
            client.geometry.x = 0;
            client.geometry.y = 30;
            client.geometry.w = 1916;
            client.geometry.h = 1046;
            client.geometry.maximize_restore_rect = Some(restore);
            if promoted {
                // Promotion keeps its own pre-promotion floating rect.
                client.geometry.floating_x = 50;
                client.geometry.floating_y = 60;
                client.geometry.floating_w = 700;
                client.geometry.floating_h = 500;
            } else {
                client.geometry.floating_x = restore.x;
                client.geometry.floating_y = restore.y;
                client.geometry.floating_w = restore.w;
                client.geometry.floating_h = restore.h;
            }
            assert!(client.state.is_maximize_realized());
            let key = state.clients.insert(client);
            state.client_order.push(key);
        }

        let snapshot = capture_snapshot(&state, "status-bar");
        assert_eq!(snapshot.clients.len(), 2);
        let floating = &snapshot.clients[0];
        assert!(floating.is_floating);
        assert_eq!(
            floating.floating,
            Some((restore.x, restore.y, restore.w, restore.h)),
            "a maximized floating window is saved at its pre-maximize rect"
        );
        assert_eq!(
            floating.maximize,
            Some(SessionMaximize {
                vert: true,
                horz: true,
                restore: Some((restore.x, restore.y, restore.w, restore.h)),
                promoted: false,
            })
        );
        let promoted = &snapshot.clients[1];
        assert!(
            !promoted.is_floating,
            "a window maximize pulled out of the layout is saved tiled"
        );
        assert_eq!(promoted.floating, None);
        assert_eq!(
            promoted.maximize,
            Some(SessionMaximize {
                vert: true,
                horz: true,
                restore: Some((restore.x, restore.y, restore.w, restore.h)),
                promoted: true,
            })
        );
        assert!(snapshot.validate().is_ok());
    }

    /// Regression: capture only recognised a *realized* maximize, so a
    /// maximized window in PiP was saved floating at the maximized rect PiP
    /// keeps in floating_* (restoring as an unmaximized window covering the
    /// work area), and a promoted one in PiP or fullscreen came back
    /// floating instead of tiled.
    #[test]
    fn capture_snapshot_sees_maximize_under_pip_and_fullscreen() {
        let mut state = WMState::new();
        let mut monitor = WMMonitor::new();
        monitor.num = 0;
        let monitor_key = state.monitors.insert(monitor);
        state.monitor_order.push(monitor_key);

        let restore = Rect::new(300, 200, 640, 480);
        let maximized = Rect::new(0, 30, 1916, 1046);
        let cases = [
            (1, "Pip", false, true, false),
            (2, "PromotedPip", true, true, false),
            (3, "PromotedFullscreen", true, false, true),
        ];
        for (raw, class, promoted, pip, fullscreen) in cases {
            let mut client = WMClient::new(WindowId::from_raw(raw));
            client.class = class.into();
            client.instance = class.to_lowercase();
            client.state.tags = 1;
            client.mon = Some(monitor_key);
            client.state.is_floating = true;
            client.state.old_state = true;
            client.state.is_pip = pip;
            client.state.is_fullscreen = fullscreen;
            client.state.set_maximized_axes(MaximizeAxes::BOTH);
            client.state.maximize_restore_tiled = promoted;
            client.geometry.maximize_restore_rect = Some(restore);
            // PiP's return slot: the maximized rect it was entered from.
            client.geometry.floating_x = maximized.x;
            client.geometry.floating_y = maximized.y;
            client.geometry.floating_w = maximized.w;
            client.geometry.floating_h = maximized.h;
            assert!(!client.state.is_maximize_realized());
            let key = state.clients.insert(client);
            state.client_order.push(key);
        }

        let snapshot = capture_snapshot(&state, "status-bar");
        assert_eq!(snapshot.clients.len(), 3);
        let pip = &snapshot.clients[0];
        assert!(pip.is_floating);
        assert_eq!(
            pip.floating,
            Some((restore.x, restore.y, restore.w, restore.h)),
            "a maximized window in PiP is saved at its pre-maximize rect"
        );
        assert_eq!(
            pip.maximize,
            Some(SessionMaximize {
                vert: true,
                horz: true,
                restore: Some((restore.x, restore.y, restore.w, restore.h)),
                promoted: false,
            })
        );
        for promoted in &snapshot.clients[1..] {
            assert!(
                !promoted.is_floating,
                "{}: a promoted window is saved tiled",
                promoted.class
            );
            assert_eq!(promoted.floating, None, "{}", promoted.class);
            assert_eq!(
                promoted.maximize,
                Some(SessionMaximize {
                    vert: true,
                    horz: true,
                    restore: Some((restore.x, restore.y, restore.w, restore.h)),
                    promoted: true,
                }),
                "{}",
                promoted.class
            );
        }
        assert!(snapshot.validate().is_ok());
    }

    #[test]
    fn capture_exports_monitor_clients_order_and_skips_bars_and_docks() {
        let mut state = WMState::new();
        let mut monitor = WMMonitor::new();
        monitor.num = 0;
        let monitor_key = state.monitors.insert(monitor);
        state.monitor_order.push(monitor_key);

        let mut ordered_keys = Vec::new();
        for (raw, class, instance) in [
            (1, "Firefox", "Navigator"),
            (2, "Term", "kitty"),
            (3, "xbar", "xbar"),
            (4, "Gimp", "gimp"),
        ] {
            let mut client = WMClient::new(WindowId::from_raw(raw));
            client.class = class.into();
            client.instance = instance.into();
            client.state.tags = 1;
            client.mon = Some(monitor_key);
            if class == "xbar" {
                client.state.is_dock = true;
            }
            let key = state.clients.insert(client);
            state.client_order.push(key);
            ordered_keys.push(key);
        }
        state.monitor_clients.insert(monitor_key, ordered_keys);

        let snapshot = capture_snapshot(&state, "status-bar");
        assert_eq!(snapshot.monitor_orders.len(), 1);
        assert_eq!(snapshot.monitor_orders[0].monitor_num, 0);
        // 顺序即 monitor_clients 顺序；dock 窗口与 clients 列表一样被跳过。
        assert_eq!(
            snapshot.monitor_orders[0].clients,
            vec![
                identity("Firefox", "Navigator"),
                identity("Term", "kitty"),
                identity("Gimp", "gimp"),
            ]
        );
    }

    /// The window classes a capture saves as the order of a monitor listing
    /// `list`. Each class in `promoted` was pulled out of the tiles by
    /// maximize and anchored on the named window; each in `floating` is a
    /// plain floating window.
    fn saved_order(
        list: &[&str],
        promoted: &[(&str, Option<&str>)],
        floating: &[&str],
    ) -> Vec<String> {
        let mut state = WMState::new();
        let mut monitor = WMMonitor::new();
        monitor.num = 0;
        let monitor_key = state.monitors.insert(monitor);
        state.monitor_order.push(monitor_key);
        let mut keys = Vec::new();
        for (raw, &class) in (0x40..).zip(list) {
            let mut client = WMClient::new(WindowId::from_raw(raw));
            client.class = class.into();
            client.instance = class.to_lowercase();
            client.state.tags = 1;
            client.mon = Some(monitor_key);
            client.state.is_floating = floating.contains(&class);
            let key = state.clients.insert(client);
            state.client_order.push(key);
            keys.push((class, key));
        }
        let key_of = |class: &str| {
            keys.iter()
                .find(|(listed, _)| *listed == class)
                .map(|&(_, key)| key)
        };
        for &(class, anchor) in promoted {
            let Some(key) = key_of(class) else {
                panic!("{class} is not listed");
            };
            let client = &mut state.clients[key];
            client.state.is_floating = true;
            client.state.set_maximized_axes(MaximizeAxes::BOTH);
            client.state.maximize_restore_tiled = true;
            client.state.maximize_restore_anchor = anchor.and_then(key_of);
        }
        state
            .monitor_clients
            .insert(monitor_key, keys.iter().map(|&(_, key)| key).collect());
        capture_snapshot(&state, "status-bar").monitor_orders[0]
            .clients
            .iter()
            .map(|identity| identity.class.clone())
            .collect()
    }

    /// Regression: the saved order was the raw client list, where maximize
    /// had moved a promoted tile to the floating tail. The window is saved
    /// tiled, so the restore re-tiled it after every other tile: saving with
    /// the master maximized made the next tile master.
    #[test]
    fn capture_saves_a_promoted_tile_in_its_slot() {
        let cases: [(&[&str], &[(&str, Option<&str>)], &[&str], &[&str]); 8] = [
            // The master, promoted in front of B.
            (&["B", "C", "A"], &[("A", Some("B"))], &[], &["A", "B", "C"]),
            // A middle tile.
            (&["A", "C", "B"], &[("B", Some("C"))], &[], &["A", "B", "C"]),
            // Neighbours anchored on each other, promoted in either order.
            (
                &["C", "A", "B"],
                &[("A", Some("B")), ("B", Some("C"))],
                &[],
                &["A", "B", "C"],
            ),
            (
                &["C", "B", "A"],
                &[("A", Some("B")), ("B", Some("C"))],
                &[],
                &["A", "B", "C"],
            ),
            // Every tile out, the last with no tile after it.
            (
                &["C", "B", "A"],
                &[("A", Some("B")), ("B", Some("C")), ("C", None)],
                &[],
                &["A", "B", "C"],
            ),
            // The last tile rests at the end of the tiled group, still
            // ahead of a plain floating window.
            (
                &["A", "B", "F", "C"],
                &[("C", None)],
                &["F"],
                &["A", "B", "C", "F"],
            ),
            // An anchor floated since (regrouped behind the promoted window)
            // has no slot to offer.
            (
                &["C", "A", "B"],
                &[("A", Some("B"))],
                &["B"],
                &["C", "A", "B"],
            ),
            // Anchors naming each other still leave every window saved.
            (
                &["C", "A", "B"],
                &[("A", Some("B")), ("B", Some("A"))],
                &[],
                &["C", "A", "B"],
            ),
        ];
        for (list, promoted, floating, expected) in cases {
            assert_eq!(
                saved_order(list, promoted, floating),
                expected,
                "list {list:?}, promoted {promoted:?}, floating {floating:?}"
            );
        }
    }

    /// The master maximized when the session is saved: the restore, in the
    /// same session or over windows mapped in another order, makes it master
    /// again.
    #[test]
    fn a_session_saved_with_the_master_maximized_restores_it_as_master() {
        use crate::core::layout::LayoutEnum;
        use std::rc::Rc;

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        jwm.state.monitors[monitor].lt = Rc::new(LayoutEnum::TILE);
        let tags = jwm.state.monitors[monitor].get_active_tags();
        let [a, b, c] = [(0x71, "A"), (0x72, "B"), (0x73, "C")].map(|(raw, class)| {
            let mut client = WMClient::new(WindowId::from_raw(raw));
            client.class = class.into();
            client.instance = class.to_lowercase();
            client.mon = Some(monitor);
            client.state.tags = tags;
            let key = jwm.insert_client(client);
            jwm.attach_to_monitor(key, monitor);
            key
        });
        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(a));
        jwm.arrange(&mut backend, Some(monitor));
        jwm.togglemaximize(&mut backend, &WMArgEnum::Int(0))
            .expect("togglemaximize");
        assert_eq!(jwm.state.monitor_clients[monitor], vec![b, c, a]);

        let snapshot = capture_snapshot(&jwm.state, "status-bar");
        assert_eq!(
            snapshot.monitor_orders[0].clients,
            vec![identity("A", "a"), identity("B", "b"), identity("C", "c")]
        );

        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snapshot), 3);
        assert!(jwm.state.clients[a].state.maximize_restore_tiled);
        assert_eq!(
            jwm.state.clients[a].state.maximized_axes(),
            MaximizeAxes::BOTH
        );
        // While maximized, a promoted window sits in the floating tail —
        // same as the live togglemaximize that produced the snapshot.
        assert_eq!(jwm.state.monitor_clients[monitor], vec![b, c, a]);
        jwm.togglemaximize(&mut backend, &WMArgEnum::Int(0))
            .expect("unmaximize");
        assert_eq!(
            jwm.state.monitor_clients[monitor],
            vec![a, b, c],
            "unmaximize returns the restored master to its tile slot"
        );
        assert!(!jwm.state.clients[a].state.maximized_axes().any());

        jwm.state.monitor_clients.insert(monitor, vec![c, b, a]);
        jwm.apply_session_snapshot(&mut backend, &snapshot);
        assert_eq!(jwm.state.monitor_clients[monitor], vec![b, c, a]);
        assert_eq!(
            jwm.state.clients[a].state.maximized_axes(),
            MaximizeAxes::BOTH
        );
    }

    #[test]
    fn plan_order_restore_full_match_uses_saved_order() {
        let saved = vec![identity("B", "b"), identity("A", "a")];
        let current = vec![(0, identity("A", "a")), (1, identity("B", "b"))];
        assert_eq!(plan_order_restore(&saved, &current), vec![1, 0]);
    }

    #[test]
    fn plan_order_restore_ignores_saved_identities_that_disappeared() {
        let saved = vec![identity("B", "b"), identity("X", "x"), identity("A", "a")];
        let current = vec![(0, identity("A", "a")), (1, identity("B", "b"))];
        assert_eq!(plan_order_restore(&saved, &current), vec![1, 0]);
    }

    #[test]
    fn plan_order_restore_appends_new_windows_in_current_order() {
        let saved = vec![identity("B", "b")];
        let current = vec![
            (0, identity("N1", "n1")),
            (1, identity("B", "b")),
            (2, identity("N2", "n2")),
        ];
        assert_eq!(plan_order_restore(&saved, &current), vec![1, 0, 2]);
    }

    #[test]
    fn plan_order_restore_empty_saved_list_keeps_current_order() {
        let current = vec![(0, identity("A", "a")), (1, identity("B", "b"))];
        assert_eq!(plan_order_restore(&[], &current), vec![0, 1]);
    }

    #[test]
    fn plan_order_restore_duplicate_identities_match_by_occurrence() {
        // Two terminals with the same identity: the k-th saved occurrence
        // consumes the k-th still-unused current match, so each instance
        // keeps its own relative slot.
        let saved = vec![
            identity("Term", "kitty"),
            identity("Firefox", "Navigator"),
            identity("Term", "kitty"),
        ];
        let current = vec![
            (0, identity("Term", "kitty")),
            (1, identity("Term", "kitty")),
            (2, identity("Firefox", "Navigator")),
        ];
        assert_eq!(plan_order_restore(&saved, &current), vec![0, 2, 1]);

        // More saved occurrences than current windows: the excess is dropped.
        let saved = vec![identity("Term", "kitty"), identity("Term", "kitty")];
        let current = vec![(0, identity("Term", "kitty"))];
        assert_eq!(plan_order_restore(&saved, &current), vec![0]);
    }

    #[test]
    fn plan_order_restore_matches_case_insensitively() {
        let saved = vec![identity("firefox", "navigator")];
        let current = vec![
            (0, identity("Term", "kitty")),
            (1, identity("Firefox", "Navigator")),
        ];
        assert_eq!(plan_order_restore(&saved, &current), vec![1, 0]);
    }

    /// A JWM on one output with a hidden "term" scratchpad and an ordinary
    /// terminal of the same class on tag 4, returning (scratchpad, terminal).
    fn jwm_with_a_hidden_scratchpad(
        backend: &mut DisplaySpyBackend,
    ) -> (Jwm, ClientKey, ClientKey) {
        let mut jwm = Jwm::new_with_runtime_backend(backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let mut keys = Vec::new();
        for (raw, tags) in [(0x61, 0), (0x62, 1 << 3)] {
            let mut client = WMClient::new(WindowId::from_raw(raw));
            client.class = "Alacritty".into();
            client.instance = "Alacritty".into();
            client.mon = Some(monitor);
            client.state.tags = tags;
            client.state.is_floating = tags == 0;
            client.geometry.x = 400;
            client.geometry.y = 200;
            client.geometry.w = 900;
            client.geometry.h = 500;
            let key = jwm.insert_client(client);
            jwm.attach_to_monitor(key, monitor);
            keys.push(key);
        }
        jwm.scratchpads.insert("term".into(), keys[0]);
        (jwm, keys[0], keys[1])
    }

    #[test]
    fn a_saved_session_leaves_scratchpads_out() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let (jwm, _scratchpad, _terminal) = jwm_with_a_hidden_scratchpad(&mut backend);
        let scratchpads: HashSet<ClientKey> = jwm.scratchpads.values().copied().collect();

        let snapshot = capture_snapshot_excluding(&jwm.state, "status-bar", &scratchpads);

        assert_eq!(snapshot.clients.len(), 1);
        assert_eq!(snapshot.clients[0].tags, 1 << 3);
        assert_eq!(
            snapshot.monitor_orders[0].clients,
            vec![identity("Alacritty", "Alacritty")]
        );
        // Without the scratchpad set, a window parked on no tag is still a
        // hidden scratchpad and is not saved either.
        assert_eq!(capture_snapshot(&jwm.state, "status-bar").clients.len(), 1);
    }

    /// A snapshot an older JWM wrote holds the hidden scratchpad first. The
    /// restore neither reveals the scratchpad nor hands its entry to the
    /// ordinary terminal, which keeps its own tag.
    #[test]
    fn restoring_a_session_never_reveals_a_hidden_scratchpad() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let (mut jwm, scratchpad, terminal) = jwm_with_a_hidden_scratchpad(&mut backend);
        let mut parked = entry("Alacritty", "Alacritty", 0);
        parked.is_floating = true;
        parked.floating = Some((400, 200, 900, 500));
        let snapshot = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![parked, entry("Alacritty", "Alacritty", 1 << 3)],
            monitor_orders: vec![SessionMonitorOrder {
                monitor_num: 0,
                connector: None,
                clients: vec![
                    identity("Alacritty", "Alacritty"),
                    identity("Alacritty", "Alacritty"),
                ],
            }],
        };

        let matched = jwm.apply_session_snapshot(&mut backend, &snapshot);

        assert_eq!(matched, 1);
        assert_eq!(jwm.state.clients[scratchpad].state.tags, 0);
        assert!(!jwm.is_client_visible_by_key(scratchpad));
        assert_eq!(jwm.state.clients[terminal].state.tags, 1 << 3);
        assert!(!jwm.state.clients[terminal].state.is_floating);
    }

    #[test]
    fn attach_session_connectors_records_live_output_keys() {
        use crate::backend::api::OutputIdentity;

        let mut left = output(1, 0, 0, 1920, 1080);
        left.identity = OutputIdentity::connector_only("eDP-1");
        left.name = "eDP-1".into();
        let mut right = output(2, 1920, 0, 1920, 1080);
        right.identity = OutputIdentity::connector_only("HDMI-A-1");
        right.name = "HDMI-A-1".into();

        let mut backend = DisplaySpyBackend::new(vec![left, right]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let mon1 = jwm.state.monitor_order[1];
        let tags = jwm.state.monitors[mon1].get_active_tags();

        let mut client = WMClient::new(WindowId::from_raw(0x42));
        client.class = "Firefox".into();
        client.instance = "Navigator".into();
        client.mon = Some(mon1);
        client.state.tags = tags;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, mon1);

        let mut snapshot = capture_snapshot(&jwm.state, "status-bar");
        assert!(snapshot.clients[0].connector.is_none());
        jwm.attach_session_connectors(&backend, &mut snapshot);
        assert_eq!(snapshot.clients[0].monitor_num, 1);
        assert_eq!(snapshot.clients[0].connector.as_deref(), Some("HDMI-A-1"));
        let order = snapshot
            .monitor_orders
            .iter()
            .find(|order| order.monitor_num == 1)
            .expect("monitor 1 order");
        assert_eq!(order.connector.as_deref(), Some("HDMI-A-1"));
    }

    #[test]
    fn session_restores_to_connector_after_monitor_renumber() {
        use crate::backend::api::OutputIdentity;

        let mut left = output(1, 0, 0, 1920, 1080);
        left.identity = OutputIdentity::connector_only("eDP-1");
        left.name = "eDP-1".into();
        let mut right = output(2, 1920, 0, 1920, 1080);
        right.identity = OutputIdentity::connector_only("HDMI-A-1");
        right.name = "HDMI-A-1".into();

        let mut backend = DisplaySpyBackend::new(vec![left, right]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let mon_edp = jwm.state.monitor_order[0];
        let mon_hdmi = jwm.state.monitor_order[1];
        assert_eq!(jwm.state.monitors[mon_edp].num, 0);
        assert_eq!(jwm.state.monitors[mon_hdmi].num, 1);

        let tags = jwm.state.monitors[mon_hdmi].get_active_tags();
        let mut client = WMClient::new(WindowId::from_raw(0x71));
        client.class = "Firefox".into();
        client.instance = "Navigator".into();
        client.mon = Some(mon_hdmi);
        client.state.tags = tags;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, mon_hdmi);

        let mut snapshot = capture_snapshot(&jwm.state, "status-bar");
        jwm.attach_session_connectors(&backend, &mut snapshot);
        assert_eq!(snapshot.clients[0].monitor_num, 1);
        assert_eq!(snapshot.clients[0].connector.as_deref(), Some("HDMI-A-1"));

        // Hole-fill renumber: HDMI-A-1 was monitor 1, now monitor 0.
        jwm.state.monitors[mon_edp].num = 1;
        jwm.state.monitors[mon_hdmi].num = 0;
        // Park the window on eDP so restore has to move it by connector.
        jwm.state.clients[key].mon = Some(mon_edp);
        jwm.attach_to_monitor(key, mon_edp);

        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snapshot), 1);
        assert_eq!(
            jwm.state.clients[key].mon,
            Some(mon_hdmi),
            "connector finds the renumbered HDMI output despite stale monitor_num"
        );
    }

    #[test]
    fn session_without_connector_keeps_saved_monitor_num() {
        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
        ]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let mon0 = jwm.state.monitor_order[0];
        let mon1 = jwm.state.monitor_order[1];
        let tags = jwm.state.monitors[mon1].get_active_tags();

        let mut client = WMClient::new(WindowId::from_raw(0x72));
        client.class = "Term".into();
        client.instance = "kitty".into();
        client.mon = Some(mon0);
        client.state.tags = tags;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, mon0);

        let mut parked = entry("Term", "kitty", tags);
        parked.monitor_num = 1;
        parked.connector = None;
        let snapshot = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![parked],
            monitor_orders: vec![SessionMonitorOrder {
                monitor_num: 1,
                connector: None,
                clients: vec![identity("Term", "kitty")],
            }],
        };

        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snapshot), 1);
        assert_eq!(
            jwm.state.clients[key].mon,
            Some(mon1),
            "pre-v5 entries without a connector keep monitor_num"
        );
    }

    #[test]
    fn session_maximize_restore_hint_is_clamped_to_destination_work_area() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let tags = jwm.state.monitors[monitor].get_active_tags();
        let work = jwm.monitor_work_area(monitor).expect("work area");

        let mut client = WMClient::new(WindowId::from_raw(0x73));
        client.class = "FloatApp".into();
        client.instance = "floatapp".into();
        client.mon = Some(monitor);
        client.state.tags = tags;
        client.state.is_floating = true;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);

        // Absolute coords from a previous geometry (e.g. another monitor's
        // origin) that sit entirely off this work area.
        let off_screen = (work.x + work.w + 500, work.y + work.h + 200, 400, 300);
        let mut entry = entry("FloatApp", "floatapp", tags);
        entry.is_floating = true;
        entry.floating = Some(off_screen);
        entry.maximize = Some(SessionMaximize {
            vert: true,
            horz: true,
            restore: Some(off_screen),
            promoted: false,
        });
        let snapshot = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![entry],
            monitor_orders: vec![],
        };

        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snapshot), 1);
        assert_eq!(
            jwm.state.clients[key].state.maximized_axes(),
            MaximizeAxes::BOTH
        );
        let hint = jwm.state.clients[key]
            .geometry
            .maximize_restore_rect
            .expect("restore hint after maximize");
        assert!(
            hint.x >= work.x
                && hint.y >= work.y
                && hint.x + hint.w <= work.x + work.w
                && hint.y + hint.h <= work.y + work.h,
            "restore hint {hint:?} must sit inside work area {work:?}"
        );
    }

    #[test]
    fn session_captures_and_restores_sticky() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let tags = jwm.state.monitors[monitor].get_active_tags();

        let mut client = WMClient::new(WindowId::from_raw(0x74));
        client.class = "StickyApp".into();
        client.instance = "stickyapp".into();
        client.mon = Some(monitor);
        client.state.tags = tags;
        client.state.is_sticky = true;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);

        let snapshot = capture_snapshot(&jwm.state, "status-bar");
        assert!(snapshot.clients[0].is_sticky, "capture must record sticky");

        jwm.state.clients[key].state.is_sticky = false;
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snapshot), 1);
        assert!(
            jwm.state.clients[key].state.is_sticky,
            "restore must re-apply sticky through set_client_sticky"
        );
    }

    #[test]
    fn session_captures_and_restores_above_below() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let tags = jwm.state.monitors[monitor].get_active_tags();

        let mut client = WMClient::new(WindowId::from_raw(0x75));
        client.class = "AboveApp".into();
        client.instance = "aboveapp".into();
        client.mon = Some(monitor);
        client.state.tags = tags;
        client.state.is_above = true;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);

        let snapshot = capture_snapshot(&jwm.state, "status-bar");
        assert!(
            snapshot.clients[0].is_above && !snapshot.clients[0].is_below,
            "capture must record Above"
        );

        jwm.state.clients[key].state.is_above = false;
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snapshot), 1);
        assert!(
            jwm.state.clients[key].state.is_above && !jwm.state.clients[key].state.is_below,
            "restore must re-apply Above through apply_external_stacking_request"
        );

        // Below path.
        jwm.state.clients[key].state.is_above = false;
        jwm.state.clients[key].state.is_below = true;
        let below_snap = capture_snapshot(&jwm.state, "status-bar");
        assert!(
            !below_snap.clients[0].is_above && below_snap.clients[0].is_below,
            "capture must record Below"
        );
        jwm.state.clients[key].state.is_below = false;
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &below_snap), 1);
        assert!(
            !jwm.state.clients[key].state.is_above && jwm.state.clients[key].state.is_below,
            "restore must re-apply Below"
        );

        // Above wins when a snapshot somehow recorded both.
        let mut both = below_snap;
        both.clients[0].is_above = true;
        both.clients[0].is_below = true;
        jwm.state.clients[key].state.is_above = false;
        jwm.state.clients[key].state.is_below = false;
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &both), 1);
        assert!(
            jwm.state.clients[key].state.is_above && !jwm.state.clients[key].state.is_below,
            "Above must win when both flags are saved"
        );
    }

    #[test]
    fn session_captures_and_restores_minimized() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let tags = jwm.state.monitors[monitor].get_active_tags();

        let mut client = WMClient::new(WindowId::from_raw(0x76));
        client.class = "MinApp".into();
        client.instance = "minapp".into();
        client.mon = Some(monitor);
        client.state.tags = tags;
        client.geometry.x = 100;
        client.geometry.y = 80;
        client.geometry.w = 640;
        client.geometry.h = 480;
        client.geometry.border_w = 2;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);

        jwm.set_client_minimized(&mut backend, key, true)
            .expect("minimize");
        assert!(jwm.state.clients[key].state.is_hidden);

        let mut snapshot = capture_snapshot(&jwm.state, "status-bar");
        assert!(
            snapshot.clients[0].is_minimized,
            "capture must record minimized"
        );

        // A snapshot can come from a different output topology.  `hidden_x`
        // is a derived parking coordinate, so this stale value must not
        // overwrite the coordinate at which restore actually parks the live
        // window.
        snapshot.clients[0].hidden_x = Some(-123_456);
        snapshot.clients[0].hidden_restore = Some((9_000, 8_000, 640, 480));

        jwm.set_client_minimized(&mut backend, key, false)
            .expect("unminimize");
        assert!(!jwm.state.clients[key].state.is_hidden);
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snapshot), 1);
        assert!(
            jwm.state.clients[key].state.is_hidden,
            "restore must re-apply minimized through set_client_minimized"
        );
        let restored = &jwm.state.clients[key].geometry;
        assert_eq!(restored.hidden_x, Some(restored.x));
        assert_ne!(restored.hidden_x, snapshot.clients[0].hidden_x);
        let hidden_restore = restored
            .hidden_restore_rect
            .expect("minimized restore geometry");
        let work = jwm.monitor_work_area(monitor).expect("monitor work area");
        assert!(hidden_restore.x >= work.x && hidden_restore.y >= work.y);
        assert!(hidden_restore.x + hidden_restore.w <= work.x + work.w);
        assert!(hidden_restore.y + hidden_restore.h <= work.y + work.h);
    }

    #[test]
    fn session_minimized_fullscreen_keeps_the_complete_output_restore_rect() {
        let output_rect = Rect::new(0, 0, 1920, 1080);
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let tags = jwm.state.monitors[monitor].get_active_tags();

        // A real dock reservation distinguishes the output from its work
        // area. The ordinary fullscreen-only fixture has no bar and cannot
        // expose a work-area clamp mistakenly applied to fullscreen.
        let mut dock = WMClient::new(WindowId::from_raw(0x780));
        dock.mon = Some(monitor);
        dock.state.tags = tags;
        dock.state.is_dock = true;
        dock.state.is_floating = true;
        dock.geometry.x = 0;
        dock.geometry.y = 0;
        dock.geometry.w = 1920;
        dock.geometry.h = 32;
        let dock_key = jwm.insert_client(dock);
        jwm.attach_to_monitor(dock_key, monitor);

        let mut client = WMClient::new(WindowId::from_raw(0x781));
        client.class = "FullscreenSession".into();
        client.instance = "fullscreen-session".into();
        client.mon = Some(monitor);
        client.state.tags = tags;
        client.geometry.x = 100;
        client.geometry.y = 80;
        client.geometry.w = 640;
        client.geometry.h = 480;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);
        jwm.setfullscreen(&mut backend, key, true).unwrap();
        jwm.set_client_minimized(&mut backend, key, true).unwrap();
        assert!(jwm.monitor_work_area(monitor).unwrap().y > output_rect.y);
        let snapshot = capture_snapshot(&jwm.state, "status-bar");
        assert_eq!(snapshot.clients.len(), 1);
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &snapshot), 1);
        assert!(jwm.state.clients[key].state.is_fullscreen);
        assert!(jwm.state.clients[key].state.is_hidden);
        assert_eq!(
            jwm.state.clients[key].geometry.hidden_restore_rect,
            Some(output_rect),
            "session restore must not shrink fullscreen into the bar's work area"
        );

        jwm.set_client_minimized(&mut backend, key, false).unwrap();
        let geometry = &jwm.state.clients[key].geometry;
        assert_eq!(
            Rect::new(geometry.x, geometry.y, geometry.w, geometry.h),
            output_rect
        );
        assert!(jwm.state.clients[key].state.is_fullscreen);
    }

    #[test]
    fn session_captures_and_restores_fullscreen_and_pip() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let monitor = jwm.state.monitor_order[0];
        let tags = jwm.state.monitors[monitor].get_active_tags();

        let mut fs_client = WMClient::new(WindowId::from_raw(0x77));
        fs_client.class = "FsApp".into();
        fs_client.instance = "fsapp".into();
        fs_client.mon = Some(monitor);
        fs_client.state.tags = tags;
        fs_client.geometry.x = 100;
        fs_client.geometry.y = 80;
        fs_client.geometry.w = 640;
        fs_client.geometry.h = 480;
        fs_client.geometry.border_w = 2;
        let fs_key = jwm.insert_client(fs_client);
        jwm.attach_to_monitor(fs_key, monitor);

        jwm.setfullscreen(&mut backend, fs_key, true)
            .expect("enter fullscreen");
        assert!(jwm.state.clients[fs_key].state.is_fullscreen);

        let mut fs_snap = capture_snapshot(&jwm.state, "status-bar");
        assert!(
            fs_snap
                .clients
                .iter()
                .any(|e| e.class == "FsApp" && e.is_fullscreen && !e.is_pip),
            "capture must record fullscreen"
        );
        fs_snap
            .clients
            .iter_mut()
            .find(|entry| entry.class == "FsApp")
            .expect("FsApp entry")
            .old_geometry = Some((9_000, 8_000, 640, 480));

        jwm.setfullscreen(&mut backend, fs_key, false)
            .expect("leave fullscreen");
        assert!(!jwm.state.clients[fs_key].state.is_fullscreen);
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &fs_snap), 1);
        assert!(
            jwm.state.clients[fs_key].state.is_fullscreen
                && !jwm.state.clients[fs_key].state.is_pip,
            "restore must re-apply fullscreen through setfullscreen"
        );

        jwm.setfullscreen(&mut backend, fs_key, false)
            .expect("leave before pip");
        let restored = &jwm.state.clients[fs_key].geometry;
        let work = jwm.monitor_work_area(monitor).expect("monitor work area");
        assert!(restored.x >= work.x && restored.y >= work.y);
        assert!(restored.x + restored.w <= work.x + work.w);
        assert!(restored.y + restored.h <= work.y + work.h);
        jwm.set_client_pip(&mut backend, fs_key, true)
            .expect("enter pip");
        assert!(jwm.state.clients[fs_key].state.is_pip);

        let pip_snap = capture_snapshot(&jwm.state, "status-bar");
        assert!(
            pip_snap
                .clients
                .iter()
                .any(|e| e.class == "FsApp" && e.is_pip && !e.is_fullscreen),
            "capture must record PiP"
        );

        jwm.set_client_pip(&mut backend, fs_key, false)
            .expect("leave pip");
        assert!(!jwm.state.clients[fs_key].state.is_pip);
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &pip_snap), 1);
        assert!(
            jwm.state.clients[fs_key].state.is_pip
                && !jwm.state.clients[fs_key].state.is_fullscreen,
            "restore must re-apply PiP through set_client_pip"
        );

        // Fullscreen wins when a snapshot somehow recorded both.
        let mut both = pip_snap;
        let entry = both
            .clients
            .iter_mut()
            .find(|e| e.class == "FsApp")
            .expect("FsApp entry");
        entry.is_fullscreen = true;
        entry.is_pip = true;
        jwm.set_client_pip(&mut backend, fs_key, false)
            .expect("clear before both");
        assert_eq!(jwm.apply_session_snapshot(&mut backend, &both), 1);
        assert!(
            jwm.state.clients[fs_key].state.is_fullscreen
                && !jwm.state.clients[fs_key].state.is_pip,
            "Fullscreen must win when both flags are saved"
        );
    }
}
