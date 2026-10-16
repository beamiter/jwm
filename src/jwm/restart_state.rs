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
use crate::core::models::{ClientKey, MonitorKey};
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
                .selected_monitor
                .is_none_or(|index| index < self.monitors.len())
            && self.clients.iter().all(|client| {
                client.xid != 0
                    && ids.insert(client.xid)
                    && client.client_fact.is_finite()
                    && (0.25..=4.0).contains(&client.client_fact)
                    && client.tags != 0
                    && client.monitor < self.monitors.len()
            })
            && self
                .monitors
                .iter()
                .enumerate()
                .all(|(monitor_index, monitor)| {
                    let mut ordered = HashSet::new();
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

    fn decode(bytes: &[u8], intent: &RestartIntent) -> Option<Self> {
        if bytes.len() > MAX_RESTART_PROPERTY_BYTES {
            return None;
        }
        let snapshot: Self = serde_json::from_slice(bytes).ok()?;
        (snapshot.intent == *intent && snapshot.validate()).then_some(snapshot)
    }
}

/// A unique panel identity wins over enumeration order. An ambiguous identity
/// never picks the first duplicate. Geometry is only a placement fallback and
/// never grants ownership of another output's layout settings.
fn exact_output(saved: &Output, live: &[Output], sources: &[Output]) -> Option<usize> {
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
            let xid =
                xid(backend, client.win).ok_or("X11 restart client has no native identity")?;
            clients.push(Client {
                xid,
                tags: client.state.tags,
                monitor,
                client_fact: client.state.client_fact,
            });
        }
        let captured: HashSet<_> = clients.iter().map(|client| client.xid).collect();
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
                })
            })
            .collect::<Result<_, Box<dyn Error>>>()?;
        let snapshot = RestartSnapshot {
            intent: intent.clone(),
            monitors,
            clients,
            selected_monitor: live
                .iter()
                .position(|(key, _)| Some(*key) == self.state.sel_mon),
        };
        let encoded = snapshot.encode()?;
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
                pertag.prev_tag = pertag.clamp_tag(saved.prev_tag);
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
                self.restart_state
                    .adopted
                    .contains(&id)
                    .then_some((id, *key))
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
        for saved in &snapshot.monitors {
            let Some(index) = exact_output(&saved.output, &outputs, &sources) else {
                continue;
            };
            let key = live[index].0;
            if let Some(selected) = saved
                .selected
                .and_then(|id| restored.get(&id))
                .copied()
                .filter(|selected| self.state.clients[*selected].mon == Some(key))
            {
                self.state.monitors[key].set_selected_client_for_current_tag(Some(selected));
                if let Some(stack) = self.state.monitor_stack.get_mut(key) {
                    stack.retain(|entry| *entry != selected);
                    stack.insert(0, selected);
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
            }],
            clients: vec![Client {
                xid: 42,
                tags: 0b101,
                monitor: 0,
                client_fact: 2.5,
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
