//! One-exec X11 workspace handoff. This is transient state, not authentication:
//! other clients of the same X server can read and change window properties.
//! A per-window generation prevents accidental XID reuse; the explicit startup
//! intent prevents cold starts from consuming an abandoned root snapshot.

use crate::Jwm;
use crate::backend::api::{
    Backend, MAX_RESTART_PROPERTY_BYTES, RestartProperty, WindowHandoffIdentity,
};
use crate::backend::common_define::WindowId;
use crate::config::{CONFIG, MAX_N_MASTER};
use crate::core::layout::LayoutEnum;
use crate::core::master_resize::{ResizeContext, ResizeIntent};
use crate::core::models::{ClientKey, MonitorKey, ScrollingState};
use crate::core::types::Rect;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const RESTART_STATE_ENV: &str = "JWM_RESTART_STATE_V1";
const VERSION: u32 = 1;
const MAX_MONITORS: usize = 64;
const MAX_CLIENTS: usize = 4096;
const MAX_SHAPE_CONTEXTS: usize = 4096;
const MAX_AGE_MS: u64 = 300_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestartIntent {
    version: u32,
    pid: u32,
    captured_ms: u64,
    generation: String,
}

impl RestartIntent {
    pub(crate) fn encode(&self) -> String {
        // The only string is generated fixed-width ASCII hex; this bounded
        // encoding cannot introduce a fallible step after client preflight.
        format!(
            r#"{{"version":{},"pid":{},"captured_ms":{},"generation":"{}"}}"#,
            self.version, self.pid, self.captured_ms, self.generation
        )
    }

    pub(crate) fn decode(value: &str, pid: u32, now_ms: u64) -> Option<Self> {
        if value.len() > 256 {
            return None;
        }
        let value: Self = serde_json::from_str(value).ok()?;
        (value.version == VERSION
            && value.pid == pid
            && pid != 0
            && value.generation.len() == 32
            && value
                .generation
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            && now_ms
                .checked_sub(value.captured_ms)
                .is_some_and(|age| age <= MAX_AGE_MS))
        .then_some(value)
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    stable_key: String,
    connector: String,
    rect: [i32; 4],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tag {
    layout: String,
    alt: String,
    n_master: u32,
    m_fact: f32,
    gap: i32,
    show_bar: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Monitor {
    output: Output,
    tag_set: [u32; 2],
    sel_tags: usize,
    cur_tag: usize,
    prev_tag: usize,
    tags: Vec<Tag>,
    order: Vec<u32>,
    selected: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mru: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tag_selected: Vec<Option<u32>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    shapes: Vec<ShapeContext>,
}

/// Only normalized, committed layout intent crosses the exec boundary. A
/// scrolling grouping and its resize anchor share one optional eviction unit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShapeContext {
    tag_mask: u32,
    layout: String,
    resize: Option<ResizeAnchor>,
    scrolling: Option<ScrollingShape>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResizeAnchor {
    xid: u32,
    width: f64,
    height: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScrollingShape {
    columns: Vec<ScrollingColumn>,
    focused_column: Option<usize>,
    attach_new: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScrollingColumn {
    clients: Vec<u32>,
    width_factor: f32,
    focused: Option<u32>,
}

fn canonical_shape_layout(name: &str) -> Option<&'static LayoutEnum> {
    // from_name accepts spelling aliases (case/whitespace); wire scopes must
    // be exact. Monocle is a valid resize scope despite not being is_tile().
    LayoutEnum::from_name(name).filter(|layout| layout.0 == name && !layout.is_float())
}

fn exact_shape_tags(saved: u32, tagmask: u32) -> bool {
    saved != 0 && saved & tagmask == saved
}

impl ShapeContext {
    fn validate(&self, monitor_ids: &HashSet<u32>) -> bool {
        let mut members = HashSet::new();
        self.tag_mask != 0
            && canonical_shape_layout(&self.layout).is_some()
            && (self.resize.is_some() || self.scrolling.is_some())
            && self.resize.as_ref().is_none_or(|anchor| {
                monitor_ids.contains(&anchor.xid)
                    && anchor.width.is_finite()
                    && (0.0..=1.0).contains(&anchor.width)
                    && anchor.height.is_finite()
                    && (0.0..=1.0).contains(&anchor.height)
            })
            && self.scrolling.as_ref().is_none_or(|scrolling| {
                self.layout == LayoutEnum::SCROLLING.0
                    && scrolling.columns.len() <= MAX_CLIENTS
                    && scrolling
                        .focused_column
                        .is_none_or(|index| index < scrolling.columns.len())
                    && scrolling.columns.iter().all(|column| {
                        !column.clients.is_empty()
                            && column.width_factor.is_finite()
                            && (0.25..=2.5).contains(&column.width_factor)
                            && column
                                .clients
                                .iter()
                                .all(|id| monitor_ids.contains(id) && members.insert(*id))
                            && column.focused.is_none_or(|id| column.clients.contains(&id))
                    })
            })
    }
}

fn default_client_fact() -> f32 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Client {
    xid: u32,
    tags: u32,
    monitor: usize,
    #[serde(default = "default_client_fact")]
    client_fact: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    maximize_anchor: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestartSnapshot {
    intent: RestartIntent,
    monitors: Vec<Monitor>,
    clients: Vec<Client>,
    selected_monitor: Option<usize>,
}

impl RestartSnapshot {
    fn validate(&self) -> bool {
        let mut ids = HashSet::new();
        self.intent.version == VERSION
            && self.monitors.len() <= MAX_MONITORS
            && self.clients.len() <= MAX_CLIENTS
            && self
                .monitors
                .iter()
                .map(|monitor| monitor.shapes.len())
                .sum::<usize>()
                <= MAX_SHAPE_CONTEXTS
            && self
                .selected_monitor
                .is_none_or(|index| index < self.monitors.len())
            && self.clients.iter().all(|client| {
                client.xid != 0
                    && ids.insert(client.xid)
                    && client.client_fact.is_finite()
                    && (0.25..=4.0).contains(&client.client_fact)
                    && client.tags != 0
                    && client.monitor < self.monitors.len()
                    && client.maximize_anchor.is_none_or(|anchor| {
                        anchor != client.xid
                            && self
                                .clients
                                .iter()
                                .any(|other| other.xid == anchor && other.monitor == client.monitor)
                    })
            })
            && self
                .monitors
                .iter()
                .enumerate()
                .all(|(monitor_index, monitor)| {
                    let mut ordered = HashSet::new();
                    let mut stacked = HashSet::new();
                    let mut scopes = HashSet::new();
                    let monitor_ids: HashSet<_> = self
                        .clients
                        .iter()
                        .filter(|client| client.monitor == monitor_index)
                        .map(|client| client.xid)
                        .collect();
                    monitor.output.stable_key.len() <= 1024
                        && monitor.output.connector.len() <= 1024
                        && monitor.output.rect[2] > 0
                        && monitor.output.rect[3] > 0
                        && monitor.sel_tags < 2
                        && monitor.tag_set.iter().all(|mask| *mask != 0)
                        && (2..=33).contains(&monitor.tags.len())
                        && monitor.cur_tag < monitor.tags.len()
                        && monitor.prev_tag < monitor.tags.len()
                        && monitor.order.len() <= MAX_CLIENTS
                        && monitor.order.iter().all(|id| {
                            ids.contains(id)
                                && ordered.insert(*id)
                                && self.clients.iter().any(|client| {
                                    client.xid == *id && client.monitor == monitor_index
                                })
                        })
                        && self
                            .clients
                            .iter()
                            .filter(|client| client.monitor == monitor_index)
                            .all(|client| monitor.order.contains(&client.xid))
                        && monitor
                            .selected
                            .is_none_or(|id| monitor.order.contains(&id))
                        && monitor.mru.len() <= MAX_CLIENTS
                        && monitor
                            .mru
                            .iter()
                            .all(|id| monitor_ids.contains(id) && stacked.insert(*id))
                        && (monitor.tag_selected.is_empty()
                            || monitor.tag_selected.len() == monitor.tags.len())
                        && monitor
                            .tag_selected
                            .iter()
                            .flatten()
                            .all(|id| monitor_ids.contains(id))
                        && monitor.shapes.iter().all(|shape| {
                            scopes.insert((shape.tag_mask, shape.layout.as_str()))
                                && shape.validate(&monitor_ids)
                        })
                        && monitor.tags.iter().all(|tag| {
                            LayoutEnum::from_name(&tag.layout).is_some()
                                && LayoutEnum::from_name(&tag.alt).is_some()
                                && tag.n_master <= MAX_N_MASTER
                                && tag.m_fact.is_finite()
                                && (0.05..=0.95).contains(&tag.m_fact)
                                && (0..=100).contains(&tag.gap)
                        })
                })
    }

    fn encode(&self) -> Result<Vec<u8>, Box<dyn Error>> {
        if !self.validate() {
            return Err("invalid X11 restart snapshot".into());
        }
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_RESTART_PROPERTY_BYTES {
            return Err("X11 restart snapshot exceeds property limit".into());
        }
        Ok(bytes)
    }

    /// Candidates arrive in descending importance. Validate the base before
    /// touching optional state, then discard only a suffix of whole contexts.
    /// Exact JSON costs avoid repeatedly serializing an oversized snapshot.
    fn encode_with_shapes(
        &mut self,
        candidates: Vec<(usize, ShapeContext)>,
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        let base = self.encode()?;
        let mut size = base.len();
        let monitor_ids: Vec<HashSet<_>> = (0..self.monitors.len())
            .map(|index| {
                self.clients
                    .iter()
                    .filter(|client| client.monitor == index)
                    .map(|client| client.xid)
                    .collect()
            })
            .collect();
        let mut scopes = HashSet::new();
        let mut retained = Vec::new();
        let mut invalid = 0;
        let mut omitted = 0;
        for (index, shape) in candidates {
            if !monitor_ids
                .get(index)
                .is_some_and(|ids| shape.validate(ids))
                || !scopes.insert((index, shape.tag_mask, shape.layout.clone()))
            {
                invalid += 1;
                continue;
            }
            if retained.len() == MAX_SHAPE_CONTEXTS {
                omitted += 1;
                continue;
            }
            let shapes = &mut self.monitors[index].shapes;
            // The base omits an empty shapes field. Later entries add a comma.
            let cost = serde_json::to_vec(&shape)?.len()
                + if shapes.is_empty() {
                    b",\"shapes\":[]".len()
                } else {
                    1
                };
            size += cost;
            shapes.push(shape);
            retained.push((index, cost));
        }
        while size > MAX_RESTART_PROPERTY_BYTES {
            let (index, cost) = retained.pop().expect("base encoding already fits");
            self.monitors[index].shapes.pop();
            size -= cost;
            omitted += 1;
        }
        if invalid != 0 {
            log::warn!("[restart] skipped {invalid} invalid optional shape contexts");
        }
        if omitted != 0 {
            log::warn!(
                "[restart] omitted {omitted} lowest-priority optional shape contexts to fit snapshot bounds"
            );
        }
        if retained.is_empty() {
            return Ok(base);
        }
        let encoded = self.encode()?;
        debug_assert_eq!(encoded.len(), size);
        Ok(encoded)
    }

    fn decode(bytes: &[u8], intent: &RestartIntent) -> Option<Self> {
        if bytes.len() > MAX_RESTART_PROPERTY_BYTES {
            return None;
        }
        let snapshot: Self = serde_json::from_slice(bytes).ok()?;
        (snapshot.intent == *intent && snapshot.validate()).then_some(snapshot)
    }
}

/// A unique panel identity wins over enumeration order. An ambiguous identity
/// never picks the first duplicate. Only a complete, unchanged legacy topology
/// can recover unnamed XCB outputs by geometry; named outputs never use it.
fn exact_output(saved: &Output, live: &[Output], sources: &[Output]) -> Option<usize> {
    if !saved.stable_key.is_empty() || !saved.connector.is_empty() {
        return named_output(saved, live, sources);
    }
    // Older XCB processes wrote two empty identity strings. Upgrade those
    // snapshots only when the *whole* output geometry is unchanged and has
    // a unique one-to-one match. Partial or mirrored topologies and changed
    // rectangle sets provide no identity evidence and must not claim a panel.
    if live.len() != sources.len() || !sources.contains(saved) {
        return None;
    }
    let mut claimed = HashSet::new();
    for source in sources {
        let mut candidates = live
            .iter()
            .enumerate()
            .filter(|(_, output)| output.rect == source.rect)
            .map(|(index, _)| index);
        let index = candidates.next()?;
        if candidates.next().is_some() || !claimed.insert(index) {
            return None;
        }
        // Named panels keep identity precedence even in a mixed legacy
        // snapshot. Their movement/removal cannot make an anonymous source
        // inherit a known panel's old or new slot.
        if (!source.stable_key.is_empty() || !source.connector.is_empty())
            && named_output(source, live, sources) != Some(index)
        {
            return None;
        }
    }
    live.iter().position(|output| output.rect == saved.rect)
}

fn named_output(saved: &Output, live: &[Output], sources: &[Output]) -> Option<usize> {
    let stable: Vec<_> = live
        .iter()
        .enumerate()
        .filter(|(_, output)| !saved.stable_key.is_empty() && output.stable_key == saved.stable_key)
        .map(|(index, _)| index)
        .collect();
    if stable.len() == 1
        && sources
            .iter()
            .filter(|source| source.stable_key == saved.stable_key)
            .count()
            == 1
    {
        return stable.first().copied();
    }
    let connectors: Vec<_> = live
        .iter()
        .enumerate()
        .filter(|(index, output)| {
            !saved.connector.is_empty()
                && output.connector == saved.connector
                && (stable.is_empty() || stable.contains(index))
                // Never let a stale connector steal a live panel whose
                // stable identity belongs to another saved source. This
                // reserves identity matches before connector fallback,
                // independently of the order saved monitors are visited.
                && (output.stable_key.is_empty() || output.stable_key == saved.stable_key
                    || !sources.iter().any(|source| source.stable_key == output.stable_key))
        })
        .map(|(index, _)| index)
        .collect();
    if connectors.len() == 1
        && sources
            .iter()
            .filter(|source| source.connector == saved.connector)
            .count()
            == 1
    {
        connectors.first().copied()
    } else {
        None
    }
}

fn fallback_output(saved: &Output, live: &[Output]) -> Option<usize> {
    live.iter()
        .enumerate()
        .min_by_key(|(index, output)| {
            let center = |rect: [i32; 4]| {
                (
                    i64::from(rect[0]) * 2 + i64::from(rect[2]),
                    i64::from(rect[1]) * 2 + i64::from(rect[3]),
                )
            };
            let (sx, sy) = center(saved.rect);
            let (lx, ly) = center(output.rect);
            (
                i128::from(sx - lx).pow(2) + i128::from(sy - ly).pow(2),
                *index,
            )
        })
        .map(|(index, _)| index)
}

fn legal_tags(saved: u32, tagmask: u32, fallback: u32) -> u32 {
    let saved = saved & tagmask;
    if saved != 0 {
        saved
    } else {
        let fallback = fallback & tagmask;
        if fallback != 0 {
            fallback
        } else {
            tagmask & tagmask.wrapping_neg()
        }
    }
}

fn restored_tag_slot(saved: usize, active: u32, tagmask: u32, tags_length: usize) -> usize {
    if tags_length > 1 && active == tagmask {
        0
    } else if saved > 0 && saved <= tags_length && active & (1 << (saved - 1)) != 0 {
        saved
    } else {
        (active.trailing_zeros() as usize + 1).min(tags_length)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestartWriteStep {
    WriteClient(u32),
    VerifyClient(u32),
    WriteRoot,
    VerifyRoot,
}

/// Root publication is last, and every write must be read back before the
/// next step. The caller owns generation-checked rollback on any error.
fn publish_restart_snapshot<E>(
    clients: &[u32],
    mut step: impl FnMut(RestartWriteStep) -> Result<(), E>,
) -> Result<(), E> {
    for &id in clients {
        step(RestartWriteStep::WriteClient(id))?;
        step(RestartWriteStep::VerifyClient(id))?;
    }
    step(RestartWriteStep::WriteRoot)?;
    step(RestartWriteStep::VerifyRoot)
}

#[derive(Default)]
pub(crate) struct PendingRestartState {
    snapshot: Option<RestartSnapshot>,
    adopted: HashSet<u32>,
    adopted_keys: HashMap<u32, ClientKey>,
    pub(crate) adopting: bool,
}

fn xid(backend: &dyn Backend, window: WindowId) -> Option<u32> {
    match backend.window_handoff_identity(window)? {
        WindowHandoffIdentity::X11(id) => Some(id),
    }
}

fn live_outputs(jwm: &Jwm, backend: &dyn Backend) -> Vec<(MonitorKey, Output)> {
    let outputs = backend.output_ops().enumerate_outputs();
    jwm.state
        .monitor_order
        .iter()
        .filter_map(|key| {
            let monitor = jwm.state.monitors.get(*key)?;
            let output_id = jwm.state.output_map.get(*key)?;
            let output = outputs.iter().find(|output| output.id == *output_id)?;
            Some((
                *key,
                Output {
                    stable_key: output.identity.stable_key.clone(),
                    connector: output.identity.connector.clone(),
                    rect: [
                        monitor.geometry.m_x,
                        monitor.geometry.m_y,
                        monitor.geometry.m_w,
                        monitor.geometry.m_h,
                    ],
                },
            ))
        })
        .collect()
}

impl Jwm {
    /// Capture only native X11 clients; special shell/scratchpad ownership stays
    /// in its existing handoff. Minimized, sticky and swallowed ordinary clients
    /// intentionally remain included and retain their complete tag bitmask.
    pub(crate) fn prepare_restart_state(
        &self,
        backend: &dyn Backend,
    ) -> Result<Option<RestartIntent>, Box<dyn Error>> {
        let Some(root) = backend
            .root_window()
            .filter(|root| xid(backend, *root).is_some())
        else {
            return Ok(None);
        };
        let generation: [u8; 16] = rand::random();
        let intent = RestartIntent {
            version: VERSION,
            pid: std::process::id(),
            captured_ms: now_ms(),
            generation: format!("{:032x}", u128::from_be_bytes(generation)),
        };
        let live = live_outputs(self, backend);
        let cfg = CONFIG.load();
        let special: HashSet<_> = self.scratchpads.values().copied().collect();
        if live.len() != self.state.monitor_order.len() {
            return Err("X11 restart output enumeration is incomplete".into());
        }
        let mut clients = Vec::new();
        for (key, client) in self.state.clients.iter() {
            if special.contains(&key)
                || client.is_status_bar(cfg.status_bar_name())
                || client.state.is_dock
            {
                continue;
            }
            let monitor = live
                .iter()
                .position(|(key, _)| Some(*key) == client.mon)
                .ok_or("X11 restart client has no live output")?;
            let client_id =
                xid(backend, client.win).ok_or("X11 restart client has no native identity")?;
            clients.push(Client {
                xid: client_id,
                tags: client.state.tags,
                monitor,
                client_fact: client.state.client_fact,
                maximize_anchor: client
                    .state
                    .maximize_restore_anchor
                    .filter(|anchor| {
                        *anchor != key
                            && client.state.maximize_restore_tiled
                            && client.state.maximized_axes().any()
                    })
                    .and_then(|anchor| self.state.clients.get(anchor))
                    .filter(|anchor| anchor.mon == client.mon)
                    .and_then(|anchor| xid(backend, anchor.win)),
            });
        }
        let captured: HashSet<_> = clients.iter().map(|client| client.xid).collect();
        for client in &mut clients {
            client.maximize_anchor = client.maximize_anchor.filter(|id| captured.contains(id));
        }
        let monitors: Vec<Monitor> = live
            .iter()
            .map(|(key, output)| {
                let monitor = &self.state.monitors[*key];
                let pertag = monitor
                    .pertag
                    .as_ref()
                    .ok_or("monitor has no per-tag state")?;
                let count = pertag.lts.len();
                if [
                    pertag.prev_lts.len(),
                    pertag.n_masters.len(),
                    pertag.m_facts.len(),
                    pertag.gaps.len(),
                    pertag.show_bars.len(),
                    pertag.sel.len(),
                ]
                .into_iter()
                .any(|len| len != count)
                {
                    return Err("inconsistent per-tag restart state".into());
                }
                let order = self
                    .state
                    .monitor_clients
                    .get(*key)
                    .into_iter()
                    .flatten()
                    .filter_map(|key| {
                        self.state
                            .clients
                            .get(*key)
                            .and_then(|client| xid(backend, client.win))
                    })
                    .filter(|id| captured.contains(id))
                    .collect::<Vec<_>>();
                let selected = monitor
                    .sel
                    .and_then(|key| self.state.clients.get(key))
                    .and_then(|client| xid(backend, client.win))
                    .filter(|id| order.contains(id));
                let saved_id = |client_key| {
                    self.state
                        .clients
                        .get(client_key)
                        .filter(|client| client.mon == Some(*key))
                        .and_then(|client| xid(backend, client.win))
                        .filter(|id| order.contains(id))
                };
                let mut stacked = HashSet::new();
                let mru = self
                    .state
                    .monitor_stack
                    .get(*key)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter_map(saved_id)
                    .filter(|id| stacked.insert(*id))
                    .collect();
                let tag_selected = pertag
                    .sel
                    .iter()
                    .map(|selected| selected.and_then(saved_id))
                    .collect();
                let mut tags = Vec::new();
                for index in 0..pertag.lts.len() {
                    tags.push(Tag {
                        layout: pertag.lts[index].0.to_owned(),
                        alt: pertag.prev_lts[index].0.to_owned(),
                        n_master: pertag.n_masters[index],
                        m_fact: pertag.m_facts[index],
                        gap: pertag.gaps[index],
                        show_bar: pertag.show_bars[index],
                    });
                }
                Ok(Monitor {
                    output: output.clone(),
                    tag_set: monitor.tag_set,
                    sel_tags: monitor.sel_tags,
                    cur_tag: pertag.cur_tag,
                    prev_tag: pertag.prev_tag,
                    tags,
                    order,
                    selected,
                    mru,
                    tag_selected,
                    shapes: Vec::new(),
                })
            })
            .collect::<Result<_, Box<dyn Error>>>()?;
        let mut snapshot = RestartSnapshot {
            intent: intent.clone(),
            monitors,
            clients,
            selected_monitor: live
                .iter()
                .position(|(key, _)| Some(*key) == self.state.sel_mon),
        };
        // A malformed/oversized base remains a hard failure. Optional capture
        // cannot make otherwise valid client/tag/layout handoff unavailable.
        snapshot.encode()?;
        let shapes = self.capture_restart_shapes(backend, &live, &snapshot);
        let encoded = snapshot.encode_with_shapes(shapes)?;
        let ids: Vec<_> = snapshot.clients.iter().map(|client| client.xid).collect();
        let result = publish_restart_snapshot(&ids, |step| -> Result<(), Box<dyn Error>> {
            match step {
                RestartWriteStep::WriteClient(id) | RestartWriteStep::VerifyClient(id) => {
                    let window = backend
                        .resolve_window_handoff_identity(WindowHandoffIdentity::X11(id))
                        .ok_or("restart client disappeared")?;
                    if matches!(step, RestartWriteStep::WriteClient(_)) {
                        backend.property_ops().set_restart_property(
                            window,
                            RestartProperty::ClientGeneration,
                            intent.generation.as_bytes(),
                        )?;
                    } else if backend
                        .property_ops()
                        .get_restart_property(window, RestartProperty::ClientGeneration)?
                        .as_deref()
                        != Some(intent.generation.as_bytes())
                    {
                        return Err("restart client generation readback mismatch".into());
                    }
                }
                RestartWriteStep::WriteRoot => {
                    backend.property_ops().set_restart_property(
                        root,
                        RestartProperty::Snapshot,
                        &encoded,
                    )?;
                }
                RestartWriteStep::VerifyRoot => {
                    if backend
                        .property_ops()
                        .get_restart_property(root, RestartProperty::Snapshot)?
                        .as_deref()
                        != Some(encoded.as_slice())
                    {
                        return Err("restart root snapshot readback mismatch".into());
                    }
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            self.clear_restart_state(backend, &intent);
            return Err(error);
        }
        Ok(Some(intent))
    }

    fn capture_restart_shapes(
        &self,
        backend: &dyn Backend,
        live: &[(MonitorKey, Output)],
        snapshot: &RestartSnapshot,
    ) -> Vec<(usize, ShapeContext)> {
        let saved_ids: HashMap<_, _> = snapshot
            .clients
            .iter()
            .map(|client| (client.xid, client))
            .collect();
        let captured: HashMap<_, _> = self
            .state
            .clients
            .iter()
            .filter_map(|(key, client)| {
                let id = xid(backend, client.win)?;
                Some((key, *saved_ids.get(&id)?))
            })
            .collect();
        let mut candidates = Vec::new();
        let tagmask = CONFIG.load().tagmask();
        let mut invalid_masks = 0;
        for (index, (monitor_key, _)) in live.iter().enumerate() {
            let monitor = &self.state.monitors[*monitor_key];
            let eligible = |key: ClientKey, mask: u32| -> Option<u32> {
                let client = self.state.clients.get(key)?;
                let saved = captured.get(&key)?;
                (saved.monitor == index
                    && client.mon == Some(*monitor_key)
                    && self
                        .state
                        .monitor_clients
                        .get(*monitor_key)
                        .is_some_and(|clients| clients.contains(&key))
                    && !client.state.is_floating
                    && !client.state.is_hidden
                    && !client.state.is_swallowed
                    && (client.state.is_sticky || saved.tags & mask != 0))
                    .then_some(saved.xid)
            };
            let mut contexts: HashMap<(u32, &str), ShapeContext> = HashMap::new();
            for (&(mask, layout), context) in &monitor.layout_resizes {
                // Some(None) is an uncommitted first drag: it must not fall
                // back to the in-progress intent.
                let Some(intent) = context.drag_snapshot.unwrap_or(context.intent) else {
                    continue;
                };
                let Some(id) = eligible(intent.client, mask) else {
                    continue;
                };
                contexts
                    .entry((mask, layout))
                    .or_insert_with(|| ShapeContext {
                        tag_mask: mask,
                        layout: layout.to_owned(),
                        resize: None,
                        scrolling: None,
                    })
                    .resize = Some(ResizeAnchor {
                    xid: id,
                    width: intent.width,
                    height: intent.height,
                });
            }
            for (&(key, mask), scrolling) in &self.scrolling_states {
                if key != *monitor_key {
                    continue;
                }
                let mut columns = Vec::new();
                let mut focused_column = None;
                for (old_index, column) in scrolling.columns.iter().enumerate() {
                    let clients: Vec<_> = column
                        .iter()
                        .filter_map(|key| eligible(*key, mask))
                        .collect();
                    if clients.is_empty() {
                        continue;
                    }
                    if scrolling.focused_column == Some(old_index) {
                        focused_column = Some(columns.len());
                    }
                    let focused = scrolling
                        .focused_clients
                        .get(old_index)
                        .copied()
                        .flatten()
                        .and_then(|key| eligible(key, mask))
                        .filter(|id| clients.contains(id));
                    columns.push(ScrollingColumn {
                        clients,
                        width_factor: scrolling
                            .column_width_factors
                            .get(old_index)
                            .copied()
                            .unwrap_or(1.0),
                        focused,
                    });
                }
                if columns.is_empty() && !scrolling.attach_new_windows_to_focused_column {
                    continue;
                }
                contexts
                    .entry((mask, LayoutEnum::SCROLLING.0))
                    .or_insert_with(|| ShapeContext {
                        tag_mask: mask,
                        layout: LayoutEnum::SCROLLING.0.to_owned(),
                        resize: None,
                        scrolling: None,
                    })
                    .scrolling = Some(ScrollingShape {
                    columns,
                    focused_column,
                    attach_new: scrolling.attach_new_windows_to_focused_column,
                });
            }
            for shape in contexts.into_values() {
                if exact_shape_tags(shape.tag_mask, tagmask) {
                    candidates.push((index, shape));
                } else {
                    invalid_masks += 1;
                }
            }
        }
        if invalid_masks != 0 {
            log::warn!(
                "[restart] skipped {invalid_masks} optional shape contexts with retired tag scopes"
            );
        }
        candidates.sort_by(|(a_index, a), (b_index, b)| {
            let priority = |index: usize, shape: &ShapeContext| {
                let monitor = &self.state.monitors[live[index].0];
                if shape.tag_mask != monitor.get_active_tags() {
                    2
                } else if shape.layout != monitor.lt.0 {
                    1
                } else {
                    0
                }
            };
            (
                priority(*a_index, a),
                snapshot.selected_monitor != Some(*a_index),
                *a_index,
                a.tag_mask,
                a.layout.as_str(),
            )
                .cmp(&(
                    priority(*b_index, b),
                    snapshot.selected_monitor != Some(*b_index),
                    *b_index,
                    b.tag_mask,
                    b.layout.as_str(),
                ))
        });
        candidates
    }

    pub(crate) fn restart_has_shapes(&self) -> bool {
        self.restart_state
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| {
                snapshot
                    .monitors
                    .iter()
                    .any(|monitor| !monitor.shapes.is_empty())
            })
    }

    pub(crate) fn restart_adoption_in_progress(&self) -> bool {
        self.restart_state.adopting
    }

    /// Called after selection acquisition, but before setup/adoption. A failed
    /// bootstrap leaves server properties intact for the bounded retry loop.
    pub(crate) fn install_restart_state(
        &mut self,
        backend: &dyn Backend,
        intent: &RestartIntent,
    ) -> Result<(), Box<dyn Error>> {
        let Some(root) = backend
            .root_window()
            .filter(|root| xid(backend, *root).is_some())
        else {
            return Ok(());
        };
        let Some(bytes) = backend
            .property_ops()
            .get_restart_property(root, RestartProperty::Snapshot)?
        else {
            log::warn!(
                "[restart] X11 workspace snapshot missing or malformed; using ordinary adoption"
            );
            return Ok(());
        };
        self.restart_state.snapshot = RestartSnapshot::decode(&bytes, intent);
        if self.restart_state.snapshot.is_none() {
            log::warn!("[restart] rejected stale or invalid X11 workspace snapshot");
        }
        Ok(())
    }

    pub(crate) fn restore_restart_monitors(&mut self, backend: &mut dyn Backend) {
        let Some(snapshot) = self.restart_state.snapshot.as_ref() else {
            return;
        };
        let live = live_outputs(self, backend);
        let outputs: Vec<_> = live.iter().map(|(_, output)| output.clone()).collect();
        let sources: Vec<_> = snapshot
            .monitors
            .iter()
            .map(|monitor| monitor.output.clone())
            .collect();
        let tagmask = CONFIG.load().tagmask();
        let mut claimed = HashSet::new();
        for (source, saved) in snapshot.monitors.iter().enumerate() {
            let Some(index) = exact_output(&saved.output, &outputs, &sources) else {
                continue;
            };
            // Two saved identities must never overwrite the same live panel.
            if !claimed.insert(index) {
                continue;
            }
            let key = live[index].0;
            let monitor = &mut self.state.monitors[key];
            monitor.tag_set = saved.tag_set.map(|mask| legal_tags(mask, tagmask, 1));
            monitor.sel_tags = saved.sel_tags;
            if let Some(pertag) = monitor.pertag.as_mut() {
                for (index, tag) in saved.tags.iter().enumerate().take(pertag.lts.len()) {
                    if let Some(layout) = LayoutEnum::from_name(&tag.layout) {
                        pertag.lts[index] = Rc::new(layout.clone());
                    }
                    if let Some(layout) = LayoutEnum::from_name(&tag.alt) {
                        pertag.prev_lts[index] = Rc::new(layout.clone());
                    }
                    pertag.n_masters[index] = tag.n_master;
                    pertag.m_facts[index] = tag.m_fact;
                    pertag.gaps[index] = tag.gap;
                    pertag.show_bars[index] = tag.show_bar;
                }
                let active = monitor.tag_set[monitor.sel_tags];
                pertag.cur_tag =
                    restored_tag_slot(saved.cur_tag, active, tagmask, pertag.lts.len() - 1);
            }
            monitor.reload_current_tag_context();
            if let Some(pertag) = monitor.pertag.as_mut() {
                pertag.prev_tag = restored_tag_slot(
                    saved.prev_tag,
                    monitor.tag_set[1 - monitor.sel_tags],
                    tagmask,
                    pertag.lts.len() - 1,
                );
            }
            if snapshot.selected_monitor == Some(source) {
                self.state.sel_mon = Some(key);
            }
        }
        self.refresh_compositor_monitors(backend);
    }

    /// The manage caller already proved this window is a live root child.
    /// Keep the marker until all setup succeeds, rather than consuming it in
    /// the middle of a bootstrap which may still fail and retry.
    pub(crate) fn restore_restart_client(
        &mut self,
        backend: &dyn Backend,
        key: ClientKey,
    ) -> Result<bool, Box<dyn Error>> {
        let Some(snapshot) = self.restart_state.snapshot.as_ref() else {
            return Ok(false);
        };
        let Some(client) = self.state.clients.get(key) else {
            return Ok(false);
        };
        let Some(id) = xid(backend, client.win) else {
            return Ok(false);
        };
        let Some(saved) = snapshot.clients.iter().find(|client| client.xid == id) else {
            return Ok(false);
        };
        if backend
            .property_ops()
            .get_restart_property(client.win, RestartProperty::ClientGeneration)?
            .as_deref()
            != Some(snapshot.intent.generation.as_bytes())
        {
            return Ok(false);
        }
        let live = live_outputs(self, backend);
        let outputs: Vec<_> = live.iter().map(|(_, output)| output.clone()).collect();
        let sources: Vec<_> = snapshot
            .monitors
            .iter()
            .map(|monitor| monitor.output.clone())
            .collect();
        let output = &snapshot.monitors[saved.monitor].output;
        let Some(index) =
            exact_output(output, &outputs, &sources).or_else(|| fallback_output(output, &outputs))
        else {
            return Ok(false);
        };
        let monitor = live[index].0;
        let tags = legal_tags(
            saved.tags,
            CONFIG.load().tagmask(),
            self.state.monitors[monitor].get_active_tags(),
        );
        let client = &mut self.state.clients[key];
        client.mon = Some(monitor);
        client.state.tags = tags;
        client.state.client_fact = saved.client_fact;
        self.restart_state.adopted.insert(id);
        self.restart_state.adopted_keys.insert(id, key);
        Ok(true)
    }

    /// Rebase a verified restart's hand-float rectangle only when its output
    /// geometry changed. Ordinary adoption and deliberately partial off-screen
    /// placement on an unchanged output keep their exact absolute rectangle.
    pub(super) fn restart_floating_restore_rect(
        &self,
        backend: &dyn Backend,
        key: ClientKey,
        rect: Rect,
    ) -> Rect {
        let Some(client) = self.state.clients.get(key) else {
            return rect;
        };
        let Some(id) =
            xid(backend, client.win).filter(|id| self.restart_state.adopted.contains(id))
        else {
            return rect;
        };
        let Some(snapshot) = self.restart_state.snapshot.as_ref() else {
            return rect;
        };
        let Some(saved) = snapshot.clients.iter().find(|saved| saved.xid == id) else {
            return rect;
        };
        let Some((target, work)) = client
            .mon
            .and_then(|monitor| self.monitor_migration_areas(monitor))
        else {
            return rect;
        };
        let source = snapshot.monitors[saved.monitor].output.rect;
        if source == [target.x, target.y, target.w, target.h] {
            return rect;
        }
        // The snapshot stores output bounds, not work areas. Translate by
        // output origins first, then clamp to today's work area; otherwise a
        // top bar's inset would be added twice to the saved vertical offset.
        let translated = Rect::new(
            target.x.saturating_add(rect.x.saturating_sub(source[0])),
            target.y.saturating_add(rect.y.saturating_sub(source[1])),
            rect.w,
            rect.h,
        );
        super::monitor::translate_and_clamp_restore_rect(
            translated,
            None,
            work,
            client.geometry.border_w,
        )
    }

    fn restart_focus_eligible(&self, key: ClientKey, monitor: MonitorKey, tags: u32) -> bool {
        self.is_application_focus_eligible(key)
            && self.state.clients.get(key).is_some_and(|client| {
                client.mon == Some(monitor)
                    && !client.state.is_hidden
                    && !client.state.is_swallowed
                    && (client.state.is_sticky || client.state.tags & tags != 0)
            })
            && self
                .state
                .monitor_clients
                .get(monitor)
                .is_some_and(|clients| clients.contains(&key))
    }

    pub(crate) fn finish_restart_adoption(&mut self, backend: &mut dyn Backend) {
        // Other initial windows (shell/transient clients) must not change the
        // saved view. Reconcile again after the single authoritative scan.
        self.restore_restart_monitors(backend);
        let Some(snapshot) = self.restart_state.snapshot.as_ref() else {
            return;
        };
        let restored: HashMap<_, _> = self
            .state
            .client_order
            .iter()
            .filter_map(|key| {
                let client = self.state.clients.get(*key)?;
                let id = xid(backend, client.win)?;
                // An XID can be reused after its original client was adopted.
                // Only the still-live runtime key that passed the generation
                // check may inherit that client's saved order or selection.
                (self.restart_state.adopted_keys.get(&id) == Some(key)).then_some((id, *key))
            })
            .collect();
        for (monitor_key, clients) in self.state.monitor_clients.iter_mut() {
            let mut ordered = Vec::new();
            for saved in &snapshot.monitors {
                for id in &saved.order {
                    if let Some(key) = restored
                        .get(id)
                        .filter(|key| self.state.clients[**key].mon == Some(monitor_key))
                    {
                        if !ordered.contains(key) {
                            ordered.push(*key);
                        }
                    }
                }
            }
            for key in clients.iter() {
                if !ordered.contains(key) {
                    ordered.push(*key);
                }
            }
            // Preserve the existing tiled-first/floating-last invariant even
            // when disconnected outputs merge their saved client orders.
            ordered.sort_by_key(|key| self.state.clients[*key].state.is_floating);
            *clients = ordered;
        }
        let live = live_outputs(self, backend);
        let outputs: Vec<_> = live.iter().map(|(_, output)| output.clone()).collect();
        let sources: Vec<_> = snapshot
            .monitors
            .iter()
            .map(|monitor| monitor.output.clone())
            .collect();
        let destinations: Vec<_> = sources
            .iter()
            .map(|source| {
                let exact = exact_output(source, &outputs, &sources);
                (exact, exact.or_else(|| fallback_output(source, &outputs)))
            })
            .collect();

        // Maximize adopts before attach and therefore cannot recover a tile's
        // old successor at manage time. Rebind both ends only after every
        // surviving window has passed generation and runtime-key checks.
        let anchors: Vec<_> = snapshot
            .clients
            .iter()
            .filter_map(|saved| {
                let key = *restored.get(&saved.xid)?;
                let client = self.state.clients.get(key)?;
                if !client.state.maximize_restore_tiled || !client.state.maximized_axes().any() {
                    return None;
                }
                let anchor = saved
                    .maximize_anchor
                    .and_then(|id| restored.get(&id))
                    .copied()
                    .filter(|anchor| {
                        self.state.clients.get(*anchor).is_some_and(|other| {
                            other.mon == client.mon
                                && (!other.state.is_floating || other.state.maximize_restore_tiled)
                        }) && client.mon.is_some_and(|monitor| {
                            self.state
                                .monitor_clients
                                .get(monitor)
                                .is_some_and(|clients| {
                                    clients.contains(&key) && clients.contains(anchor)
                                })
                        })
                    });
                Some((key, anchor))
            })
            .collect();
        for (key, anchor) in anchors {
            self.state.clients[key].state.maximize_restore_anchor = anchor;
        }

        // Preserve each surviving panel's MRU ahead of clients arriving from
        // a missing panel. New/unverified clients keep their ordinary relative
        // order after the saved stack. Legacy snapshots leave it untouched.
        for (index, (key, _)) in live.iter().enumerate() {
            if !snapshot.monitors.iter().enumerate().any(|(source, saved)| {
                destinations[source].1 == Some(index) && !saved.mru.is_empty()
            }) {
                continue;
            }
            let mut stack = Vec::new();
            let mut seen = HashSet::new();
            for exact in [true, false] {
                for (source, saved) in snapshot.monitors.iter().enumerate() {
                    let (owner, destination) = destinations[source];
                    if destination != Some(index) || owner.is_some() != exact {
                        continue;
                    }
                    for id in &saved.mru {
                        if let Some(client_key) = restored.get(id).copied().filter(|client_key| {
                            self.state.clients[*client_key].mon == Some(*key)
                                && self.is_application_focus_eligible(*client_key)
                                && self
                                    .state
                                    .monitor_clients
                                    .get(*key)
                                    .is_some_and(|clients| clients.contains(client_key))
                        }) {
                            if seen.insert(client_key) {
                                stack.push(client_key);
                            }
                        }
                    }
                }
            }
            for client_key in self.state.monitor_stack.get(*key).into_iter().flatten() {
                if self
                    .state
                    .clients
                    .get(*client_key)
                    .is_some_and(|client| client.mon == Some(*key))
                    && seen.insert(*client_key)
                {
                    stack.push(*client_key);
                }
            }
            self.state.monitor_stack.insert(*key, stack);
        }

        let tagmask = CONFIG.load().tagmask();
        let mut claimed = HashSet::new();
        for (source, saved) in snapshot.monitors.iter().enumerate() {
            let Some(index) = destinations[source].0 else {
                continue;
            };
            if !claimed.insert(index) {
                continue;
            }
            let key = live[index].0;
            let monitor = &self.state.monitors[key];
            let active = monitor.get_active_tags();
            let history = (!saved.tag_selected.is_empty()).then(|| {
                (0..monitor.pertag.as_ref().map_or(0, |pertag| pertag.sel.len()))
                    .map(|slot| {
                        let tags = if slot == 0 {
                            tagmask
                        } else {
                            1u32 << (slot - 1)
                        };
                        saved
                            .tag_selected
                            .get(slot)
                            .copied()
                            .flatten()
                            .and_then(|id| restored.get(&id))
                            .copied()
                            .filter(|client| self.restart_focus_eligible(*client, key, tags))
                    })
                    .collect::<Vec<_>>()
            });
            let selected = saved
                .selected
                .and_then(|id| restored.get(&id))
                .copied()
                .filter(|selected| self.restart_focus_eligible(*selected, key, active))
                .or_else(|| {
                    history.as_ref().and_then(|history| {
                        monitor
                            .pertag
                            .as_ref()
                            .and_then(|pertag| history.get(pertag.cur_tag).copied().flatten())
                    })
                })
                .or_else(|| {
                    monitor
                        .sel
                        .filter(|selected| self.restart_focus_eligible(*selected, key, active))
                })
                .or_else(|| {
                    self.state
                        .monitor_stack
                        .get(key)
                        .into_iter()
                        .flatten()
                        .copied()
                        .find(|selected| self.restart_focus_eligible(*selected, key, active))
                });
            if let Some(history) = history
                && let Some(pertag) = self.state.monitors[key].pertag.as_mut()
            {
                pertag.sel = history;
            }
            self.state.monitors[key].set_selected_client_for_current_tag(selected);
            if let Some(selected) = selected {
                if let Some(stack) = self.state.monitor_stack.get_mut(key) {
                    stack.retain(|entry| *entry != selected);
                    stack.insert(0, selected);
                }
            }
        }

        // A removed selected output may have sent its still-live focused
        // window to a surviving panel. Follow that verified placement without
        // replacing the survivor's tag/layout. Focus it only on a visible tag.
        if let Some(source) = snapshot.selected_monitor
            && destinations[source].0.is_none()
            && let Some(selected) = snapshot.monitors[source]
                .selected
                .and_then(|id| restored.get(&id))
                .copied()
            && let Some(monitor) = self.state.clients[selected].mon
            && let Some(target) = self.state.monitors.get(monitor)
        {
            let active = target.get_active_tags();
            self.state.sel_mon = Some(monitor);
            if self.restart_focus_eligible(selected, monitor, active) {
                self.state.monitors[monitor].set_selected_client_for_current_tag(Some(selected));
                if let Some(stack) = self.state.monitor_stack.get_mut(monitor) {
                    stack.retain(|entry| *entry != selected);
                    stack.insert(0, selected);
                }
            }
        }
        self.restore_restart_shapes(backend);
        self.protect_restart_focus(backend);
    }

    pub(crate) fn protect_restart_focus(&mut self, backend: &mut dyn Backend) {
        if self.restart_state.snapshot.is_none() {
            return;
        }
        // Restart can finish long after manage's last focus guard, or with
        // no managed windows at all. Re-arm it at the completed handoff and
        // seed pointer-monitor history from today's pointer, so stationary
        // pointer motion on another screen does not undo keyboard selection.
        self.state.motion_mon = backend
            .input_ops()
            .get_pointer_position()
            .ok()
            .and_then(|(x, y)| {
                self.last_mouse_root = (x, y);
                self.recttomon(backend, x.floor() as i32, y.floor() as i32)
            })
            .or(self.state.sel_mon);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        self.suppress_mouse_focus_until = Some(
            self.suppress_mouse_focus_until
                .map_or(deadline, |previous| previous.max(deadline)),
        );
    }

    /// A saved XID is usable only through the generation-verified slotmap key.
    /// A new client reusing the XID or slot cannot inherit the old shape.
    fn restore_restart_shapes(&mut self, backend: &dyn Backend) {
        let Some(snapshot) = self.restart_state.snapshot.as_ref() else {
            return;
        };
        let live = live_outputs(self, backend);
        let outputs: Vec<_> = live.iter().map(|(_, output)| output.clone()).collect();
        let sources: Vec<_> = snapshot
            .monitors
            .iter()
            .map(|monitor| monitor.output.clone())
            .collect();
        let tagmask = CONFIG.load().tagmask();
        let mut claimed = HashSet::new();
        for saved in &snapshot.monitors {
            let Some(index) = exact_output(&saved.output, &outputs, &sources) else {
                continue;
            };
            if !claimed.insert(index) {
                continue;
            }
            let monitor_key = live[index].0;
            for shape in &saved.shapes {
                // Clamping an exact scope could merge separate workspaces and
                // let a retired tag overwrite a surviving tag's shape.
                if !exact_shape_tags(shape.tag_mask, tagmask) {
                    continue;
                }
                let Some(layout) = canonical_shape_layout(&shape.layout) else {
                    continue;
                };
                let eligible = |id: u32| -> Option<ClientKey> {
                    let key = *self.restart_state.adopted_keys.get(&id)?;
                    let client = self.state.clients.get(key)?;
                    (xid(backend, client.win) == Some(id)
                        && client.mon == Some(monitor_key)
                        && self
                            .state
                            .monitor_clients
                            .get(monitor_key)
                            .is_some_and(|clients| clients.contains(&key))
                        && !client.state.is_floating
                        && !client.state.is_hidden
                        && !client.state.is_swallowed
                        && (client.state.is_sticky || client.state.tags & shape.tag_mask != 0))
                        .then_some(key)
                };
                let resize = shape.resize.as_ref().and_then(|anchor| {
                    Some(ResizeIntent {
                        client: eligible(anchor.xid)?,
                        width: anchor.width,
                        height: anchor.height,
                    })
                });
                let scrolling = shape.scrolling.as_ref().map(|saved| {
                    let mut restored = ScrollingState::new();
                    restored.attach_new_windows_to_focused_column = saved.attach_new;
                    for (old_index, column) in saved.columns.iter().enumerate() {
                        let clients: Vec<_> = column
                            .clients
                            .iter()
                            .filter_map(|id| eligible(*id))
                            .collect();
                        if clients.is_empty() {
                            continue;
                        }
                        if saved.focused_column == Some(old_index) {
                            restored.focused_column = Some(restored.columns.len());
                        }
                        let focused = column
                            .focused
                            .and_then(eligible)
                            .filter(|key| clients.contains(key));
                        restored.columns.push(clients);
                        restored.column_width_factors.push(column.width_factor);
                        restored.focused_clients.push(focused);
                    }
                    // Viewport/geometry is deliberately rebuilt by normal sync.
                    restored
                });
                if let Some(scrolling) = scrolling {
                    self.scrolling_states
                        .insert((monitor_key, shape.tag_mask), scrolling);
                }
                if let Some(intent) = resize {
                    self.state.monitors[monitor_key].layout_resizes.insert(
                        (shape.tag_mask, layout.0),
                        ResizeContext {
                            intent: Some(intent),
                            ..Default::default()
                        },
                    );
                }
            }
        }
    }

    /// Only initial exact adoption needs this guard; normal placement and
    /// Wayland behavior remain unchanged. Off-view is not semantic minimized.
    pub(crate) fn restart_client_off_view(&self, backend: &dyn Backend, key: ClientKey) -> bool {
        self.state
            .clients
            .get(key)
            .and_then(|client| xid(backend, client.win))
            .is_some_and(|id| self.restart_state.adopted.contains(&id))
            && !self.is_client_visible_by_key(key)
    }

    /// Best-effort, generation-checked cleanup. Never delete another restart's
    /// state; missing/destroyed windows are expected after adoption.
    pub(crate) fn clear_restart_state(&self, backend: &dyn Backend, intent: &RestartIntent) {
        for (_, client) in self.state.clients.iter() {
            match backend
                .property_ops()
                .get_restart_property(client.win, RestartProperty::ClientGeneration)
            {
                Ok(Some(value)) if value.as_slice() == intent.generation.as_bytes() => {
                    if let Err(error) = backend
                        .property_ops()
                        .clear_restart_property(client.win, RestartProperty::ClientGeneration)
                    {
                        log::warn!("[restart] clearing client marker {:?}: {error}", client.win);
                    }
                }
                Err(error) => log::warn!(
                    "[restart] reading client marker during cleanup {:?}: {error}",
                    client.win
                ),
                _ => {}
            }
        }
        if let Some(root) = backend.root_window() {
            match backend
                .property_ops()
                .get_restart_property(root, RestartProperty::Snapshot)
            {
                Ok(Some(value)) if RestartSnapshot::decode(&value, intent).is_some() => {
                    if let Err(error) = backend
                        .property_ops()
                        .clear_restart_property(root, RestartProperty::Snapshot)
                    {
                        log::warn!("[restart] clearing root snapshot: {error}");
                    }
                }
                Err(error) => log::warn!("[restart] reading root snapshot during cleanup: {error}"),
                _ => {}
            }
        }
    }

    pub(crate) fn consume_restart_state(&mut self, backend: &dyn Backend, intent: &RestartIntent) {
        self.clear_restart_state(backend, intent);
        self.restart_state = PendingRestartState::default();
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn output(key: &str, connector: &str, x: i32) -> Output {
        Output {
            stable_key: key.into(),
            connector: connector.into(),
            rect: [x, 0, 1920, 1080],
        }
    }

    #[test]
    fn enumeration_order_does_not_move_the_panel() {
        let a = output("panel-a", "DP-1", 0);
        let b = output("panel-b", "DP-2", 1920);
        let saved = vec![a.clone(), b.clone()];
        assert_eq!(exact_output(&a, &[b.clone(), a.clone()], &saved), Some(1));
        assert_eq!(exact_output(&b, &[b.clone(), a], &saved), Some(0));
    }

    #[test]
    fn legacy_empty_output_identities_restore_only_a_complete_unique_geometry_bijection() {
        let left = output("", "", 0);
        let right = output("", "", 1920);
        let saved = vec![left.clone(), right.clone()];
        let live = vec![
            output("new-right", "DP-2", 1920),
            output("new-left", "DP-1", 0),
        ];
        assert_eq!(exact_output(&left, &live, &saved), Some(1));
        assert_eq!(exact_output(&right, &live, &saved), Some(0));
        assert_eq!(exact_output(&left, &saved, &saved), Some(0));
        assert_eq!(exact_output(&right, &saved, &saved), Some(1));

        // Missing, added, moved/resized, and mirrored panels provide no safe
        // identity for *any* of the old anonymous monitor contexts.
        let mut resized = live.clone();
        resized[0].rect[2] = 1280;
        for changed in [
            vec![live[0].clone()],
            vec![
                live[0].clone(),
                live[1].clone(),
                output("extra", "DP-3", 3840),
            ],
            vec![live[0].clone(), output("left", "DP-1", -1920)],
            resized,
            vec![live[1].clone(), live[1].clone()],
        ] {
            assert_eq!(exact_output(&left, &changed, &saved), None);
            assert_eq!(exact_output(&right, &changed, &saved), None);
        }
        assert_eq!(
            exact_output(&left, &live, &[left.clone(), left.clone()]),
            None
        );
    }

    #[test]
    fn legacy_geometry_never_replaces_named_panel_identity() {
        let anonymous = output("", "", 0);
        let named = output("named", "DP-2", 1920);
        let saved = vec![anonymous.clone(), named.clone()];
        let unchanged = vec![output("left", "DP-1", 0), named.clone()];
        assert_eq!(exact_output(&anonymous, &unchanged, &saved), Some(0));
        assert_eq!(exact_output(&named, &unchanged, &saved), Some(1));

        let moved = vec![
            output("named", "DP-1", 0),
            output("replacement", "DP-2", 1920),
        ];
        assert_eq!(exact_output(&anonymous, &moved, &saved), None);
        assert_eq!(exact_output(&named, &moved, &saved), Some(0));

        let missing = output("gone", "gone-connector", 0);
        assert_eq!(
            exact_output(&missing, &[output("new", "new", 0)], std::slice::from_ref(&missing)),
            None
        );
        let partial_identity = output("known-stable", "", 0);
        assert_eq!(
            exact_output(
                &partial_identity,
                &[output("new", "new", 0)],
                std::slice::from_ref(&partial_identity)
            ),
            None
        );
    }

    #[test]
    fn unplugged_duplicate_edid_cannot_claim_the_remaining_panel() {
        let a = output("same-edid", "DP-1", 0);
        let b = output("same-edid", "DP-2", 1920);
        let saved = vec![a.clone(), b.clone()];
        assert_eq!(exact_output(&a, std::slice::from_ref(&b), &saved), None);
        assert_eq!(exact_output(&b, std::slice::from_ref(&b), &saved), Some(0));
    }

    #[test]
    fn duplicate_identity_requires_a_unique_connector_on_both_sides() {
        let a = output("same", "DP-1", 0);
        let b = output("same", "DP-2", 1920);
        let saved = vec![a.clone(), b.clone()];
        assert_eq!(exact_output(&a, &[b, a.clone()], &saved), Some(1));
        assert_eq!(exact_output(&a, &[a.clone(), a.clone()], &saved), None);
        assert_eq!(
            exact_output(&a, std::slice::from_ref(&a), &[a.clone(), a.clone()]),
            None
        );
    }

    #[test]
    fn unplugged_source_cannot_steal_another_panel_on_its_old_connector() {
        let a = output("a", "DP-1", 0);
        let b = output("b", "DP-2", 1920);
        let moved_b = output("b", "DP-1", 0);
        for saved in [vec![a.clone(), b.clone()], vec![b.clone(), a.clone()]] {
            assert_eq!(
                exact_output(&a, std::slice::from_ref(&moved_b), &saved),
                None
            );
            assert_eq!(
                exact_output(&b, std::slice::from_ref(&moved_b), &saved),
                Some(0)
            );
        }
    }

    #[test]
    fn unique_panel_can_change_connector() {
        let a = output("serial-a", "DP-1", 0);
        let moved = output("serial-a", "HDMI-2", 1000);
        assert_eq!(
            exact_output(&a, &[moved], std::slice::from_ref(&a)),
            Some(0)
        );
    }

    #[test]
    fn missing_output_falls_back_by_geometry_without_layout_ownership() {
        let missing = output("gone", "gone", -1920);
        let live = vec![output("a", "DP-1", 1920), output("b", "DP-2", 0)];
        assert_eq!(
            exact_output(&missing, &live, std::slice::from_ref(&missing)),
            None
        );
        assert_eq!(fallback_output(&missing, &live), Some(1));
        assert_eq!(fallback_output(&missing, &[]), None);
    }

    #[test]
    fn extreme_coordinates_do_not_overflow_geometry_fallback() {
        let a = output("gone", "gone", i32::MIN);
        let live = [
            output("far", "DP-1", i32::MAX),
            output("near", "DP-2", i32::MIN),
        ];
        assert_eq!(fallback_output(&a, &live), Some(1));
    }

    #[test]
    fn multi_tag_masks_are_preserved_instead_of_reduced_to_one_desktop() {
        assert_eq!(legal_tags(0b10101, 0b111111, 2), 0b10101);
        assert_eq!(legal_tags(u32::MAX, 0b111, 2), 0b111);
    }

    #[test]
    fn tag_count_shrink_recomputes_all_tags_slot_before_retaining_old_slot() {
        assert_eq!(restored_tag_slot(1, 0b11, 0b11, 2), 0);
        assert_eq!(restored_tag_slot(2, 0b11, 0b111, 3), 2);
        assert_eq!(restored_tag_slot(0, 0b1, 0b1, 1), 1);
        assert_eq!(restored_tag_slot(9, 0b10, 0b111, 3), 2);
    }

    #[test]
    fn every_client_is_verified_before_root_snapshot_publication() {
        let mut steps = Vec::new();
        publish_restart_snapshot(&[7, 9], |step| {
            steps.push(step);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(
            steps,
            vec![
                RestartWriteStep::WriteClient(7),
                RestartWriteStep::VerifyClient(7),
                RestartWriteStep::WriteClient(9),
                RestartWriteStep::VerifyClient(9),
                RestartWriteStep::WriteRoot,
                RestartWriteStep::VerifyRoot
            ]
        );
    }

    #[test]
    fn every_write_or_verification_error_stops_before_any_later_step() {
        for failure in 0..6 {
            let mut observed = 0;
            let result = publish_restart_snapshot(&[7, 9], |_| {
                let this = observed;
                observed += 1;
                if this == failure { Err(this) } else { Ok(()) }
            });
            assert_eq!(result, Err(failure));
            assert_eq!(observed, failure + 1);
        }
    }

    #[test]
    fn retired_tags_fall_back_to_the_destination_current_view() {
        assert_eq!(legal_tags(0b10000, 0b111, 0b110), 0b110);
        assert_eq!(legal_tags(0, 0b111, 0), 1);
        assert_eq!(legal_tags(0, 0x80000000, 0), 0x80000000);
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    fn intent() -> RestartIntent {
        RestartIntent {
            version: VERSION,
            pid: 8123,
            captured_ms: 1000,
            generation: "0123456789abcdef0123456789abcdef".into(),
        }
    }

    fn snapshot() -> RestartSnapshot {
        let tag = Tag {
            layout: "tile".into(),
            alt: "float".into(),
            n_master: 1,
            m_fact: 0.55,
            gap: 8,
            show_bar: true,
        };
        RestartSnapshot {
            intent: intent(),
            monitors: vec![Monitor {
                output: Output {
                    stable_key: "panel".into(),
                    connector: "DP-1".into(),
                    rect: [0, 0, 1920, 1080],
                },
                tag_set: [0b101, 0b10],
                sel_tags: 0,
                cur_tag: 1,
                prev_tag: 2,
                tags: vec![tag; 4],
                order: vec![42],
                selected: Some(42),
                mru: Vec::new(),
                tag_selected: Vec::new(),
                shapes: Vec::new(),
            }],
            clients: vec![Client {
                xid: 42,
                tags: 0b101,
                monitor: 0,
                client_fact: 2.5,
                maximize_anchor: None,
            }],
            selected_monitor: Some(0),
        }
    }

    #[test]
    fn explicit_same_exec_intent_is_bounded_and_expires() {
        let intent = intent();
        assert_eq!(
            RestartIntent::decode(&intent.encode(), 8123, 1001),
            Some(intent.clone())
        );
        assert!(RestartIntent::decode(&intent.encode(), 8124, 1001).is_none());
        assert!(RestartIntent::decode(&intent.encode(), 8123, 999).is_none());
        assert!(RestartIntent::decode(&intent.encode(), 8123, 1001 + MAX_AGE_MS).is_none());
        assert!(RestartIntent::decode(&"x".repeat(257), 8123, 1001).is_none());
    }

    #[test]
    fn snapshot_roundtrip_preserves_multitag_and_every_layout_parameter() {
        let saved = snapshot();
        let bytes = saved.encode().expect("valid snapshot");
        assert_eq!(RestartSnapshot::decode(&bytes, &saved.intent), Some(saved));
    }

    #[test]
    fn restart_focus_history_and_maximize_anchor_roundtrip_with_legacy_defaults() {
        let legacy = snapshot();
        let encoded = serde_json::to_value(&legacy).unwrap();
        assert!(encoded["monitors"][0].get("mru").is_none());
        assert!(encoded["monitors"][0].get("tag_selected").is_none());
        assert!(encoded["clients"][0].get("maximize_anchor").is_none());
        assert_eq!(
            RestartSnapshot::decode(&legacy.encode().unwrap(), &intent()),
            Some(legacy)
        );

        let mut saved = snapshot();
        saved.clients.push(Client {
            xid: 43,
            ..saved.clients[0].clone()
        });
        saved.clients[0].maximize_anchor = Some(43);
        saved.monitors[0].order.push(43);
        saved.monitors[0].mru = vec![42, 43];
        saved.monitors[0].tag_selected = vec![Some(43), Some(42), None, Some(43)];
        assert_eq!(
            RestartSnapshot::decode(&saved.encode().unwrap(), &intent()),
            Some(saved)
        );
    }

    #[test]
    fn restart_focus_history_and_maximize_anchor_reject_invalid_references() {
        for mru in [vec![42, 42], vec![43], vec![0]] {
            let mut saved = snapshot();
            saved.monitors[0].mru = mru;
            assert!(!saved.validate());
        }
        for history in [
            vec![Some(42)],
            vec![None; 5],
            vec![None, Some(43), None, None],
        ] {
            let mut saved = snapshot();
            saved.monitors[0].tag_selected = history;
            assert!(!saved.validate());
        }
        for anchor in [0, 42, 43] {
            let mut saved = snapshot();
            saved.clients[0].maximize_anchor = Some(anchor);
            assert!(!saved.validate());
        }
        let mut saved = snapshot();
        let mut other = saved.monitors[0].clone();
        other.order = vec![43];
        other.selected = Some(43);
        saved.monitors.push(other);
        saved.clients.push(Client {
            xid: 43,
            monitor: 1,
            ..saved.clients[0].clone()
        });
        assert!(saved.validate());
        saved.clients[0].maximize_anchor = Some(43);
        assert!(
            !saved.validate(),
            "an anchor cannot cross saved output ownership"
        );
        saved.clients[0].maximize_anchor = None;
        saved.monitors[0].tag_selected = vec![None, Some(43), None, None];
        assert!(
            !saved.validate(),
            "a per-tag selection cannot cross saved output ownership"
        );
    }

    #[test]
    fn older_snapshots_default_missing_client_weight_to_one() {
        let mut value = serde_json::to_value(snapshot()).unwrap();
        value["clients"][0]
            .as_object_mut()
            .unwrap()
            .remove("client_fact");
        let bytes = serde_json::to_vec(&value).unwrap();
        let restored = RestartSnapshot::decode(&bytes, &intent()).unwrap();
        assert_eq!(restored.clients[0].client_fact, 1.0);
    }

    #[test]
    fn invalid_client_weights_reject_the_entire_snapshot() {
        for factor in [
            0.0,
            -1.0,
            0.249,
            4.001,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            let mut saved = snapshot();
            saved.clients[0].client_fact = factor;
            assert!(!saved.validate(), "accepted {factor}");
            assert!(saved.encode().is_err());
        }
        for value in [
            serde_json::json!(null),
            serde_json::json!("2"),
            serde_json::json!(0),
            serde_json::json!(5),
        ] {
            let mut saved = serde_json::to_value(snapshot()).unwrap();
            saved["clients"][0]["client_fact"] = value;
            assert!(
                RestartSnapshot::decode(&serde_json::to_vec(&saved).unwrap(), &intent()).is_none()
            );
        }
        for factor in [0.25, 1.0, 4.0] {
            let mut saved = snapshot();
            saved.clients[0].client_fact = factor;
            assert_eq!(
                RestartSnapshot::decode(&saved.encode().unwrap(), &intent()),
                Some(saved)
            );
        }
    }

    #[test]
    fn unrelated_generation_and_unknown_version_are_rejected() {
        let saved = snapshot();
        let bytes = saved.encode().unwrap();
        let mut other = saved.intent.clone();
        other.generation = "ffffffffffffffffffffffffffffffff".into();
        assert!(RestartSnapshot::decode(&bytes, &other).is_none());
        let mut unsupported = saved;
        unsupported.intent.version += 1;
        assert!(!unsupported.validate());
    }

    #[test]
    fn duplicate_or_missing_client_order_is_not_a_partial_success() {
        let mut saved = snapshot();
        saved.clients.push(saved.clients[0].clone());
        assert!(!saved.validate());
        let mut saved = snapshot();
        saved.monitors[0].order.clear();
        saved.monitors[0].selected = None;
        assert!(!saved.validate());
        let mut saved = snapshot();
        saved.monitors[0].order.push(42);
        assert!(!saved.validate());
    }

    #[test]
    fn invalid_tags_layout_and_parameters_are_rejected() {
        let mut saved = snapshot();
        saved.clients[0].tags = 0;
        assert!(!saved.validate());
        let mut saved = snapshot();
        saved.monitors[0].tags[0].layout = "unknown".into();
        assert!(!saved.validate());
        let mut saved = snapshot();
        saved.monitors[0].tags[0].m_fact = f32::NAN;
        assert!(!saved.validate());
        let mut saved = snapshot();
        saved.monitors[0].sel_tags = 2;
        assert!(!saved.validate());
    }

    #[test]
    fn malformed_and_oversized_payloads_never_restore() {
        assert!(RestartSnapshot::decode(b"{}", &intent()).is_none());
        assert!(
            RestartSnapshot::decode(&vec![b' '; MAX_RESTART_PROPERTY_BYTES + 1], &intent())
                .is_none()
        );
    }

    fn shape(mask: u32) -> ShapeContext {
        ShapeContext {
            tag_mask: mask,
            layout: "scrolling".into(),
            resize: Some(ResizeAnchor {
                xid: 42,
                width: 0.6,
                height: 0.7,
            }),
            scrolling: Some(ScrollingShape {
                columns: vec![ScrollingColumn {
                    clients: vec![42],
                    width_factor: 1.5,
                    focused: Some(42),
                }],
                focused_column: Some(0),
                attach_new: true,
            }),
        }
    }

    #[test]
    fn shape_codec_roundtrips_committed_metadata_without_runtime_geometry() {
        let mut saved = snapshot();
        saved.monitors[0].shapes.push(shape(0b101));
        let bytes = saved.encode().unwrap();
        assert_eq!(RestartSnapshot::decode(&bytes, &intent()), Some(saved));
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let context = &value["monitors"][0]["shapes"][0];
        assert_eq!(context["tag_mask"], 0b101);
        assert_eq!(context["scrolling"]["attach_new"], true);
        assert!(context.get("viewport_x").is_none());
        assert!(context["resize"].get("frames").is_none());
    }

    #[test]
    fn legacy_snapshots_omit_shapes_and_old_readers_fail_closed_on_new_shapes() {
        let saved = snapshot();
        let value = serde_json::to_value(&saved).unwrap();
        assert!(value["monitors"][0].get("shapes").is_none());
        assert_eq!(
            RestartSnapshot::decode(&serde_json::to_vec(&value).unwrap(), &intent()),
            Some(saved.clone())
        );
        // This is the actual previous monitor wire schema, with serde's same
        // deny_unknown_fields rule, rather than a simulated parser.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct LegacyMonitor {
            output: Output,
            tag_set: [u32; 2],
            sel_tags: usize,
            cur_tag: usize,
            prev_tag: usize,
            tags: Vec<Tag>,
            order: Vec<u32>,
            selected: Option<u32>,
        }
        assert!(serde_json::from_value::<LegacyMonitor>(value["monitors"][0].clone()).is_ok());
        let mut new = saved;
        new.monitors[0].shapes.push(shape(1));
        let value = serde_json::to_value(new).unwrap();
        assert!(serde_json::from_value::<LegacyMonitor>(value["monitors"][0].clone()).is_err());
    }

    #[test]
    fn shape_layouts_are_exact_canonical_nonfloat_including_monocle() {
        for name in ["tile", "monocle", "fullscreen", "scrolling", "vstack"] {
            let mut saved = snapshot();
            let mut context = shape(1);
            context.layout = name.into();
            context.scrolling = None;
            saved.monitors[0].shapes.push(context);
            assert!(saved.encode().is_ok(), "rejected {name}");
        }
        for name in ["float", "unknown", "", " TILE", "Tile", "tile "] {
            let mut saved = snapshot();
            let mut context = shape(1);
            context.layout = name.into();
            context.scrolling = None;
            saved.monitors[0].shapes.push(context);
            assert!(saved.encode().is_err(), "accepted {name}");
        }
        let mut saved = snapshot();
        let mut context = shape(1);
        context.layout = "tile".into();
        saved.monitors[0].shapes.push(context);
        assert!(
            !saved.validate(),
            "scrolling metadata must have scrolling scope"
        );
    }

    #[test]
    fn shape_sizes_and_column_factors_have_finite_inclusive_bounds() {
        for value in [-0.01, 1.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for width in [true, false] {
                let mut saved = snapshot();
                let mut context = shape(1);
                let anchor = context.resize.as_mut().unwrap();
                if width {
                    anchor.width = value;
                } else {
                    anchor.height = value;
                }
                saved.monitors[0].shapes.push(context);
                assert!(saved.encode().is_err());
            }
        }
        for factor in [0.249, 2.501, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut saved = snapshot();
            let mut context = shape(1);
            context.scrolling.as_mut().unwrap().columns[0].width_factor = factor;
            saved.monitors[0].shapes.push(context);
            assert!(saved.encode().is_err());
        }
        for (width, height, factor) in [(0.0, 1.0, 0.25), (1.0, 0.0, 2.5)] {
            let mut saved = snapshot();
            let mut context = shape(1);
            let anchor = context.resize.as_mut().unwrap();
            anchor.width = width;
            anchor.height = height;
            context.scrolling.as_mut().unwrap().columns[0].width_factor = factor;
            saved.monitors[0].shapes.push(context);
            assert_eq!(
                RestartSnapshot::decode(&saved.encode().unwrap(), &intent()),
                Some(saved)
            );
        }
    }

    #[test]
    fn shape_scope_membership_and_focus_are_strict() {
        for case in 0..10 {
            let mut saved = snapshot();
            let mut context = shape(1);
            match case {
                0 => context.tag_mask = 0,
                1 => context.resize.as_mut().unwrap().xid = 0,
                2 => context.resize.as_mut().unwrap().xid = 99,
                3 => context.scrolling.as_mut().unwrap().columns[0]
                    .clients
                    .clear(),
                4 => context.scrolling.as_mut().unwrap().columns[0]
                    .clients
                    .push(42),
                5 => {
                    let scrolling = context.scrolling.as_mut().unwrap();
                    scrolling.columns.push(scrolling.columns[0].clone());
                }
                6 => context.scrolling.as_mut().unwrap().columns[0].focused = Some(99),
                7 => context.scrolling.as_mut().unwrap().focused_column = Some(1),
                8 => context.scrolling.as_mut().unwrap().columns[0].clients = vec![99],
                _ => {
                    context.resize = None;
                    context.scrolling = None;
                }
            }
            saved.monitors[0].shapes.push(context);
            assert!(!saved.validate(), "accepted case {case}");
            assert!(
                RestartSnapshot::decode(&serde_json::to_vec(&saved).unwrap(), &intent()).is_none()
            );
        }
        let mut saved = snapshot();
        saved.monitors[0].shapes = vec![shape(1), shape(1)];
        assert!(!saved.validate());
        // The same XID is valid in distinct exact scopes.
        saved.monitors[0].shapes[1].tag_mask = 3;
        assert!(saved.validate());
        let mut second = saved.monitors[0].clone();
        second.order.clear();
        second.selected = None;
        saved.monitors.push(second);
        assert!(
            !saved.validate(),
            "shape XIDs cannot come from another monitor"
        );
    }

    #[test]
    fn empty_scrolling_attach_preference_is_meaningful() {
        let mut saved = snapshot();
        saved.monitors[0].shapes.push(ShapeContext {
            tag_mask: 1,
            layout: "scrolling".into(),
            resize: None,
            scrolling: Some(ScrollingShape {
                columns: Vec::new(),
                focused_column: None,
                attach_new: true,
            }),
        });
        assert_eq!(
            RestartSnapshot::decode(&saved.encode().unwrap(), &intent()),
            Some(saved)
        );
    }

    #[test]
    fn shape_unknown_fields_are_rejected_at_every_level() {
        let mut saved = snapshot();
        saved.monitors[0].shapes.push(shape(1));
        for level in 0..4 {
            let mut value = serde_json::to_value(&saved).unwrap();
            let shape = &mut value["monitors"][0]["shapes"][0];
            let object = match level {
                0 => shape,
                1 => &mut shape["resize"],
                2 => &mut shape["scrolling"],
                _ => &mut shape["scrolling"]["columns"][0],
            };
            object
                .as_object_mut()
                .unwrap()
                .insert("future".into(), true.into());
            assert!(
                RestartSnapshot::decode(&serde_json::to_vec(&value).unwrap(), &intent()).is_none()
            );
        }
    }

    #[test]
    fn exact_shape_scopes_are_dropped_not_clamped_when_tags_shrink() {
        assert!(exact_shape_tags(0b101, 0b111));
        assert!(exact_shape_tags(0x80000000, u32::MAX));
        assert!(!exact_shape_tags(0b101, 0b011));
        assert!(!exact_shape_tags(0, 0b111));
        assert!(!exact_shape_tags(1, 0));
    }

    #[test]
    fn shape_context_limit_is_global_across_monitors() {
        let mut saved = snapshot();
        saved.monitors[0].shapes = (1..=MAX_SHAPE_CONTEXTS as u32).map(shape).collect();
        assert!(saved.validate());
        let mut second = saved.monitors[0].clone();
        second.order.clear();
        second.selected = None;
        second.shapes = vec![ShapeContext {
            tag_mask: 1,
            layout: "scrolling".into(),
            resize: None,
            scrolling: Some(ScrollingShape {
                columns: Vec::new(),
                focused_column: None,
                attach_new: true,
            }),
        }];
        saved.monitors.push(second);
        assert!(!saved.validate());
    }

    #[test]
    fn optional_budget_evicts_a_deterministic_suffix_of_whole_contexts() {
        let mut saved = snapshot();
        let base = saved.clone();
        let candidates: Vec<_> = (1..=MAX_SHAPE_CONTEXTS as u32 + 1)
            .map(|mask| (0, shape(mask)))
            .collect();
        let encoded = saved.encode_with_shapes(candidates.clone()).unwrap();
        assert!(encoded.len() <= MAX_RESTART_PROPERTY_BYTES);
        assert!(!saved.monitors[0].shapes.is_empty());
        assert!(saved.monitors[0].shapes.len() < MAX_SHAPE_CONTEXTS);
        for (index, shape) in saved.monitors[0].shapes.iter().enumerate() {
            assert_eq!(shape, &candidates[index].1);
            assert!(shape.resize.is_some() && shape.scrolling.is_some());
        }
        let retained = saved.monitors[0].shapes.len();
        let mut over = saved.clone();
        over.monitors[0].shapes.push(candidates[retained].1.clone());
        assert!(over.encode().is_err());
        let mut again = base.clone();
        assert_eq!(again.encode_with_shapes(candidates).unwrap(), encoded);
        let mut stripped = saved;
        stripped.monitors[0].shapes.clear();
        assert_eq!(stripped, base);
        assert!(RestartSnapshot::decode(&encoded, &intent()).is_some());
    }

    #[test]
    fn optional_budget_accounts_for_each_monitor_field_and_evicted_last_entry() {
        let mut saved = snapshot();
        let mut second = saved.monitors[0].clone();
        second.output.stable_key = "panel-2".into();
        second.output.connector = "DP-2".into();
        second.order.clear();
        second.selected = None;
        saved.monitors.push(second);
        let base = saved.clone();
        let empty = |mask| ShapeContext {
            tag_mask: mask,
            layout: "scrolling".into(),
            resize: None,
            scrolling: Some(ScrollingShape {
                columns: Vec::new(),
                focused_column: None,
                attach_new: true,
            }),
        };
        let mut candidates = vec![(1, empty(1)), (0, shape(1))];
        candidates.extend((2..1000).map(|mask| (0, shape(mask))));
        // This lone low-priority entry must disappear with its field overhead
        // if the other monitor already fills the snapshot budget.
        candidates.push((1, empty(2)));
        let bytes = saved.encode_with_shapes(candidates).unwrap();
        assert_eq!(saved.monitors[1].shapes, vec![empty(1)]);
        assert_eq!(
            RestartSnapshot::decode(&bytes, &intent()),
            Some(saved.clone())
        );
        for monitor in &mut saved.monitors {
            monitor.shapes.clear();
        }
        assert_eq!(saved, base);
    }

    #[test]
    fn invalid_optional_context_cannot_poison_valid_base_or_other_shapes() {
        let mut saved = snapshot();
        let mut invalid = shape(1);
        invalid.resize.as_mut().unwrap().width = f64::NAN;
        let encoded = saved
            .encode_with_shapes(vec![
                (0, invalid),
                (1, shape(1)),
                (0, shape(2)),
                (0, shape(2)),
            ])
            .unwrap();
        assert_eq!(saved.monitors[0].shapes, vec![shape(2)]);
        assert!(RestartSnapshot::decode(&encoded, &intent()).is_some());
        let mut invalid_base = snapshot();
        invalid_base.clients[0].client_fact = f32::NAN;
        assert!(
            invalid_base
                .encode_with_shapes(vec![(0, shape(1))])
                .is_err()
        );
    }

    #[test]
    fn exact_property_limit_base_survives_eviction_of_all_optional_shapes() {
        let mut saved = snapshot();
        for index in 1..=32 {
            let mut monitor = saved.monitors[0].clone();
            monitor.output.stable_key = format!("panel-{index}");
            monitor.output.connector = format!("DP-{index}");
            monitor.order.clear();
            monitor.selected = None;
            saved.monitors.push(monitor);
        }
        // ASCII padding has exactly its byte length in JSON. Recheck after
        // each bounded field rather than serializing once per padding byte.
        let mut remaining = MAX_RESTART_PROPERTY_BYTES - serde_json::to_vec(&saved).unwrap().len();
        'padding: for index in 0..saved.monitors.len() {
            for stable_key in [true, false] {
                let output = &mut saved.monitors[index].output;
                let field = if stable_key {
                    &mut output.stable_key
                } else {
                    &mut output.connector
                };
                field.push_str(&"x".repeat(remaining.min(1024 - field.len())));
                remaining = MAX_RESTART_PROPERTY_BYTES - serde_json::to_vec(&saved).unwrap().len();
                if remaining == 0 {
                    break 'padding;
                }
            }
        }
        assert_eq!(remaining, 0, "fixture needs enough bounded string capacity");
        let base = saved.encode().unwrap();
        assert_eq!(base.len(), MAX_RESTART_PROPERTY_BYTES);
        assert_eq!(
            RestartSnapshot::decode(&base, &intent()),
            Some(saved.clone())
        );
        let original = saved.clone();
        let encoded = saved.encode_with_shapes(vec![(0, shape(1))]).unwrap();
        assert_eq!(encoded, base);
        assert_eq!(
            saved, original,
            "every optional context and its field must be evicted"
        );
        assert!(
            saved
                .monitors
                .iter()
                .all(|monitor| monitor.shapes.is_empty())
        );

        // One extra valid ASCII byte crosses the strict property boundary.
        let mut oversized = original;
        let connector = &mut oversized.monitors.last_mut().unwrap().output.connector;
        assert!(connector.len() < 1024);
        connector.push('x');
        assert!(oversized.validate());
        let bytes = serde_json::to_vec(&oversized).unwrap();
        assert_eq!(bytes.len(), MAX_RESTART_PROPERTY_BYTES + 1);
        assert!(RestartSnapshot::decode(&bytes, &intent()).is_none());
        assert!(oversized.encode_with_shapes(vec![(0, shape(1))]).is_err());
        assert!(
            oversized
                .monitors
                .iter()
                .all(|monitor| monitor.shapes.is_empty())
        );
    }

    #[test]
    fn oversized_base_is_still_a_hard_failure_before_optional_mutation() {
        let mut saved = snapshot();
        saved.clients.clear();
        for id in 1..=MAX_CLIENTS as u32 {
            saved.clients.push(Client {
                xid: id,
                tags: 1,
                monitor: 0,
                client_fact: 1.0,
                maximize_anchor: None,
            });
        }
        saved.monitors[0].order = saved.clients.iter().map(|client| client.xid).collect();
        saved.monitors[0].selected = None;
        assert!(saved.validate());
        assert!(saved.encode_with_shapes(vec![(0, shape(1))]).is_err());
        assert!(saved.monitors[0].shapes.is_empty());
    }

    #[test]
    fn actual_restart_intent_is_cleared_from_child_environments() {
        assert!(crate::application::RESTART_ENV_KEYS.contains(&RESTART_STATE_ENV));
    }

    #[test]
    fn lifecycle_restores_before_geometry_and_consumes_after_successful_setup() {
        let client = include_str!("client.rs");
        let body = client
            .split_once("pub(crate) fn manage_regular_client(")
            .unwrap()
            .1
            .split_once("fn apply_minimized_restore_before_adjust(")
            .unwrap()
            .0;
        assert!(
            body.find("apply_minimized_restore_before_adjust(client_key, state)")
                .unwrap()
                < body
                    .find("restore_restart_client(backend, client_key)?")
                    .unwrap()
        );
        assert!(
            body.find("restore_restart_client(backend, client_key)?")
                .unwrap()
                < body
                    .find("self.adjust_client_position(backend, client_key)")
                    .unwrap()
        );
        let application = include_str!("../application.rs");
        let body = application
            .split_once("fn bootstrap_jwm_instance(")
            .unwrap()
            .1
            .split_once("/// Keeps")
            .unwrap()
            .0;
        assert!(
            body.find("jwm.install_restart_state(").unwrap() < body.find("jwm.setup(").unwrap()
        );
        assert!(
            body.find("jwm.setup(&mut *backend)?;").unwrap()
                < body.find("jwm.consume_restart_state(").unwrap()
        );
        assert!(application.contains("pending_workspace_handoff = prepared_workspace_handoff;"));
    }
}
