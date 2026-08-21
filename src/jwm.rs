pub mod client;
pub mod client_stack;
pub mod constraints;
pub mod event_dispatcher;
pub mod features;
pub mod focus;
pub mod focus_manager;
pub mod geometry;
pub mod input_handler;
pub mod ipc_handler;
pub mod layout;
pub mod lifecycle;
pub mod monitor;
pub mod mouse_handler;
pub mod navigation;
pub mod property_handler;
pub(crate) mod restart_preflight;
pub mod rules;
pub(crate) mod scratchpad_handoff;
pub(crate) mod scratchpad_pending;
pub mod session;
pub mod stacking;
pub mod statusbar;
pub mod strut_manager;
pub mod swallowing;
pub mod tag_manager;
pub mod types;
pub(crate) mod update_readiness;
pub mod visibility;

pub mod monitor_management;
pub mod positioning;
pub mod process;
pub mod rendering;
pub mod window_state;
pub mod window_tabs;
pub use types::{
    ICONIC_STATE, InteractionAction, InteractionState, MonitorIndex, NORMAL_STATE, STEXT_MAX_LEN,
    SecondaryBarInstance, WITHDRAWN_STATE, WMArgEnum, WMButton, WMClickType, WMFuncType, WMKey,
    WMRule, WMWindowGeom,
};

pub use features::{
    AudioRecordingState, FeatureStates, MagnifierState, OverviewState, RecordingState,
    ScreenshotState, SystemUiState,
};

pub use geometry::GeometryConstraints;
pub use rules::{RuleApplication, RuleMatcher};
pub use statusbar::{StatusBarBuilder, StatusBarUpdateManager};

use log::info;
use log::warn;
use log::{debug, error};

use crate::backend::common_define::WindowId;
use crate::core::state::WMState;
use slotmap::SecondaryMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::backend::api::Backend;
use crate::backend::api::CompositorRect;
use crate::backend::api::Geometry;
use crate::backend::api::StrutPartial;
use crate::backend::api::WindowAttributes;
use crate::backend::api::WindowChanges;
use crate::backend::api::WindowType;
use crate::backend::common_define::ArgbColor;
use crate::backend::common_define::ColorScheme;
use crate::backend::common_define::EventMaskBits;
use crate::backend::common_define::SchemeType;
use crate::backend::common_define::{KeySym, Mods};
use crate::backend::error::BackendError;
use crate::config::CONFIG;
use crate::core::layout::LayoutEnum;
use crate::core::models::{ClientKey, MonitorKey, ScrollingState, WMClient, WMMonitor};
use crate::ipc_server::IpcServer;
use crate::jwm::update_readiness::UpdateReadinessHub;

use crate::core::animation::AnimationManager;
use xbar_core::shared_structures::CommandType;
use xbar_core::shared_structures::SharedCommand;
use xbar_core::shared_structures::{
    MAX_MINIMIZED_WINDOWS, MonitorInfo, PREVIEW_MINIMIZED_FLAG_RENEWAL,
    PREVIEW_MINIMIZED_FLAG_VISIBLE, SharedMessage, TagStatus,
};

lazy_static::lazy_static! {
    pub static ref BUTTONMASK: EventMaskBits  = EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE;
    pub static ref MOUSEMASK: EventMaskBits   = EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE | EventMaskBits::POINTER_MOTION;
}

static WM_SESSION_ID: OnceLock<u64> = OnceLock::new();
static BAR_SNAPSHOT_GENERATION: AtomicU64 = AtomicU64::new(1);
const MAX_BAR_COMMANDS_PER_MONITOR_UPDATE: usize = 64;

/// A window id is intentionally only meaningful for one WM lifetime.  Mixing
/// a queued click from a previous JWM process with a recycled backend id could
/// otherwise restore an unrelated window after a restart.
fn wm_session_id() -> u64 {
    *WM_SESSION_ID.get_or_init(|| {
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let process = u64::from(std::process::id()).rotate_left(32);
        // Keep the opaque token exactly representable by JavaScript-backed
        // bars while retaining 53 bits of per-process entropy. Web frontends
        // must echo this value; rounding a general u64 would defeat the stale
        // action guard at the Rust boundary.
        const JS_SAFE_INTEGER_MASK: u64 = (1_u64 << 53) - 1;
        let id = (epoch ^ process ^ 0x4a57_4d44_4f43_4b31) & JS_SAFE_INTEGER_MASK;
        id.max(1)
    })
}

pub struct Jwm {
    // 纯状态数据
    pub state: WMState,

    /// Backend selected by the application composition root. This is captured
    /// from `ApplicationOptions`, rather than inferred from environment state.
    pub runtime_backend: String,
    pub started_at: std::time::Instant,

    pub s_w: i32,
    pub s_h: i32,
    pub running: AtomicBool,
    pub is_restarting: AtomicBool,
    pub last_mouse_root: (f64, f64),

    /// A pointer drag being watched: armed by a button press or a client
    /// `_NET_WM_MOVERESIZE` request, engaged once the pointer crosses the
    /// drag threshold. See [`mouse_handler::DragCtl`].
    pub(crate) drag_ctl: Option<mouse_handler::DragCtl>,

    pub message: SharedMessage,

    // Per-monitor status bars
    pub secondary_bars: HashMap<i32, SecondaryBarInstance>,
    pub secondary_bar_failures: HashMap<i32, u32>,
    pub secondary_bar_retry_after: HashMap<i32, std::time::Instant>,

    /// Fire-and-forget processes launched by JWM itself. Other subsystems keep
    /// exclusive ownership of their own `Child` handles.
    pub(crate) transient_children: process::TransientChildSupervisor,

    pub last_key_grab_refresh_at: Option<std::time::Instant>,

    pub pending_bar_updates: HashSet<MonitorIndex>,

    /// Ordered minimized projection and its current incarnation per monitor.
    /// Ordinary title/focus/tag snapshots retain the same generation, while
    /// membership, slot order, and restore-then-re-minimize allocate a new one.
    pub(crate) minimized_projection_epochs: HashMap<MonitorIndex, (Vec<(u64, u64)>, u64)>,

    /// Projection epoch whose non-addressable prefix has already had every
    /// compositor Dock target withdrawn. Web bars do not retain transport
    /// acknowledgements, so this WM-side guard makes the bounded newest-16
    /// contract authoritative without repeating teardown on every lease.
    pub(crate) reconciled_minimized_target_generations: HashMap<MonitorIndex, u64>,

    /// Last physical minimized-window shelf reported by each monitor's bar.
    /// This lets a new minimize receive the correct monitor-specific fallback
    /// before its own item exists in the next bar frame.
    pub(crate) minimized_dock_shelves: HashMap<MonitorIndex, CompositorRect>,

    /// Preview ownership is mirrored in the WM so a delayed LEAVE from one
    /// bar cannot dismiss a newer preview opened on another monitor.
    pub(crate) active_minimized_preview: Option<(MonitorIndex, WindowId)>,

    /// Exact minimized projection that owns `active_minimized_preview`.
    /// Keeping the epoch beside the existing monitor/window owner lets the WM
    /// retire a preview as soon as a rebuilt bar scene starts publishing,
    /// without letting an old LEAVE dismiss the same window's new incarnation.
    pub(crate) active_minimized_preview_generation: Option<u64>,

    pub suppress_mouse_focus_until: Option<std::time::Instant>,
    /// When true, resizeclient() skips layout animations (used during tag
    /// switch transitions so target windows appear instantly).
    pub suppress_layout_animation: bool,

    pub last_stacking: SecondaryMap<MonitorKey, Vec<WindowId>>,

    pub scratchpads: HashMap<String, ClientKey>,
    pub(crate) scratchpad_pending: scratchpad_pending::ScratchpadPendingRegistry,

    pub animations: AnimationManager,
    pub(crate) hidden_client_park_retries: monitor::HiddenClientParkRetries,

    key_bindings: Vec<WMKey>,

    /// Compiled chord (leader + second-key bindings). `None` when disabled.
    pub(crate) chord_compiled: Option<crate::config::CompiledChord>,
    /// Set when the leader fired and we're waiting for the second key.
    pub(crate) chord_armed_until: Option<std::time::Instant>,

    /// Do-not-disturb: when true, suppress urgent-window propagation and
    /// hide override-redirect notification surfaces. Initialized from
    /// `behavior.do_not_disturb` and toggled live via the `toggle_dnd` IPC.
    pub(crate) do_not_disturb: bool,

    /// Debug HUD on/off, toggled by `toggle_debug_hud` (default keybinding
    /// Alt+Shift+F12). Initialized from `behavior.debug_hud`.
    pub(crate) debug_hud_on: bool,

    /// Strut reservations from external panels (polybar, trayer, etc.).
    /// The second tuple element is the monitor that physically hosts the
    /// panel window, used to attribute legacy whole-screen (`_NET_WM_STRUT`)
    /// reservations to a single output instead of every monitor.
    external_struts: HashMap<WindowId, (StrutPartial, Option<MonitorKey>)>,

    // IPC
    pub ipc_server: Option<IpcServer>,
    pub(crate) update_readiness: Option<UpdateReadinessHub>,

    // Config hot-reload. Both backend inotify events and the backend-neutral
    // update-loop poll feed this tracker so a revision is only attempted once.
    pub(crate) config_reload_tracker: lifecycle::ConfigReloadTracker,
    /// Legacy observable value retained for source compatibility. Reload
    /// decisions are made by `config_reload_tracker`.
    pub config_last_modified: Option<std::time::SystemTime>,
    /// Legacy observable debounce timestamp retained for source compatibility.
    pub config_reload_debounce: Option<std::time::Instant>,
    pub config_reload_count: u64,
    pub config_reload_last_unix_ms: Option<u64>,
    pub config_reload_last_success: Option<bool>,
    pub config_reload_last_error: Option<String>,

    /// When a tag's layout last changed, while the write of it back to the
    /// config file is still waiting out its debounce. See
    /// [`crate::jwm::layout::persist`].
    pub(crate) layout_persist_dirty: Option<std::time::Instant>,

    /// Override-redirect windows (menus, tooltips, launchers, etc.) that are
    /// currently mapped.  These are not managed by the WM but must be rendered
    /// by the compositor when COMPOSITE_REDIRECT_MANUAL is active.
    pub override_redirect_windows: HashSet<WindowId>,

    /// Cached geometries for override-redirect windows.  Updated from
    /// ConfigureNotify so that `build_compositor_scene` doesn't need
    /// synchronous GetGeometry round-trips on every frame.
    pub or_window_geometries: HashMap<WindowId, (i32, i32, u32, u32)>,

    /// Per-monitor, per-active-tag scrolling layout state. This preserves
    /// columns, focused column/window, column widths, and viewport when moving
    /// between tags on the same monitor.
    pub scrolling_states: HashMap<(MonitorKey, u32), ScrollingState>,

    /// Night light: last time we updated color temperature
    pub last_night_light_update: Option<std::time::Instant>,
    /// User override for night light: `None` follows the configured
    /// schedule, `Some` forces it on or off until toggled back.
    pub(crate) night_light_override: Option<bool>,
    /// Last time the battery was re-read.
    pub(crate) last_battery_poll: Option<std::time::Instant>,
    /// Last time the session's idle clock was read.
    pub(crate) last_idle_poll: Option<std::time::Instant>,
    /// What the idle policy has already done this idle period.
    pub(crate) idle: crate::jwm::features::idle::IdleTracker,
    /// Caffeine: hold the session awake regardless of the idle clock, until
    /// toggled back. Not a config value — it is a decision about right now.
    pub(crate) idle_inhibited: bool,
    /// Set when a panel was rebuilt in memory and the compositor has not been
    /// told yet. Rebuilding and pushing are separate because the places that
    /// rebuild — a battery poll, an arriving notification, a connectivity
    /// re-read — do not all have a backend to push with.
    pub(crate) system_ui_dirty: bool,

    /// Whether the display server's competing blanking timer has been
    /// switched off. Done once, the first time the idle policy runs.
    pub(crate) server_saver_suppressed: bool,

    /// 所有特殊功能的状态（截图、overview、录制、放大镜等）
    pub features: FeatureStates,

    /// Event coalescer for reducing high-frequency updates
    pub event_coalescer: crate::backend::compositor_common::event_coalescer::EventCoalescer,

    /// _NET_WM_PING: pending pings awaiting pong response
    pub pending_pings: HashMap<WindowId, std::time::Instant>,
    /// Windows that failed to respond to ping within timeout
    pub unresponsive_windows: HashSet<WindowId>,
    /// Last time we sent pings to visible windows
    pub last_ping_time: Option<std::time::Instant>,
    /// Last user interaction timestamp (for _NET_WM_USER_TIME focus-steal prevention)
    pub last_user_activity_time: u32,
}

const INITIAL_WINDOW_QUERY_ATTEMPTS: usize = 2;

/// The deliberately small surface used while adopting X11 root children.
/// Keeping the retry protocol separate from `manage` makes it impossible for
/// one failed per-window request to be mistaken for a confirmed destroy race.
trait InitialWindowAdoptionQueries {
    fn scan_root_children(&mut self) -> Result<Vec<WindowId>, BackendError>;
    fn get_attributes(&mut self, window: WindowId) -> Result<WindowAttributes, BackendError>;
    fn get_wm_state(&mut self, window: WindowId) -> Result<i64, BackendError>;
    fn get_geometry(&mut self, window: WindowId) -> Result<Geometry, BackendError>;
}

struct BackendInitialWindowAdoptionQueries<'a> {
    backend: &'a mut dyn Backend,
}

impl InitialWindowAdoptionQueries for BackendInitialWindowAdoptionQueries<'_> {
    fn scan_root_children(&mut self) -> Result<Vec<WindowId>, BackendError> {
        self.backend.window_ops().scan_windows()
    }

    fn get_attributes(&mut self, window: WindowId) -> Result<WindowAttributes, BackendError> {
        self.backend.window_ops().get_window_attributes(window)
    }

    fn get_wm_state(&mut self, window: WindowId) -> Result<i64, BackendError> {
        self.backend.property_ops().get_wm_state(window)
    }

    fn get_geometry(&mut self, window: WindowId) -> Result<Geometry, BackendError> {
        self.backend.window_ops().get_geometry(window)
    }
}

#[derive(Debug)]
enum InitialWindowDisposition {
    /// The candidate was confirmed absent from a fresh root QueryTree.
    Gone,
    /// The candidate still exists but is not a managed toplevel.
    Ignore,
    /// The candidate is viewable or ICCCM Iconic and must be managed.
    Manage(Geometry),
}

fn scan_initial_window_candidates(
    queries: &mut dyn InitialWindowAdoptionQueries,
    supports_client_list: bool,
) -> Result<Vec<WindowId>, BackendError> {
    if !supports_client_list {
        return Ok(Vec::new());
    }
    queries.scan_root_children()
}

/// Retry one candidate query only after proving that the candidate is still a
/// root child. A failure followed by absence is the ordinary destroy race; a
/// failure while the XID remains present is an adoption failure and must stop
/// replacement startup rather than silently orphaning the window.
fn retry_initial_window_query<T>(
    queries: &mut dyn InitialWindowAdoptionQueries,
    window: WindowId,
    operation: &'static str,
    mut query: impl FnMut(&mut dyn InitialWindowAdoptionQueries, WindowId) -> Result<T, BackendError>,
) -> Result<Option<T>, BackendError> {
    for attempt in 1..=INITIAL_WINDOW_QUERY_ATTEMPTS {
        let query_error = match query(queries, window) {
            Ok(value) => return Ok(Some(value)),
            Err(error) => error,
        };

        let still_present = queries
            .scan_root_children()
            .map(|children| children.contains(&window))
            .map_err(|membership_error| {
                BackendError::Message(format!(
                    "startup adoption {operation} query for {window:?} failed ({query_error}); \
                     root membership recheck also failed: {membership_error}"
                ))
            })?;
        if !still_present {
            debug!(
                "startup adoption: {window:?} disappeared after {operation} query failed; \
                 treating it as a destroy race"
            );
            return Ok(None);
        }

        if attempt == INITIAL_WINDOW_QUERY_ATTEMPTS {
            return Err(BackendError::Message(format!(
                "startup adoption {operation} query for {window:?} failed after \
                 {INITIAL_WINDOW_QUERY_ATTEMPTS} attempts while it remained a root child: \
                 {query_error}"
            )));
        }
        debug!(
            "startup adoption: retrying {operation} query for {window:?} after failure \
             {attempt}/{INITIAL_WINDOW_QUERY_ATTEMPTS}: {query_error}"
        );
    }
    unreachable!("initial-window query loop always returns")
}

fn inspect_initial_window(
    queries: &mut dyn InitialWindowAdoptionQueries,
    window: WindowId,
) -> Result<InitialWindowDisposition, BackendError> {
    let Some(attributes) =
        retry_initial_window_query(queries, window, "attributes", |queries, window| {
            queries.get_attributes(window)
        })?
    else {
        return Ok(InitialWindowDisposition::Gone);
    };
    if attributes.override_redirect {
        return Ok(InitialWindowDisposition::Ignore);
    }

    let Some(wm_state) =
        retry_initial_window_query(queries, window, "WM_STATE", |queries, window| {
            queries.get_wm_state(window)
        })?
    else {
        return Ok(InitialWindowDisposition::Gone);
    };
    let iconic = crate::jwm::types::wm_state_is_minimized(wm_state);
    if !attributes.map_state_viewable && !iconic {
        return Ok(InitialWindowDisposition::Ignore);
    }

    let Some(geometry) =
        retry_initial_window_query(queries, window, "geometry", |queries, window| {
            queries.get_geometry(window)
        })?
    else {
        return Ok(InitialWindowDisposition::Gone);
    };
    Ok(InitialWindowDisposition::Manage(geometry))
}

#[cfg(test)]
mod initial_window_adoption_tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum QueryStage {
        Attributes,
        WmState,
        Geometry,
    }

    impl QueryStage {
        const ALL: [Self; 3] = [Self::Attributes, Self::WmState, Self::Geometry];

        const fn operation(self) -> &'static str {
            match self {
                Self::Attributes => "attributes",
                Self::WmState => "WM_STATE",
                Self::Geometry => "geometry",
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum RootMembership {
        Present,
        Gone,
    }

    struct FakeAdoptionQueries {
        failing_stage: Option<QueryStage>,
        failures_remaining: usize,
        root_membership: VecDeque<RootMembership>,
        root_scans: usize,
        attribute_queries: usize,
        wm_state_queries: usize,
        geometry_queries: usize,
    }

    impl FakeAdoptionQueries {
        fn failing(
            stage: QueryStage,
            failures: usize,
            root_membership: impl IntoIterator<Item = RootMembership>,
        ) -> Self {
            Self {
                failing_stage: Some(stage),
                failures_remaining: failures,
                root_membership: root_membership.into_iter().collect(),
                root_scans: 0,
                attribute_queries: 0,
                wm_state_queries: 0,
                geometry_queries: 0,
            }
        }

        fn idle() -> Self {
            Self {
                failing_stage: None,
                failures_remaining: 0,
                root_membership: VecDeque::new(),
                root_scans: 0,
                attribute_queries: 0,
                wm_state_queries: 0,
                geometry_queries: 0,
            }
        }

        fn fail_if_requested(&mut self, stage: QueryStage) -> Result<(), BackendError> {
            if self.failing_stage == Some(stage) && self.failures_remaining > 0 {
                self.failures_remaining -= 1;
                return Err(BackendError::Message(format!(
                    "injected {} query failure",
                    stage.operation()
                )));
            }
            Ok(())
        }

        const fn query_count(&self, stage: QueryStage) -> usize {
            match stage {
                QueryStage::Attributes => self.attribute_queries,
                QueryStage::WmState => self.wm_state_queries,
                QueryStage::Geometry => self.geometry_queries,
            }
        }
    }

    impl InitialWindowAdoptionQueries for FakeAdoptionQueries {
        fn scan_root_children(&mut self) -> Result<Vec<WindowId>, BackendError> {
            self.root_scans += 1;
            match self.root_membership.pop_front() {
                Some(RootMembership::Present) => Ok(vec![test_window()]),
                Some(RootMembership::Gone) => Ok(Vec::new()),
                None => Err(BackendError::Message(
                    "unexpected root membership query".into(),
                )),
            }
        }

        fn get_attributes(&mut self, _window: WindowId) -> Result<WindowAttributes, BackendError> {
            self.attribute_queries += 1;
            self.fail_if_requested(QueryStage::Attributes)?;
            Ok(WindowAttributes {
                override_redirect: false,
                map_state_viewable: false,
            })
        }

        fn get_wm_state(&mut self, _window: WindowId) -> Result<i64, BackendError> {
            self.wm_state_queries += 1;
            self.fail_if_requested(QueryStage::WmState)?;
            Ok(i64::from(ICONIC_STATE))
        }

        fn get_geometry(&mut self, _window: WindowId) -> Result<Geometry, BackendError> {
            self.geometry_queries += 1;
            self.fail_if_requested(QueryStage::Geometry)?;
            Ok(test_geometry())
        }
    }

    fn test_window() -> WindowId {
        WindowId::from_raw(0x5a01)
    }

    const fn test_geometry() -> Geometry {
        Geometry {
            x: 31,
            y: 47,
            w: 640,
            h: 480,
            border: 2,
        }
    }

    fn assert_manage(disposition: InitialWindowDisposition) {
        let InitialWindowDisposition::Manage(geometry) = disposition else {
            panic!("an Iconic root child must be adopted: {disposition:?}");
        };
        let expected = test_geometry();
        assert_eq!(
            (
                geometry.x,
                geometry.y,
                geometry.w,
                geometry.h,
                geometry.border
            ),
            (
                expected.x,
                expected.y,
                expected.w,
                expected.h,
                expected.border
            )
        );
    }

    #[test]
    fn transient_candidate_queries_retry_and_adopt_the_iconic_window() {
        for stage in QueryStage::ALL {
            let mut queries = FakeAdoptionQueries::failing(stage, 1, [RootMembership::Present]);

            let disposition = inspect_initial_window(&mut queries, test_window())
                .unwrap_or_else(|error| panic!("{} retry failed: {error}", stage.operation()));

            assert_manage(disposition);
            assert_eq!(queries.query_count(stage), 2, "stage={stage:?}");
            assert_eq!(queries.root_scans, 1, "stage={stage:?}");
        }
    }

    #[test]
    fn persistent_candidate_queries_fail_closed_while_the_window_remains_present() {
        for stage in QueryStage::ALL {
            let mut queries = FakeAdoptionQueries::failing(
                stage,
                INITIAL_WINDOW_QUERY_ATTEMPTS,
                [RootMembership::Present, RootMembership::Present],
            );

            let error = inspect_initial_window(&mut queries, test_window())
                .expect_err("a persistent query failure must cancel startup adoption");

            let message = error.to_string();
            assert!(message.contains(stage.operation()), "{message}");
            assert!(message.contains("failed after 2 attempts"), "{message}");
            assert!(message.contains("remained a root child"), "{message}");
            assert_eq!(queries.query_count(stage), 2, "stage={stage:?}");
            assert_eq!(queries.root_scans, 2, "stage={stage:?}");
        }
    }

    #[test]
    fn candidate_query_failure_skips_only_a_confirmed_destroy_race() {
        for stage in QueryStage::ALL {
            let mut queries = FakeAdoptionQueries::failing(stage, 1, [RootMembership::Gone]);

            let disposition =
                inspect_initial_window(&mut queries, test_window()).unwrap_or_else(|error| {
                    panic!("{} destroy race failed: {error}", stage.operation())
                });

            assert!(matches!(disposition, InitialWindowDisposition::Gone));
            assert_eq!(queries.query_count(stage), 1, "stage={stage:?}");
            assert_eq!(queries.root_scans, 1, "stage={stage:?}");
        }
    }

    #[test]
    fn backend_without_client_list_never_scans_root_children() {
        let mut queries = FakeAdoptionQueries::idle();

        let candidates = scan_initial_window_candidates(&mut queries, false)
            .expect("a Wayland-style backend has no root scan to fail");

        assert!(candidates.is_empty());
        assert_eq!(queries.root_scans, 0);
    }
}

impl Jwm {
    pub(crate) fn scrolling_state_key(&self, mon_key: MonitorKey) -> Option<(MonitorKey, u32)> {
        self.state
            .monitors
            .get(mon_key)
            .map(|monitor| (mon_key, monitor.get_active_tags()))
    }

    pub(crate) fn scrolling_state_for_monitor(
        &self,
        mon_key: MonitorKey,
    ) -> Option<&ScrollingState> {
        let key = self.scrolling_state_key(mon_key)?;
        self.scrolling_states.get(&key)
    }

    pub(crate) fn scrolling_state_for_monitor_mut(
        &mut self,
        mon_key: MonitorKey,
    ) -> Option<&mut ScrollingState> {
        let key = self.scrolling_state_key(mon_key)?;
        self.scrolling_states.get_mut(&key)
    }

    pub(crate) fn scrolling_state_for_monitor_mut_or_default(
        &mut self,
        mon_key: MonitorKey,
    ) -> Option<&mut ScrollingState> {
        let key = self.scrolling_state_key(mon_key)?;
        Some(
            self.scrolling_states
                .entry(key)
                .or_insert_with(ScrollingState::new),
        )
    }

    pub(crate) fn drop_scrolling_states_for_monitor(&mut self, mon_key: MonitorKey) -> usize {
        let before = self.scrolling_states.len();
        self.scrolling_states
            .retain(|(state_mon_key, _), _| *state_mon_key != mon_key);
        before.saturating_sub(self.scrolling_states.len())
    }

    fn enable_floating_keep_geometry(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };

        if let Some(client) = self.state.clients.get_mut(client_key) {
            if !client.state.is_floating {
                client.state.is_floating = true;
                // Floating started from a drag, so a later layout apply may
                // reclaim this client into the tiling grid.
                client.state.is_drag_floating = true;
                client.geometry.floating_x = client.geometry.x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
            }
        }

        self.reorder_client_in_monitor_groups(client_key);

        self.arrange(backend, Some(sel_mon_key));
        Ok(())
    }
    fn debug_drag_enabled() -> bool {
        std::env::var("JWM_DEBUG_DRAG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    /// Whether an action opens one of the modal system UI panels, and so is
    /// bound as a toggle: pressing the key again takes its panel back down.
    ///
    /// The lock screen is deliberately absent — a lock its own key could
    /// undo is not a lock — as is the layout picker, whose repeats step the
    /// film strip instead.
    pub(crate) fn opens_system_ui_panel(func: WMFuncType) -> bool {
        macro_rules! eq {
            ($f:path) => {
                std::ptr::fn_addr_eq(func, $f as WMFuncType)
            };
        }

        eq!(Jwm::app_launcher)
            || eq!(Jwm::control_center)
            || eq!(Jwm::notification_center)
            || eq!(Jwm::session_menu)
            || eq!(Jwm::calendar)
            || eq!(Jwm::wifi_picker)
            || eq!(Jwm::bluetooth_picker)
            || eq!(Jwm::clipboard_picker)
            || eq!(Jwm::wallpaper_picker)
            || eq!(Jwm::audio_output_picker)
            || eq!(Jwm::audio_input_picker)
            || eq!(Jwm::monitor_layout)
    }

    fn func_name(func: WMFuncType) -> &'static str {
        macro_rules! eq {
            ($f:path) => {
                std::ptr::fn_addr_eq(func, $f as WMFuncType)
            };
        }

        if eq!(Jwm::spawn) {
            "spawn"
        } else if eq!(Jwm::app_launcher) {
            "app_launcher"
        } else if eq!(Jwm::monitor_layout) {
            "monitor_layout"
        } else if eq!(Jwm::lock_screen) {
            "lock_screen"
        } else if eq!(Jwm::focusstack) {
            "focusstack"
        } else if eq!(Jwm::focusmon) {
            "focusmon"
        } else if eq!(Jwm::take_screenshot) {
            "take_screenshot"
        } else if eq!(Jwm::quit) {
            "quit"
        } else if eq!(Jwm::restart) {
            "restart"
        } else if eq!(Jwm::killclient) {
            "killclient"
        } else if eq!(Jwm::zoom) {
            "zoom"
        } else if eq!(Jwm::setlayout) {
            "setlayout"
        } else if eq!(Jwm::lastlayout) {
            "lastlayout"
        } else if eq!(Jwm::layout_picker) {
            "layout_picker"
        } else if eq!(Jwm::togglefloating) {
            "togglefloating"
        } else if eq!(Jwm::togglebar) {
            "togglebar"
        } else if eq!(Jwm::setmfact) {
            "setmfact"
        } else if eq!(Jwm::setcfact) {
            "setcfact"
        } else if eq!(Jwm::incnmaster) {
            "incnmaster"
        } else if eq!(Jwm::movestack) {
            "movestack"
        } else if eq!(Jwm::scrolling_toggle_attach_mode) {
            "scrolling_toggle_attach_mode"
        } else if eq!(Jwm::view) {
            "view"
        } else if eq!(Jwm::tag) {
            "tag"
        } else if eq!(Jwm::toggleview) {
            "toggleview"
        } else if eq!(Jwm::toggletag) {
            "toggletag"
        } else if eq!(Jwm::tagmon) {
            "tagmon"
        } else if eq!(Jwm::loopview) {
            "loopview"
        } else if eq!(Jwm::movemouse) {
            "movemouse"
        } else if eq!(Jwm::resizemouse) {
            "resizemouse"
        } else if eq!(Jwm::togglesticky) {
            "togglesticky"
        } else if eq!(Jwm::togglescratchpad) {
            "togglescratchpad"
        } else if eq!(Jwm::togglepip) {
            "togglepip"
        } else if eq!(Jwm::toggle_overview) {
            "toggle_overview"
        } else if eq!(Jwm::cycle_overview) {
            "cycle_overview"
        } else if eq!(Jwm::toggle_magnifier) {
            "toggle_magnifier"
        } else if eq!(Jwm::toggle_peek) {
            "toggle_peek"
        } else if eq!(Jwm::toggle_annotation) {
            "toggle_annotation"
        } else if eq!(Jwm::toggle_waterlily) {
            "toggle_waterlily"
        } else if eq!(Jwm::waterlily_case) {
            "waterlily_case"
        } else if eq!(Jwm::save_session) {
            "save_session"
        } else if eq!(Jwm::restore_session) {
            "restore_session"
        } else if eq!(Jwm::toggle_expose) {
            "toggle_expose"
        } else if eq!(Jwm::toggle_recording) {
            "toggle_recording"
        } else if eq!(Jwm::adjust_recording_region) {
            "adjust_recording_region"
        } else if eq!(Jwm::toggle_audio_recording) {
            "toggle_audio_recording"
        } else {
            "<unknown>"
        }
    }

    fn maybe_clamp_override_redirect_notification(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
    ) {
        let attr = match backend.window_ops().get_window_attributes(win) {
            Ok(a) => a,
            Err(_) => return,
        };
        if !attr.override_redirect {
            return;
        }

        // Avoid meddling with regular menus/tooltips unless we're confident.
        let types = backend.property_ops().get_window_types(win);
        let (inst, cls) = backend.property_ops().get_class(win);
        let title = backend.property_ops().get_title(win);

        let is_dunst = title == "Dunst"
            || inst.eq_ignore_ascii_case("dunst")
            || cls.eq_ignore_ascii_case("dunst")
            || inst.eq_ignore_ascii_case("dunstify")
            || cls.eq_ignore_ascii_case("dunstify");
        let is_notification = types.contains(&WindowType::Notification) || is_dunst;
        if !is_notification {
            return;
        }

        // Do-not-disturb: suppress notification windows entirely.
        if self.do_not_disturb {
            if let Err(e) = backend.window_ops().unmap_window(win) {
                debug!("DND: unmap notification {:?} failed: {:?}", win, e);
            }
            return;
        }

        let geom = match backend.window_ops().get_geometry(win) {
            Ok(g) => g,
            Err(_) => return,
        };

        // Find the monitor by window center (fallback to selected monitor).
        let cx = geom.x.saturating_add((geom.w as i32) / 2);
        let cy = geom.y.saturating_add((geom.h as i32) / 2);
        let mon_key = self.recttomon(backend, cx, cy).or(self.state.sel_mon);
        let Some(mon_key) = mon_key else {
            return;
        };

        // Skip windows that cover most of the monitor (e.g. screenshot overlays
        // like Feishu/Lark that set _NET_WM_WINDOW_TYPE_NOTIFICATION).
        // Real notifications are small; full-screen overlays must not be clamped.
        // Compare against the monitor size, not the virtual screen, so that
        // per-monitor overlays in multi-monitor setups are correctly skipped.
        let (mon_w, mon_h) = self
            .state
            .monitors
            .get(mon_key)
            .map(|m| (m.geometry.m_w as u32, m.geometry.m_h as u32))
            .unwrap_or((self.s_w as u32, self.s_h as u32));
        if geom.w >= mon_w.saturating_sub(4) && geom.h >= mon_h.saturating_sub(4) {
            return;
        }

        let work = match self.monitor_work_area(mon_key) {
            Some(r) => r,
            None => return,
        };

        let w = geom.w as i32;
        let h = geom.h as i32;
        let mut new_x = geom.x;
        let mut new_y = geom.y;

        // Clamp to workarea bounds.
        let min_x = work.x;
        let max_x = work.x + work.w - w;
        new_x = if min_x <= max_x {
            new_x.clamp(min_x, max_x)
        } else {
            min_x
        };

        let min_y = work.y;
        let max_y = work.y + work.h - h;
        new_y = if min_y <= max_y {
            new_y.clamp(min_y, max_y)
        } else {
            min_y
        };

        if new_x == geom.x && new_y == geom.y {
            return;
        }

        let changes = WindowChanges {
            x: Some(new_x),
            y: Some(new_y),
            ..Default::default()
        };
        if let Err(e) = backend.window_ops().apply_window_changes(win, changes) {
            debug!(
                "Failed to clamp override_redirect notification win={:?}: {:?}",
                win, e
            );
        }
    }

    pub fn new(backend: &mut dyn Backend) -> Result<Self, Box<dyn std::error::Error>> {
        // Preserve the public constructor's historical reporting contract for
        // library callers. The application composition root uses the explicit
        // constructor below and therefore never needs this compatibility
        // inference.
        let runtime_backend = std::env::var("JWM_BACKEND").unwrap_or_else(|_| "x11rb".to_string());
        Self::new_with_runtime_backend(backend, runtime_backend)
    }

    pub(crate) fn new_with_runtime_backend(
        backend: &mut dyn Backend,
        runtime_backend: impl Into<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        info!("[new] Starting JWM initialization");
        Self::log_x11_environment();
        backend.cursor_provider().preload_common()?;
        let si = backend.output_ops().screen_info();
        let s_w = si.width;
        let s_h = si.height;
        info!(
            "[new] Screen info - resolution: {}x{}, root: {:?}",
            s_w,
            s_h,
            backend.root_window()
        );
        let alloc = backend.color_allocator();
        let colors = crate::config::CONFIG.load().colors().clone();
        alloc.set_scheme(
            SchemeType::Norm,
            ColorScheme::new(
                ArgbColor::from_hex(&colors.dark_sea_green1, colors.opaque)?,
                ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque)?,
                ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque)?,
            ),
        );
        alloc.set_scheme(
            SchemeType::Sel,
            ColorScheme::new(
                ArgbColor::from_hex(&colors.dark_sea_green2, colors.opaque)?,
                ArgbColor::from_hex(&colors.pale_turquoise1, colors.opaque)?,
                ArgbColor::from_hex(&colors.cyan, colors.opaque)?,
            ),
        );
        backend.color_allocator().allocate_schemes_pixels()?;
        info!("[new] JWM initialization completed successfully");
        let outputs = backend.output_ops().enumerate_outputs();
        let config_revision = crate::config::Config::get_config_modified_time().ok();
        let runtime_backend: String = runtime_backend.into();
        // IPC 失败不阻止启动，但日志必须带上后端 / 边界标记以便支持诊断。
        let ipc_server = match IpcServer::new() {
            Ok(s) => Some(s),
            Err(e) => {
                let error = crate::backend::error::BackendError::from(e).with_context(
                    crate::backend::error::BackendErrorContext::new(
                        runtime_backend.clone(),
                        crate::backend::error::ErrorBoundary::Ipc,
                        "bind control socket",
                    ),
                );
                warn!("failed to start IPC server: {error}");
                None
            }
        };
        let update_readiness = match UpdateReadinessHub::new() {
            Ok(hub) => Some(hub),
            Err(error) => {
                warn!("failed to create update readiness hub; retaining timer fallback: {error}");
                None
            }
        };
        if let (Some(hub), Some(ipc_fd)) = (
            update_readiness.as_ref(),
            ipc_server.as_ref().and_then(IpcServer::readiness_fd),
        ) && let Err(error) = hub.register(ipc_fd)
        {
            warn!("failed to aggregate IPC readiness; retaining timer fallback: {error}");
        }
        let mut jwm = Jwm {
            state: WMState::new(),
            runtime_backend,
            started_at: std::time::Instant::now(),

            s_w,
            s_h,
            running: AtomicBool::new(true),
            is_restarting: AtomicBool::new(false),

            message: SharedMessage::default(),

            secondary_bars: HashMap::new(),
            secondary_bar_failures: HashMap::new(),
            secondary_bar_retry_after: HashMap::new(),
            transient_children: process::TransientChildSupervisor::default(),

            last_key_grab_refresh_at: None,
            pending_bar_updates: HashSet::new(),
            minimized_projection_epochs: HashMap::new(),
            reconciled_minimized_target_generations: HashMap::new(),
            minimized_dock_shelves: HashMap::new(),
            active_minimized_preview: None,
            active_minimized_preview_generation: None,

            suppress_mouse_focus_until: None,
            suppress_layout_animation: false,

            last_stacking: SecondaryMap::new(),
            scratchpads: HashMap::new(),
            scratchpad_pending: scratchpad_pending::ScratchpadPendingRegistry::default(),
            animations: AnimationManager::new(),
            hidden_client_park_retries: monitor::HiddenClientParkRetries::default(),
            key_bindings: CONFIG.load().get_keys(),
            chord_compiled: CONFIG.load().compile_chord(),
            chord_armed_until: None,
            do_not_disturb: CONFIG.load().behavior().do_not_disturb,
            debug_hud_on: CONFIG.load().behavior().debug_hud,
            external_struts: HashMap::new(),
            last_mouse_root: (0.0, 0.0),
            drag_ctl: None,

            ipc_server,
            update_readiness,
            config_reload_tracker: lifecycle::ConfigReloadTracker::new(config_revision),
            config_last_modified: config_revision,
            config_reload_debounce: None,
            config_reload_count: 0,
            config_reload_last_unix_ms: None,
            config_reload_last_success: None,
            config_reload_last_error: None,
            layout_persist_dirty: None,
            override_redirect_windows: HashSet::new(),
            or_window_geometries: HashMap::new(),
            scrolling_states: HashMap::new(),
            last_night_light_update: None,
            night_light_override: None,
            last_battery_poll: None,
            last_idle_poll: None,
            idle: crate::jwm::features::idle::IdleTracker::default(),
            idle_inhibited: false,
            system_ui_dirty: false,
            server_saver_suppressed: false,
            features: FeatureStates::new(),
            event_coalescer:
                crate::backend::compositor_common::event_coalescer::EventCoalescer::new(),
            pending_pings: HashMap::new(),
            unresponsive_windows: HashSet::new(),
            last_ping_time: None,
            last_user_activity_time: 0,
        };
        if let Ok((x, y)) = backend.input_ops().get_pointer_position() {
            jwm.last_mouse_root = (x, y);
        }
        for out in outputs {
            jwm.add_monitor(out);
        }
        if !jwm.state.monitor_order.is_empty() {
            jwm.state.sel_mon = Some(jwm.state.monitor_order[0]);
        }
        Ok(jwm)
    }

    // --- 热插拔处理逻辑 ---

    pub fn setup_initial_windows(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Root-child adoption is an X11 capability. In particular, do not
        // turn a failed QueryTree into a successful zero-client restart: the
        // old process may just have left true-Iconic clients unmapped for this
        // exact scan to recover.
        let supports_client_list = backend.capabilities().supports_client_list;
        let windows = {
            let mut queries = BackendInitialWindowAdoptionQueries { backend };
            scan_initial_window_candidates(&mut queries, supports_client_list)?
        };
        if !supports_client_list {
            return Ok(());
        }
        info!("[setup_initial_windows] Scanning {} windows", windows.len());
        for win in windows {
            let disposition = {
                let mut queries = BackendInitialWindowAdoptionQueries { backend };
                inspect_initial_window(&mut queries, win)?
            };
            if let InitialWindowDisposition::Manage(geometry) = disposition {
                self.manage(backend, win, &geometry)?;
            }
        }
        Ok(())
    }

    fn clean_mask(&self, backend: &mut dyn Backend, raw: u16) -> Mods {
        let mods_all = backend.key_ops().clean_mods(raw);

        mods_all
            & (Mods::SHIFT
                | Mods::CONTROL
                | Mods::ALT
                | Mods::SUPER
                | Mods::MOD2
                | Mods::MOD3
                | Mods::MOD5)
    }

    fn target_to_monitor(
        &mut self,
        backend: &mut dyn Backend,
        target: crate::backend::api::HitTarget,
        fallback_pos: (i32, i32),
    ) -> Option<MonitorKey> {
        use crate::backend::api::HitTarget;

        match target {
            HitTarget::Background { output: Some(oid) } => {
                // 直接用 output_map 找 monitor
                for (mon_key, &mapped_oid) in &self.state.output_map {
                    if mapped_oid == oid {
                        return Some(mon_key);
                    }
                }
                self.state.sel_mon
            }
            HitTarget::Background { output: None } => {
                // fallback：用坐标查
                self.recttomon(backend, fallback_pos.0, fallback_pos.1)
            }
            HitTarget::Surface(win) => {
                // 还是按原逻辑：先看 client.mon，否则用 pointer 落点
                if let Some(ck) = self.wintoclient(win) {
                    if let Some(c) = self.state.clients.get(ck) {
                        return c.mon.or(self.state.sel_mon);
                    }
                }
                self.recttomon(backend, fallback_pos.0, fallback_pos.1)
            }
        }
    }

    fn insert_client(&mut self, client: WMClient) -> ClientKey {
        let win = client.win;
        let key = self.state.clients.insert(client);
        self.state.client_order.push(key);
        self.state.win_to_client.insert(win, key);
        key
    }

    fn insert_monitor(&mut self, monitor: WMMonitor) -> MonitorKey {
        let key = self.state.monitors.insert(monitor);
        self.state.monitor_order.push(key);
        self.state.monitor_clients.insert(key, Vec::new());
        self.state.monitor_stack.insert(key, Vec::new());
        key
    }

    fn is_client_selected(&self, client_key: ClientKey) -> bool {
        self.state
            .sel_mon
            .and_then(|sel_mon_key| self.state.monitors.get(sel_mon_key))
            .and_then(|monitor| monitor.sel)
            .map(|sel_client| sel_client == client_key)
            .unwrap_or(false)
    }

    fn get_monitor_clients(&self, mon_key: MonitorKey) -> &[ClientKey] {
        self.state
            .monitor_clients
            .get(mon_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn get_selected_client_key(&self) -> Option<ClientKey> {
        self.state
            .sel_mon
            .and_then(|sel_mon_key| self.state.monitors.get(sel_mon_key))
            .and_then(|monitor| monitor.sel)
    }

    fn find_next_visible_client_by_mon(&self, mon_key: MonitorKey) -> Option<ClientKey> {
        if let Some(stack_list) = self.state.monitor_stack.get(mon_key) {
            for &client_key in stack_list {
                if let Some(_) = self.state.clients.get(client_key) {
                    if self.is_client_visible_on_monitor(client_key, mon_key) {
                        return Some(client_key);
                    }
                }
            }
        }
        None
    }

    fn is_client_visible_on_monitor(&self, client_key: ClientKey, mon_key: MonitorKey) -> bool {
        if let (Some(client), Some(monitor)) = (
            self.state.clients.get(client_key),
            self.state.monitors.get(mon_key),
        ) {
            if client.state.is_swallowed || client.state.is_hidden {
                return false;
            }
            client.state.is_sticky || (client.state.tags & monitor.get_active_tags()) > 0
        } else {
            false
        }
    }

    fn is_client_visible_by_key(&self, client_key: ClientKey) -> bool {
        if let Some(client) = self.state.clients.get(client_key) {
            if client.state.is_swallowed || client.state.is_hidden {
                return false;
            }
            if let Some(mon_key) = client.mon {
                if let Some(monitor) = self.state.monitors.get(mon_key) {
                    return client.state.is_sticky
                        || (client.state.tags & monitor.get_active_tags()) > 0;
                }
            }
        }

        false
    }

    fn should_animate_tag_switch(&self, mon_key: MonitorKey, old_mask: u32, new_mask: u32) -> bool {
        let Some(client_keys) = self.state.monitor_clients.get(mon_key) else {
            return false;
        };

        let mut has_membership_change = false;

        for client_key in client_keys.iter().copied() {
            let Some(client) = self.state.clients.get(client_key) else {
                continue;
            };

            if client.state.is_sticky {
                continue;
            }

            let old_visible = (client.state.tags & old_mask) > 0;
            let new_visible = (client.state.tags & new_mask) > 0;

            if old_visible != new_visible {
                has_membership_change = true;
                break;
            }
        }

        // Animate whenever visible window membership changes between old and
        // new tags. This includes switching to/from empty tags (wallpaper-only).
        // When both tags are empty, has_membership_change is false so we skip.
        has_membership_change
    }

    /// Return the number of pixels at the top of the monitor to exclude from
    /// the tag-switch transition. Use the monitor workarea so compositor
    /// transitions respect any top-reserved space, whether it comes from the
    /// built-in bar, secondary bars, or external panels via struts.
    fn tag_transition_exclude_top(&self, mon_key: MonitorKey) -> u32 {
        let Some(monitor) = self.state.monitors.get(mon_key) else {
            return 0;
        };

        let monitor_top = monitor.geometry.m_y;
        let workarea_top = self
            .monitor_work_area(mon_key)
            .map(|rect| rect.y)
            .unwrap_or(monitor.geometry.w_y);

        (workarea_top - monitor_top).max(0) as u32
    }

    /// Return the (x, y, w, h) rect of the given monitor for compositor transitions.
    fn monitor_rect(&self, mon_key: MonitorKey) -> (i32, i32, u32, u32) {
        if let Some(mon) = self.state.monitors.get(mon_key) {
            let g = &mon.geometry;
            (g.m_x, g.m_y, g.m_w.max(1) as u32, g.m_h.max(1) as u32)
        } else {
            (0, 0, 1, 1)
        }
    }

    fn wintoclient(&self, win: WindowId) -> Option<ClientKey> {
        self.state.win_to_client.get(&win).copied()
    }

    fn log_x11_environment() {
        info!("[X11 Environment Debug]");
        info!("DISPLAY: {:?}", env::var("DISPLAY"));
        info!("XAUTHORITY: {:?}", env::var("XAUTHORITY"));
        info!("XDG_SESSION_TYPE: {:?}", env::var("XDG_SESSION_TYPE"));
        info!("USER: {:?}", env::var("USER"));
        info!("HOME: {:?}", env::var("HOME"));

        if let Ok(display) = env::var("DISPLAY") {
            let socket_path = format!("/tmp/.X11-unix/X{}", display.trim_start_matches(":"));
            info!("X11 socket path: {}", socket_path);
            info!(
                "X11 socket exists: {}",
                std::path::Path::new(&socket_path).exists()
            );
        }

        let x_running = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("X|Xorg")
            .output()
            .map(|output| !output.stdout.is_empty())
            .unwrap_or(false);
        info!("X server running: {}", x_running);
    }

    pub fn restart(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[restart] Preparing seamless restart");
        self.running.store(false, Ordering::SeqCst);
        self.is_restarting.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn is_bar_visible_on_mon(&self, mon_key: MonitorKey) -> bool {
        if let Some(m) = self.state.monitors.get(mon_key) {
            if let Some(p) = m.pertag.as_ref() {
                if let Some(&show) = p.show_bars.get(p.cur_tag) {
                    return show;
                }
            }
        }
        true
    }
    fn mark_bar_update_needed_if_visible(&mut self, monitor_id: Option<i32>) {
        match monitor_id {
            Some(id) => {
                if let Some(mon_key) = self.get_monitor_by_id(id) {
                    if self.is_bar_visible_on_mon(mon_key) {
                        self.pending_bar_updates.insert(id);
                    }
                }
            }
            None => {
                for (key, m) in self.state.monitors.iter() {
                    if self.is_bar_visible_on_mon(key) {
                        self.pending_bar_updates.insert(m.num);
                    }
                }
            }
        }
    }

    fn clear_minimized_preview_for(
        &mut self,
        backend: &mut dyn Backend,
        monitor_id: MonitorIndex,
        window: Option<WindowId>,
    ) {
        let owns_preview =
            minimized_preview_owned_by(self.active_minimized_preview, monitor_id, window);
        if owns_preview {
            backend.compositor_set_minimized_window_preview(None, None);
            self.active_minimized_preview = None;
            self.active_minimized_preview_generation = None;
        }
    }

    /// Withdraw every compositor overlay whose input surface was this bar.
    /// The minimized textures stay cached and will be retargeted if the bar
    /// returns, but nothing is painted over an absent/hidden bar meanwhile.
    fn clear_minimized_dock_for_monitor(
        &mut self,
        backend: &mut dyn Backend,
        monitor_id: MonitorIndex,
    ) {
        self.clear_minimized_preview_for(backend, monitor_id, None);
        self.minimized_dock_shelves.remove(&monitor_id);
        let hidden_windows: Vec<_> = self
            .get_monitor_by_id(monitor_id)
            .and_then(|monitor| self.state.monitor_clients.get(monitor))
            .into_iter()
            .flatten()
            .filter_map(|key| self.state.clients.get(*key))
            .filter(|client| client.state.is_hidden)
            .map(|client| client.win)
            .collect();
        for window in hidden_windows {
            backend.compositor_set_window_dock_geometry(window, None);
        }
    }

    fn get_wm_class(
        &self,
        backend: &mut dyn Backend,
        window: WindowId,
    ) -> Option<(String, String)> {
        let (inst, cls) = backend.property_ops().get_class(window);
        if inst.is_empty() && cls.is_empty() {
            None
        } else {
            Some((inst, cls))
        }
    }

    fn reset_input_focus(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        backend.window_ops().set_input_focus_root()?;
        Ok(())
    }

    fn configurenotify(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if window == backend.root_window().expect("no root window") {
            let dirty = self.s_w != w as i32 || self.s_h != h as i32;
            self.s_w = w as i32;
            self.s_h = h as i32;
            if self.updategeom(backend) || dirty {
                self.handle_screen_geometry_change(backend)?;
            }
        }

        // For Wayland layer-shell (and other backend-driven docks), the compositor controls
        // the final geometry. Reflect it in our model and re-arrange so workareas update.
        if let Some(client_key) = self.wintoclient(window) {
            let layer_info = backend.property_ops().get_layer_surface_info(window);
            let is_likely_dock = self
                .state
                .clients
                .get(client_key)
                .map(|c| c.state.is_dock)
                .unwrap_or(false)
                || layer_info.is_some();

            if is_likely_dock {
                if let Some(c) = self.state.clients.get(client_key) {
                    info!(
                        "[dock_configure_notify] win={:?} event={}x{}+{}+{} current={}x{}+{}+{}",
                        window, w, h, x, y, c.geometry.w, c.geometry.h, c.geometry.x, c.geometry.y
                    );
                }

                let geometry_changed = self
                    .state
                    .clients
                    .get(client_key)
                    .map(|c| {
                        c.geometry.x != x
                            || c.geometry.y != y
                            || c.geometry.w != w as i32
                            || c.geometry.h != h as i32
                    })
                    .unwrap_or(true);

                if !geometry_changed {
                    return Ok(());
                }

                // Check if this is a status bar being moved back to origin by GTK
                // If so, skip the update to prevent feedback loop with arrange
                let is_status_bar_reset = self
                    .state
                    .clients
                    .get(client_key)
                    .map(|c| c.state.is_dock && x == 0 && y == 0 && c.geometry.x != 0)
                    .unwrap_or(false);

                if is_status_bar_reset {
                    // Status bar trying to reset to (0,0), ignore this configure notify
                    // to prevent feedback loop with arrange repositioning it
                    return Ok(());
                }

                if let Some(c) = self.state.clients.get_mut(client_key) {
                    c.geometry.x = x;
                    c.geometry.y = y;
                    c.geometry.w = w as i32;
                    c.geometry.h = h as i32;
                }

                // Refresh type/layer metadata so exclusive_zone changes are honored.
                self.updatewindowtype(backend, client_key);

                if let Some(mon_key) = self.state.clients.get(client_key).and_then(|c| c.mon) {
                    self.arrange(backend, Some(mon_key));
                } else {
                    self.arrange(backend, None);
                }
            }
        }

        Ok(())
    }

    fn handle_screen_geometry_change(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[handle_screen_geometry_change]");
        let monitors: Vec<_> = self.state.monitor_order.to_vec();
        for mon_key in monitors {
            self.update_fullscreen_clients_on_monitor(backend, mon_key)?;
        }
        self.focus(backend, None)?;
        self.arrange(backend, None);
        Ok(())
    }

    fn update_fullscreen_clients_on_monitor(
        &mut self,
        backend: &mut dyn Backend,
        mon_key: MonitorKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let monitor_geometry = if let Some(monitor) = self.state.monitors.get(mon_key) {
            (
                monitor.geometry.m_x,
                monitor.geometry.m_y,
                monitor.geometry.m_w,
                monitor.geometry.m_h,
            )
        } else {
            warn!(
                "[update_fullscreen_clients_on_monitor] Monitor {:?} not found",
                mon_key
            );
            return Ok(());
        };

        let fullscreen_clients: Vec<ClientKey> =
            if let Some(client_keys) = self.state.monitor_clients.get(mon_key) {
                client_keys
                    .iter()
                    .filter(|&&client_key| {
                        self.state
                            .clients
                            .get(client_key)
                            .map(|client| client.state.is_fullscreen)
                            .unwrap_or(false)
                    })
                    .copied()
                    .collect()
            } else {
                Vec::new()
            };

        for client_key in fullscreen_clients {
            let _ = self.resizeclient(
                backend,
                client_key,
                monitor_geometry.0,
                monitor_geometry.1,
                monitor_geometry.2,
                monitor_geometry.3,
            );
        }
        Ok(())
    }

    fn grabkeys(&mut self, backend: &mut dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
        let root_window = backend.root_window().expect("no root window");
        backend.key_ops().clear_key_grabs(root_window)?;
        let mut bindings: Vec<(Mods, KeySym)> = self
            .key_bindings
            .iter()
            .map(|k| (k.mask, k.key_sym))
            .collect();
        // Also grab the chord leader so the WM (not the focused client) sees it.
        // Second-key bindings inside the chord are handled via grab_keyboard
        // after the leader fires, so they don't need to be globally grabbed.
        if let Some(chord) = &self.chord_compiled {
            if !bindings.contains(&chord.leader) {
                bindings.push(chord.leader);
            }
        }
        backend.key_ops().grab_keys(root_window, &bindings)?;
        Ok(())
    }

    fn enter_notify(
        &mut self,
        backend: &mut dyn Backend,
        event_window: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.handle_statusbar_enter_generic(backend, event_window)? {
            return Ok(());
        }
        self.handle_regular_enter_generic(backend, event_window)?;
        Ok(())
    }

    fn handle_statusbar_enter_generic(
        &mut self,
        _backend: &mut dyn Backend,
        _event_window: WindowId,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(false)
    }

    fn handle_regular_enter_generic(
        &mut self,
        backend: &mut dyn Backend,
        event_window: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.mouse_focus_blocked() {
            return Ok(());
        }
        let client_key_opt = self.wintoclient(event_window);
        let monitor_key_opt = if let Some(client_key) = client_key_opt {
            self.state
                .clients
                .get(client_key)
                .and_then(|client| client.mon)
        } else {
            self.wintomon(backend, Some(event_window))
        };
        let current_event_monitor_key = match monitor_key_opt {
            Some(monitor_key) => monitor_key,
            None => return Ok(()),
        };
        let is_on_selected_monitor = Some(current_event_monitor_key) == self.state.sel_mon;
        if !is_on_selected_monitor {
            self.switch_to_monitor(backend, current_event_monitor_key)?;
        }
        if self.should_focus_client(client_key_opt, is_on_selected_monitor) {
            self.focus(backend, client_key_opt)?;
        }
        Ok(())
    }

    fn destroynotify(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Clean up external struts for override-redirect windows (e.g. polybar)
        // that are never managed but may have set strut properties.
        if self.external_struts.remove(&window).is_some() {
            info!("[strut] Removed strut on destroy for {:?}", window);
            self.apply_strut_reservations();
            self.arrange(backend, None);
        }
        let c = self.wintoclient(window);
        if c.is_some() {
            self.unmanage(backend, c, true)?;
        }
        Ok(())
    }

    pub fn run(&mut self, backend: &mut dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
        info!("[run] Handing over control to backend");
        Ok(backend.run(self)?)
    }

    fn process_commands_from_status_bar(&mut self, backend: &mut dyn Backend) {
        let mut commands_to_process: Vec<(i32, SharedCommand)> = Vec::new();
        let mut dock_generation_cache: HashMap<MonitorIndex, Option<u64>> = HashMap::new();
        let mut reconciled_preview_generations = HashSet::new();

        // Read commands from all per-monitor status bars
        for (&source_monitor, bar) in &mut self.secondary_bars {
            // Clear the outward level before consuming the ring. A command
            // arriving after this point either joins this drain or causes the
            // futex worker to publish a fresh eventfd level.
            if let Some(notifier) = bar.command_notifier.as_ref()
                && let Err(error) = notifier.drain()
            {
                warn!("[process_commands] failed to drain bar {source_monitor} notifier: {error}");
            }
            // Bound one producer's work. If it refills continuously and a
            // command remains, the level worker republishes the next batch.
            for _ in 0..MAX_BAR_COMMANDS_PER_MONITOR_UPDATE {
                match bar.shmem.try_receive_command() {
                    Ok(Some(cmd)) => commands_to_process.push((source_monitor, cmd)),
                    Ok(None) => break,
                    Err(e) => {
                        warn!("[process_commands] failed to receive bar command: {}", e);
                        break;
                    }
                }
            }
        }

        // Process all collected commands
        for (source_monitor, cmd) in commands_to_process {
            match cmd.cmd_type.into() {
                CommandType::ViewTag => {
                    if !self.select_status_bar_control_monitor(
                        backend,
                        status_bar_control_monitor(source_monitor, &cmd),
                    ) {
                        continue;
                    }
                    info!(
                        "[process_commands] ViewTag command received: {}",
                        cmd.parameter
                    );
                    let arg = WMArgEnum::UInt(cmd.parameter);
                    let _ = self.view(backend, &arg);
                }
                CommandType::ToggleTag => {
                    if !self.select_status_bar_control_monitor(
                        backend,
                        status_bar_control_monitor(source_monitor, &cmd),
                    ) {
                        continue;
                    }
                    info!(
                        "[process_commands] ToggleTag command received: {}",
                        cmd.parameter
                    );
                    let arg = WMArgEnum::UInt(cmd.parameter);
                    let _ = self.toggletag(backend, &arg);
                }
                CommandType::SetLayout => {
                    if !self.select_status_bar_control_monitor(
                        backend,
                        status_bar_control_monitor(source_monitor, &cmd),
                    ) {
                        continue;
                    }
                    info!(
                        "[process_commands] SetLayout command received: {}",
                        cmd.parameter
                    );
                    let arg = WMArgEnum::Layout(Rc::new(LayoutEnum::from(cmd.parameter)));
                    let _ = self.setlayout(backend, &arg);
                }
                CommandType::ShellHub => {
                    if !self.select_status_bar_control_monitor(
                        backend,
                        status_bar_control_monitor(source_monitor, &cmd),
                    ) {
                        continue;
                    }
                    // The command type is ShellHub here, so the route always
                    // parses; the default only guards a corrupt queue entry.
                    let page = shell_page_for(cmd.shell_hub_route().unwrap_or_default());
                    info!(
                        "[process_commands] ShellHub command received: {}",
                        page.map_or("hub", crate::jwm::features::ShellHubRoute::label)
                    );
                    if let Err(error) = self.open_shell_from_status_bar(backend, page) {
                        warn!("[process_commands] could not open the shell: {error}");
                    }
                }
                CommandType::SetMinimizedGeometry => {
                    let current_generation = dock_generation_for_batch(
                        &mut dock_generation_cache,
                        source_monitor,
                        || self.refresh_minimized_generation(source_monitor),
                    );
                    if minimized_preview_generation_reconciliation_needed(
                        &mut reconciled_preview_generations,
                        self.active_minimized_preview,
                        self.active_minimized_preview_generation,
                        source_monitor,
                        current_generation,
                    ) {
                        self.clear_minimized_preview_for(backend, source_monitor, None);
                    }
                    if !valid_dock_command_source(source_monitor, current_generation, &cmd) {
                        warn!(
                            "[process_commands] rejected stale Dock geometry from monitor {}",
                            source_monitor
                        );
                        continue;
                    }
                    self.reconcile_minimized_overflow_targets(
                        backend,
                        source_monitor,
                        cmd.minimized_generation,
                    );
                    let anchor = compositor_rect_from_bar_command(&cmd);
                    if cmd.window_id == 0 {
                        if let Some(anchor) = anchor {
                            // The shelf centre is the first-minimize fallback:
                            // the new item does not exist in the preceding bar
                            // frame, so a two-process layout cannot yet name
                            // its final slot.
                            self.minimized_dock_shelves.insert(source_monitor, anchor);
                        } else {
                            self.minimized_dock_shelves.remove(&source_monitor);
                        }
                    } else {
                        let window = WindowId::from_raw(cmd.window_id);
                        let valid_client = self.wintoclient(window).is_some_and(|key| {
                            self.state.clients.get(key).is_some_and(|client| {
                                client.state.is_hidden
                                    && client
                                        .mon
                                        .and_then(|key| self.state.monitors.get(key))
                                        .map(|monitor| monitor.num)
                                        == Some(source_monitor)
                            })
                        });
                        // Empty anchors are withdrawals and remain valid for
                        // an item that has just fallen out of the newest-16
                        // wire projection. A non-empty target must name an
                        // item the current bar scene could actually contain.
                        let addressable = minimized_projection_contains(
                            &self.minimized_projection_epochs,
                            source_monitor,
                            cmd.window_id,
                        );
                        if valid_client && (anchor.is_none() || addressable) {
                            // A zero-sized geometry is an explicit withdrawal
                            // when a responsive shelf stops realizing a slot.
                            backend.compositor_set_window_dock_geometry(window, anchor);
                        }
                    }
                }
                CommandType::PreviewMinimized => {
                    let current_generation = dock_generation_for_batch(
                        &mut dock_generation_cache,
                        source_monitor,
                        || self.refresh_minimized_generation(source_monitor),
                    );
                    if minimized_preview_generation_reconciliation_needed(
                        &mut reconciled_preview_generations,
                        self.active_minimized_preview,
                        self.active_minimized_preview_generation,
                        source_monitor,
                        current_generation,
                    ) {
                        self.clear_minimized_preview_for(backend, source_monitor, None);
                    }
                    if !valid_dock_command_source(source_monitor, current_generation, &cmd) {
                        warn!(
                            "[process_commands] rejected stale minimized preview from monitor {}",
                            source_monitor
                        );
                        continue;
                    }
                    self.reconcile_minimized_overflow_targets(
                        backend,
                        source_monitor,
                        cmd.minimized_generation,
                    );
                    if cmd.flags & PREVIEW_MINIMIZED_FLAG_VISIBLE == 0 {
                        let window =
                            (cmd.window_id != 0).then(|| WindowId::from_raw(cmd.window_id));
                        self.clear_minimized_preview_for(backend, source_monitor, window);
                        continue;
                    }
                    let window = WindowId::from_raw(cmd.window_id);
                    let renewal = cmd.flags & PREVIEW_MINIMIZED_FLAG_RENEWAL != 0;
                    // Per-monitor command queues are drained from a HashMap,
                    // so their relative order is not a global hover order. A
                    // delayed fresh ENTER from monitor A must not replace the
                    // preview after the pointer has already crossed to B.
                    // Fail open only when a backend cannot query the pointer;
                    // ownership/generation checks below still apply there.
                    let pointer_on_source_monitor = backend
                        .input_ops()
                        .get_pointer_position()
                        .ok()
                        .and_then(|(x, y)| {
                            let monitor_key = self.get_monitor_by_id(source_monitor)?;
                            let monitor = self.state.monitors.get(monitor_key)?;
                            let geometry = &monitor.geometry;
                            let left = f64::from(geometry.m_x);
                            let top = f64::from(geometry.m_y);
                            let right = left + f64::from(geometry.m_w.max(0));
                            let bottom = top + f64::from(geometry.m_h.max(0));
                            Some(x >= left && x < right && y >= top && y < bottom)
                        });
                    if !minimized_preview_may_activate(
                        self.active_minimized_preview,
                        self.active_minimized_preview_generation,
                        source_monitor,
                        window,
                        cmd.minimized_generation,
                        renewal,
                        pointer_on_source_monitor,
                    ) {
                        // A lease renewal is not a fresh hover intent. In
                        // particular, an old timer on monitor A must not take
                        // ownership back after a normal enter on monitor B.
                        continue;
                    }
                    let Some(client_key) = self.wintoclient(window) else {
                        continue;
                    };
                    let valid_client = self.state.clients.get(client_key).is_some_and(|client| {
                        client.state.is_hidden
                            && client
                                .mon
                                .and_then(|key| self.state.monitors.get(key))
                                .map(|m| m.num)
                                == Some(source_monitor)
                    });
                    let Some(anchor) = compositor_rect_from_bar_command(&cmd) else {
                        continue;
                    };
                    let addressable = minimized_projection_contains(
                        &self.minimized_projection_epochs,
                        source_monitor,
                        cmd.window_id,
                    );
                    if valid_client && addressable {
                        // Hover cards may magnify visually. Their live anchor
                        // positions the floating preview only; the resting
                        // Genie/static-thumbnail target remains the stable
                        // geometry last published by SetMinimizedGeometry.
                        backend.compositor_set_minimized_window_preview(Some(window), Some(anchor));
                        self.active_minimized_preview = Some((source_monitor, window));
                        self.active_minimized_preview_generation = Some(cmd.minimized_generation);
                    }
                }
                CommandType::RestoreMinimized => {
                    let current_generation = dock_generation_for_batch(
                        &mut dock_generation_cache,
                        source_monitor,
                        || self.refresh_minimized_generation(source_monitor),
                    );
                    if minimized_preview_generation_reconciliation_needed(
                        &mut reconciled_preview_generations,
                        self.active_minimized_preview,
                        self.active_minimized_preview_generation,
                        source_monitor,
                        current_generation,
                    ) {
                        self.clear_minimized_preview_for(backend, source_monitor, None);
                    }
                    if !valid_dock_command_source(source_monitor, current_generation, &cmd) {
                        warn!(
                            "[process_commands] rejected stale minimized restore from monitor {}",
                            source_monitor
                        );
                        continue;
                    }
                    self.reconcile_minimized_overflow_targets(
                        backend,
                        source_monitor,
                        cmd.minimized_generation,
                    );
                    let window = WindowId::from_raw(cmd.window_id);
                    let Some(client_key) = self.wintoclient(window) else {
                        continue;
                    };
                    let valid_client = self.state.clients.get(client_key).is_some_and(|client| {
                        client.state.is_hidden
                            && client
                                .mon
                                .and_then(|key| self.state.monitors.get(key))
                                .map(|m| m.num)
                                == Some(source_monitor)
                    });
                    let addressable = minimized_projection_contains(
                        &self.minimized_projection_epochs,
                        source_monitor,
                        cmd.window_id,
                    );
                    if !valid_client || !addressable {
                        continue;
                    }
                    self.clear_minimized_preview_for(backend, source_monitor, Some(window));
                    if let Some(anchor) = compositor_rect_from_bar_command(&cmd) {
                        backend.compositor_set_window_dock_geometry(window, Some(anchor));
                    }
                    if let Err(error) = self.reveal_and_focus(backend, window) {
                        warn!(
                            "[process_commands] could not restore Dock window {window:?}: {error}"
                        );
                    }
                    // Restore mutates the projection. Recompute before any
                    // later command in this drained batch so its old epoch is
                    // rejected without re-sorting for every geometry update.
                    dock_generation_cache.remove(&source_monitor);
                    reconciled_preview_generations.remove(&source_monitor);
                }
                CommandType::None => {}
            }
        }
    }

    /// Make a non-Dock bar action operate on the monitor named by its wire
    /// command.  A bar window is deliberately not focusable, so clicking a
    /// tag or layout on a secondary output does not otherwise update
    /// `sel_mon`; applying the command directly would mutate whichever output
    /// happened to own keyboard focus instead.
    fn select_status_bar_control_monitor(
        &mut self,
        backend: &mut dyn Backend,
        monitor_id: MonitorIndex,
    ) -> bool {
        let Some(monitor_key) = self.get_monitor_by_id(monitor_id) else {
            warn!(
                "[process_commands] rejected control command for missing monitor {}",
                monitor_id
            );
            return false;
        };
        if self.state.sel_mon != Some(monitor_key)
            && let Err(error) = self.switch_to_monitor(backend, monitor_key)
        {
            warn!(
                "[process_commands] could not select monitor {} for bar command: {}",
                monitor_id, error
            );
            return false;
        }
        true
    }

    fn get_transient_for(&self, backend: &mut dyn Backend, window: WindowId) -> Option<WindowId> {
        backend.property_ops().transient_for(window)
    }

    pub fn show_keybindings(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[show_keybindings]");

        let cfg = CONFIG.load();
        let mut lines: Vec<String> = Vec::new();
        for kc in cfg.key_configs() {
            let mods = kc.modifier.join("+");
            let shortcut = if mods.is_empty() {
                kc.key.clone()
            } else {
                format!("{}+{}", mods, kc.key)
            };

            let desc = match kc.function.as_str() {
                "spawn" => match &kc.argument {
                    crate::config::ArgumentConfig::StringVec(v) => {
                        format!("spawn {}", v.first().map(|s| s.as_str()).unwrap_or(""))
                    }
                    _ => "spawn".to_string(),
                },
                "setlayout" => match &kc.argument {
                    crate::config::ArgumentConfig::String(s) => format!("layout: {}", s),
                    _ => "previous layout".to_string(),
                },
                "lastlayout" => "previous layout".to_string(),
                "focusstack" => match &kc.argument {
                    crate::config::ArgumentConfig::Int(i) => {
                        if *i > 0 {
                            "focus next".to_string()
                        } else {
                            "focus prev".to_string()
                        }
                    }
                    _ => "focusstack".to_string(),
                },
                "incnmaster" => match &kc.argument {
                    crate::config::ArgumentConfig::Int(i) => {
                        if *i > 0 {
                            "master +1".to_string()
                        } else {
                            "master -1".to_string()
                        }
                    }
                    _ => "incnmaster".to_string(),
                },
                "setmfact" => match &kc.argument {
                    crate::config::ArgumentConfig::Float(f) => {
                        if *f > 0.0 {
                            "mfact +".to_string()
                        } else {
                            "mfact -".to_string()
                        }
                    }
                    _ => "setmfact".to_string(),
                },
                "view" | "tag" | "toggleview" | "toggletag" => match &kc.argument {
                    crate::config::ArgumentConfig::UInt(u) => format!("{} tag {}", kc.function, u),
                    _ => kc.function.clone(),
                },
                other => other.to_string(),
            };

            lines.push(format!("{:<28} {}", shortcut, desc));
        }

        // 添加 tag 快捷键说明
        let tags_len = cfg.tags_length();
        lines.push(format!("{:<28} view tag 1-{}", "Mod1+[1-9]", tags_len));
        lines.push(format!(
            "{:<28} move to tag 1-{}",
            "Mod1+Shift+[1-9]", tags_len
        ));
        lines.push(format!(
            "{:<28} toggle view tag 1-{}",
            "Mod1+Ctrl+[1-9]", tags_len
        ));
        lines.push(format!(
            "{:<28} toggle tag 1-{}",
            "Mod1+Ctrl+Shift+[1-9]", tags_len
        ));
        lines.push(format!("{:<28} {}", "Mod1+0", "view all tags"));

        self.prepare_system_ui(backend, "keybinding viewer", false)?;
        self.features.system_ui =
            crate::jwm::features::SystemUiState::info("JWM KEYBINDINGS", lines);
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Compute the night light color temperature factor (0.0 = neutral, up to
    /// `full_temp` when fully inside the night window).  Times are given as
    /// "HH:MM" strings.  `transition_mins` controls the linear ramp-in/out at
    /// the edges of the night window.
    fn compute_night_light_temp(
        start_str: &str,
        end_str: &str,
        full_temp: f32,
        transition_mins: u32,
    ) -> f32 {
        fn parse_hhmm(s: &str) -> Option<u32> {
            let parts: Vec<&str> = s.split(':').collect();
            if parts.len() != 2 {
                return None;
            }
            let h: u32 = parts[0].parse().ok()?;
            let m: u32 = parts[1].parse().ok()?;
            Some(h * 60 + m)
        }

        let start = match parse_hhmm(start_str) {
            Some(v) => v,
            None => return 0.0,
        };
        let end = match parse_hhmm(end_str) {
            Some(v) => v,
            None => return 0.0,
        };

        // Current time in minutes since midnight
        let now = {
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Local time: offset from UTC.  Use libc localtime.
            let secs = d as libc::time_t;
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            unsafe {
                libc::localtime_r(&secs, &mut tm);
            }
            (tm.tm_hour as u32) * 60 + (tm.tm_min as u32)
        };

        let day = 24 * 60u32; // 1440
        let trans = transition_mins;

        // Normalize everything so that `start` is time-zero (modular arithmetic).
        // Night window runs from 0 to `length` in the rotated space.
        let length = if end >= start {
            end - start
        } else {
            end + day - start
        };
        let cur = if now >= start {
            now - start
        } else {
            now + day - start
        };

        if cur > length {
            // Outside the night window — check if approaching start (ramp in)
            let before_start = if now < start {
                start - now
            } else {
                start + day - now
            };
            if trans > 0 && before_start < trans {
                // Ramping in: approaching start
                let t = 1.0 - (before_start as f32 / trans as f32);
                return full_temp * t.clamp(0.0, 1.0);
            }
            return 0.0;
        }

        // Inside the night window
        if trans > 0 && cur < trans {
            // Ramp in at the start edge
            let t = cur as f32 / trans as f32;
            return full_temp * t.clamp(0.0, 1.0);
        }
        if trans > 0 && (length - cur) < trans {
            // Ramp out at the end edge
            let t = (length - cur) as f32 / trans as f32;
            return full_temp * t.clamp(0.0, 1.0);
        }
        full_temp
    }

    /// 处理 Expose 事件（窗口需要重绘）
    fn expose(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
        count: u16,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[expose]");
        if count != 0 {
            return Ok(());
        }

        if let Some(monitor_key) = self.wintomon(backend, Some(window)) {
            if let Some(monitor) = self.state.monitors.get(monitor_key) {
                self.mark_bar_update_needed_if_visible(Some(monitor.num));
            }
        }

        Ok(())
    }

    fn update_net_client_list(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut ordered: Vec<WindowId> = Vec::with_capacity(self.state.client_order.len());
        for &key in &self.state.client_order {
            if let Some(client) = self.state.clients.get(key) {
                ordered.push(client.win);
            }
        }

        let mut stacking: Vec<WindowId> = Vec::new();
        for &mon_key in &self.state.monitor_order {
            if let Some(stack) = self.state.monitor_stack.get(mon_key) {
                for &ck in stack.iter().rev() {
                    if let Some(c) = self.state.clients.get(ck) {
                        stacking.push(c.win);
                    }
                }
            }
        }

        backend.on_client_list_changed(&ordered, &stacking)?;
        Ok(())
    }

    fn update_bar_message_for_monitor(&mut self, mon_key_opt: Option<MonitorKey>) {
        // info!("[update_bar_message_for_monitor]");

        let Some(mon_key) = mon_key_opt else {
            error!("Monitor key is None, cannot update bar message.");
            return;
        };

        let Some(monitor) = self.state.monitors.get(mon_key) else {
            error!("Monitor {mon_key:?} not found");
            return;
        };

        self.message = SharedMessage::default();
        let mut monitor_info_for_message = MonitorInfo::default();

        // Publish the actual bar outer origin when it is managed. Before its
        // first map, use the same monitor+padding formula as
        // `position_secondary_bar_on_monitor`. Never publish the client work
        // area (`w_y`): that includes the bar's own strut and feeds a +height
        // offset back into both placement and Dock anchor conversion.
        let configured_pad = CONFIG.load().status_bar_padding();
        let bar_geometry = self
            .secondary_bars
            .get(&monitor.num)
            .and_then(|bar| bar.client_key)
            .and_then(|key| self.state.clients.get(key))
            .filter(|client| client.geometry.w > 0)
            .map(|client| (client.geometry.x, client.geometry.y, client.geometry.w));
        let (bar_x, bar_y, bar_width) = bar_geometry.unwrap_or((
            monitor.geometry.m_x + configured_pad,
            monitor.geometry.m_y + configured_pad,
            (monitor.geometry.m_w - 2 * configured_pad).max(1),
        ));
        monitor_info_for_message.monitor_x = bar_x;
        monitor_info_for_message.monitor_y = bar_y;
        monitor_info_for_message.monitor_width = bar_width;
        monitor_info_for_message.monitor_height = monitor.geometry.m_h;
        monitor_info_for_message.monitor_num = monitor.num;
        monitor_info_for_message.set_ltsymbol(&monitor.lt_symbol);

        let (occupied_tags_mask, urgent_tags_mask) = self.calculate_tag_masks(mon_key);
        let active_tagset = monitor.get_active_tags();

        for i in 0..CONFIG.load().tags_length() {
            let tag_bit = 1 << i;

            let is_filled_tag = self.is_filled_tag(mon_key, tag_bit);

            let is_selected_tag = (active_tagset & tag_bit) != 0;
            let is_urgent_tag = (urgent_tags_mask & tag_bit) != 0;
            let is_occupied_tag = (occupied_tags_mask & tag_bit) != 0;
            let tag_status = TagStatus::new(
                is_selected_tag,
                is_urgent_tag,
                is_filled_tag,
                is_occupied_tag,
            );
            monitor_info_for_message.set_tag_status(i, tag_status);
        }
        // Title and application identity are one projection: the latter is
        // what every bar resolves through the desktop-entry/icon-theme
        // database. Keeping this shared with StatusBarBuilder::build_message
        // prevents the live publisher from silently dropping the icon input.
        StatusBarBuilder::set_selected_client_metadata(
            &mut monitor_info_for_message,
            &self.state.clients,
            monitor,
        );
        self.message.monitor_info = monitor_info_for_message;

        let monitor_clients = self
            .state
            .monitor_clients
            .get(mon_key)
            .map_or(&[][..], Vec::as_slice);
        let mut minimized = StatusBarBuilder::get_minimized_windows(
            &self.state.clients,
            monitor_clients,
            monitor.num,
        );
        StatusBarBuilder::prioritize_snapshot_capacity(&mut minimized);
        self.message.set_minimized_windows(&minimized);
        self.message.wm_session_id = wm_session_id();
        self.message.minimized_generation = self
            .refresh_minimized_generation(monitor.num)
            .unwrap_or_default();
        self.message.update_timestamp();
    }

    fn minimized_projection_signature(&self, monitor_id: MonitorIndex) -> Option<Vec<(u64, u64)>> {
        let monitor_key = self.get_monitor_by_id(monitor_id)?;
        let mut signature: Vec<_> = self
            .state
            .monitor_clients
            .get(monitor_key)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(|client_key| self.state.clients.get(*client_key))
            .filter(|client| {
                client.state.is_hidden && StatusBarBuilder::is_minimized_dock_eligible(client)
            })
            .map(|client| (client.win.raw(), client.state.minimized_order))
            .collect();
        signature.sort_by_key(|(window_id, order)| (*order, *window_id));
        Some(signature)
    }

    fn refresh_minimized_generation(&mut self, monitor_id: MonitorIndex) -> Option<u64> {
        let signature = self.minimized_projection_signature(monitor_id)?;
        Some(minimized_generation_for_signature(
            &mut self.minimized_projection_epochs,
            monitor_id,
            signature,
        ))
    }

    fn reconcile_minimized_overflow_targets(
        &mut self,
        backend: &mut dyn Backend,
        monitor_id: MonitorIndex,
        generation: u64,
    ) {
        let withdrawals = minimized_overflow_targets_to_withdraw(
            &mut self.reconciled_minimized_target_generations,
            &self.minimized_projection_epochs,
            monitor_id,
            generation,
        );
        for window_id in withdrawals {
            backend.compositor_set_window_dock_geometry(WindowId::from_raw(window_id), None);
        }
    }

    fn calculate_tag_masks(&self, mon_key: MonitorKey) -> (u32, u32) {
        const EMPTY_CLIENTS: &[ClientKey] = &[];
        let monitor_clients = self
            .state
            .monitor_clients
            .get(mon_key)
            .map_or(EMPTY_CLIENTS, Vec::as_slice);
        StatusBarBuilder::calculate_tag_masks(&self.state.clients, monitor_clients)
    }

    fn is_filled_tag(&self, mon_key: MonitorKey, tag_bit: u32) -> bool {
        let is_selected = self.state.sel_mon == Some(mon_key);
        if let Some(monitor) = self.state.monitors.get(mon_key) {
            StatusBarBuilder::is_filled_tag(&self.state.clients, monitor, tag_bit, is_selected)
        } else {
            false
        }
    }
}

/// Translate a bar's wire route into the shell page JWM owns.
///
/// `None` is the hub home page: the protocol gives it a route of its own,
/// while JWM models it as "no child page selected". An unknown code has
/// already degraded to `Hub` inside the protocol crate, so a bar newer than
/// the running JWM lands on the hub instead of having its click dropped.
fn shell_page_for(
    route: xbar_core::shared_structures::ShellHubRoute,
) -> Option<crate::jwm::features::ShellHubRoute> {
    use crate::jwm::features::ShellHubRoute;
    use xbar_core::shared_structures::ShellHubRoute as Wire;

    match route {
        Wire::Hub => None,
        Wire::Applications => Some(ShellHubRoute::Applications),
        Wire::Notifications => Some(ShellHubRoute::Notifications),
        Wire::Clipboard => Some(ShellHubRoute::Clipboard),
        Wire::Calendar => Some(ShellHubRoute::Calendar),
        Wire::Wallpaper => Some(ShellHubRoute::Wallpaper),
    }
}

/// Reject delayed commands from a previous WM lifetime and commands injected
/// through a different monitor's queue. Both checks matter because window ids
/// and bar shared-memory paths are intentionally reused after restart.
fn minimized_generation_for_signature(
    epochs: &mut HashMap<MonitorIndex, (Vec<(u64, u64)>, u64)>,
    monitor_id: MonitorIndex,
    signature: Vec<(u64, u64)>,
) -> u64 {
    if let Some((previous, generation)) = epochs.get(&monitor_id)
        && previous == &signature
    {
        return *generation;
    }
    let generation = BAR_SNAPSHOT_GENERATION.fetch_add(1, Ordering::Relaxed);
    epochs.insert(monitor_id, (signature, generation));
    generation
}

fn dock_generation_for_batch(
    cache: &mut HashMap<MonitorIndex, Option<u64>>,
    monitor_id: MonitorIndex,
    refresh: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    if let Some(generation) = cache.get(&monitor_id) {
        return *generation;
    }
    let generation = refresh();
    cache.insert(monitor_id, generation);
    generation
}

fn valid_dock_command_source(
    source_monitor: i32,
    current_generation: Option<u64>,
    command: &SharedCommand,
) -> bool {
    command.monitor_id == source_monitor
        && command.wm_session_id == wm_session_id()
        && current_generation == Some(command.minimized_generation)
}

/// Whether a window is present in the bounded Dock projection represented by
/// the current epoch. The signature deliberately contains every eligible
/// hidden client so membership changes beyond the wire limit still advance
/// the epoch; only its newest tail is addressable by a bar card.
fn minimized_projection_contains(
    epochs: &HashMap<MonitorIndex, (Vec<(u64, u64)>, u64)>,
    monitor_id: MonitorIndex,
    window_id: u64,
) -> bool {
    let Some((signature, _)) = epochs.get(&monitor_id) else {
        return false;
    };
    let addressable_start = signature.len().saturating_sub(MAX_MINIMIZED_WINDOWS);
    signature[addressable_start..]
        .iter()
        .any(|(candidate, _)| *candidate == window_id)
}

/// Return the non-addressable prefix whose compositor targets must be
/// withdrawn exactly once for this monitor/projection incarnation.
///
/// The wire snapshot carries only the newest [`MAX_MINIMIZED_WINDOWS`]. A
/// native reporter can explicitly withdraw cards it previously acknowledged,
/// but Web frontends are intentionally stateless across render trees. Keeping
/// this invariant in the WM prevents the 17th minimize from leaving the former
/// oldest card painted at a stale slot.
fn minimized_overflow_targets_to_withdraw(
    reconciled_generations: &mut HashMap<MonitorIndex, u64>,
    epochs: &HashMap<MonitorIndex, (Vec<(u64, u64)>, u64)>,
    monitor_id: MonitorIndex,
    generation: u64,
) -> Vec<u64> {
    let Some((signature, current_generation)) = epochs.get(&monitor_id) else {
        return Vec::new();
    };
    if *current_generation != generation
        || reconciled_generations.get(&monitor_id) == Some(&generation)
    {
        return Vec::new();
    }
    reconciled_generations.insert(monitor_id, generation);
    let addressable_start = signature.len().saturating_sub(MAX_MINIMIZED_WINDOWS);
    signature[..addressable_start]
        .iter()
        .map(|(window_id, _)| *window_id)
        .collect()
}

/// Resolve the destination of ordinary bar controls.  Current bars always
/// carry their owning monitor explicitly; the negative fallback preserves the
/// pre-monitor-id wire behavior for an older producer connected during a
/// rolling rebuild.  Dock commands intentionally do not use this fallback —
/// their source queue is an ownership boundary and is validated separately.
fn status_bar_control_monitor(
    source_monitor: MonitorIndex,
    command: &SharedCommand,
) -> MonitorIndex {
    if command.monitor_id < 0 {
        source_monitor
    } else {
        command.monitor_id
    }
}

fn minimized_preview_owned_by(
    active: Option<(MonitorIndex, WindowId)>,
    source_monitor: MonitorIndex,
    window: Option<WindowId>,
) -> bool {
    active.is_some_and(|(active_monitor, active_window)| {
        active_monitor == source_monitor && window.is_none_or(|window| window == active_window)
    })
}

fn minimized_preview_generation_mismatch(
    active: Option<(MonitorIndex, WindowId)>,
    active_generation: Option<u64>,
    source_monitor: MonitorIndex,
    current_generation: Option<u64>,
) -> bool {
    active.is_some_and(|(active_monitor, _)| active_monitor == source_monitor)
        && !matches!(
            (active_generation, current_generation),
            (Some(active), Some(current)) if active == current
        )
}

fn minimized_preview_generation_reconciliation_needed(
    reconciled_monitors: &mut HashSet<MonitorIndex>,
    active: Option<(MonitorIndex, WindowId)>,
    active_generation: Option<u64>,
    source_monitor: MonitorIndex,
    current_generation: Option<u64>,
) -> bool {
    reconciled_monitors.insert(source_monitor)
        && minimized_preview_generation_mismatch(
            active,
            active_generation,
            source_monitor,
            current_generation,
        )
}

fn minimized_preview_may_activate(
    active: Option<(MonitorIndex, WindowId)>,
    active_generation: Option<u64>,
    source_monitor: MonitorIndex,
    window: WindowId,
    requested_generation: u64,
    renewal: bool,
    pointer_on_source_monitor: Option<bool>,
) -> bool {
    pointer_on_source_monitor != Some(false)
        && (!renewal
            || (active == Some((source_monitor, window))
                && active_generation == Some(requested_generation)))
}

fn compositor_rect_from_bar_command(command: &SharedCommand) -> Option<CompositorRect> {
    CompositorRect::new(
        command.anchor_x as f32,
        command.anchor_y as f32,
        command.anchor_w as f32,
        command.anchor_h as f32,
    )
    .normalized()
}

#[cfg(test)]
mod shell_hub_command_tests {
    use super::*;
    use crate::jwm::features::ShellHubRoute;
    use xbar_core::shared_structures::MinimizedWindowAnchor;
    use xbar_core::shared_structures::ShellHubRoute as Wire;

    #[test]
    fn every_wire_route_maps_to_a_page_or_the_hub() {
        let cases = [
            (Wire::Hub, None),
            (Wire::Applications, Some(ShellHubRoute::Applications)),
            (Wire::Notifications, Some(ShellHubRoute::Notifications)),
            (Wire::Clipboard, Some(ShellHubRoute::Clipboard)),
            (Wire::Calendar, Some(ShellHubRoute::Calendar)),
            (Wire::Wallpaper, Some(ShellHubRoute::Wallpaper)),
        ];
        assert_eq!(
            cases.len(),
            Wire::ALL.len(),
            "a new wire route needs a page here"
        );
        for (wire, expected) in cases {
            assert_eq!(shell_page_for(wire), expected);
        }
    }

    #[test]
    fn a_shell_command_round_trips_from_the_bar_to_a_page() {
        let command = SharedCommand::shell_hub(Wire::Clipboard, 2);
        assert_eq!(command.get_command_type(), CommandType::ShellHub);
        assert_eq!(
            command.shell_hub_route().map(shell_page_for),
            Some(Some(ShellHubRoute::Clipboard))
        );
        // Commands from the tag/layout paths never look like shell requests.
        assert_eq!(SharedCommand::view_tag(1, 0).shell_hub_route(), None);
    }

    #[test]
    fn ordinary_bar_controls_target_the_monitor_carried_on_the_wire() {
        let explicit_other_monitor = SharedCommand::view_tag(1, 3);
        assert_eq!(status_bar_control_monitor(1, &explicit_other_monitor), 3);

        let legacy_unscoped = SharedCommand::set_layout(2, -1);
        assert_eq!(status_bar_control_monitor(1, &legacy_unscoped), 1);
    }

    #[test]
    fn dock_commands_are_scoped_to_session_and_source_monitor() {
        assert!(wm_session_id() <= (1_u64 << 53) - 1);

        let command = SharedCommand::preview_minimized(
            0xfeed,
            wm_session_id(),
            73,
            2,
            PREVIEW_MINIMIZED_FLAG_VISIBLE,
            MinimizedWindowAnchor::new(-120, 4, 36, 28),
        );
        assert!(valid_dock_command_source(2, Some(73), &command));
        assert!(!valid_dock_command_source(1, Some(73), &command));
        assert!(!valid_dock_command_source(2, Some(74), &command));

        let stale = SharedCommand::preview_minimized(
            0xfeed,
            wm_session_id().wrapping_add(1),
            73,
            2,
            PREVIEW_MINIMIZED_FLAG_VISIBLE,
            MinimizedWindowAnchor::new(-120, 4, 36, 28),
        );
        assert!(!valid_dock_command_source(2, Some(73), &stale));

        let rect = compositor_rect_from_bar_command(&command).unwrap();
        assert_eq!(rect.x, -120.0);
        assert_eq!(rect.width, 36.0);
    }

    #[test]
    fn empty_dock_anchor_never_reaches_the_compositor() {
        let command = SharedCommand::set_minimized_geometry(
            0,
            wm_session_id(),
            1,
            0,
            MinimizedWindowAnchor::new(0, 0, 0, 38),
        );
        assert!(compositor_rect_from_bar_command(&command).is_none());
    }

    #[test]
    fn rapid_restore_then_reminimize_rejects_the_old_generation() {
        let mut epochs = HashMap::new();
        let old = minimized_generation_for_signature(&mut epochs, 2, vec![(0xfeed, 10)]);
        let mut batch_cache = HashMap::new();
        assert_eq!(
            dock_generation_for_batch(&mut batch_cache, 2, || Some(old)),
            Some(old)
        );
        assert_eq!(
            minimized_generation_for_signature(&mut epochs, 2, vec![(0xfeed, 10)]),
            old,
            "ordinary snapshots of one projection keep their generation"
        );

        // The backend window id is deliberately reused. A new minimized_order
        // is the incarnation boundary that prevents a delayed Dock click from
        // restoring the newly minimized copy.
        let new = minimized_generation_for_signature(&mut epochs, 2, vec![(0xfeed, 11)]);
        assert_ne!(new, old);
        // A successful restore invalidates this monitor's batch cache. The
        // next command therefore observes the re-minimized incarnation even
        // when both commands were drained in one update turn.
        batch_cache.remove(&2);
        assert_eq!(
            dock_generation_for_batch(&mut batch_cache, 2, || Some(new)),
            Some(new)
        );
        let stale = SharedCommand::restore_minimized(
            0xfeed,
            wm_session_id(),
            old,
            2,
            MinimizedWindowAnchor::default(),
        );
        let current = SharedCommand::restore_minimized(
            0xfeed,
            wm_session_id(),
            new,
            2,
            MinimizedWindowAnchor::default(),
        );
        assert!(!valid_dock_command_source(2, Some(new), &stale));
        assert!(valid_dock_command_source(2, Some(new), &current));
    }

    #[test]
    fn current_epoch_still_rejects_overflow_and_non_projected_window_ids() {
        let mut epochs = HashMap::new();
        let signature: Vec<_> = (1..=u64::try_from(MAX_MINIMIZED_WINDOWS + 1).unwrap())
            .map(|window| (window, window * 10))
            .collect();
        minimized_generation_for_signature(&mut epochs, 2, signature);

        assert!(
            !minimized_projection_contains(&epochs, 2, 1),
            "the oldest of 17 is overflow, even with the current generation"
        );
        assert!(minimized_projection_contains(&epochs, 2, 2));
        assert!(minimized_projection_contains(
            &epochs,
            2,
            u64::try_from(MAX_MINIMIZED_WINDOWS + 1).unwrap()
        ));
        assert!(!minimized_projection_contains(&epochs, 2, 999));
        assert!(!minimized_projection_contains(&epochs, 3, 2));
    }

    #[test]
    fn first_current_generation_command_withdraws_only_the_overflow_prefix_once() {
        let mut epochs = HashMap::new();
        let signature: Vec<_> = (1..=u64::try_from(MAX_MINIMIZED_WINDOWS + 1).unwrap())
            .map(|window| (window, window * 10))
            .collect();
        let generation = minimized_generation_for_signature(&mut epochs, 2, signature);
        let mut reconciled = HashMap::new();

        assert_eq!(
            minimized_overflow_targets_to_withdraw(&mut reconciled, &epochs, 2, generation,),
            vec![1]
        );
        assert!(
            minimized_overflow_targets_to_withdraw(&mut reconciled, &epochs, 2, generation,)
                .is_empty(),
            "preview renewals in the same epoch must not repeatedly tear down targets"
        );
        assert!(
            minimized_overflow_targets_to_withdraw(
                &mut HashMap::new(),
                &epochs,
                2,
                generation.wrapping_add(1),
            )
            .is_empty(),
            "a stale or forged generation cannot trigger cleanup"
        );

        let next_signature: Vec<_> = (1..=u64::try_from(MAX_MINIMIZED_WINDOWS + 2).unwrap())
            .map(|window| (window, window * 10))
            .collect();
        let next_generation = minimized_generation_for_signature(&mut epochs, 2, next_signature);
        assert_eq!(
            minimized_overflow_targets_to_withdraw(&mut reconciled, &epochs, 2, next_generation,),
            vec![1, 2]
        );
    }

    #[test]
    fn reused_monitor_number_gets_a_new_projection_generation() {
        let mut epochs = HashMap::new();
        let signature = vec![(0xfeed, 10)];
        let old = minimized_generation_for_signature(&mut epochs, 2, signature.clone());
        epochs.remove(&2); // the output-removal seam invalidates its incarnation
        let replacement = minimized_generation_for_signature(&mut epochs, 2, signature);
        assert_ne!(replacement, old);
    }

    #[test]
    fn delayed_preview_leave_cannot_dismiss_another_monitor_or_window() {
        let first = WindowId::from_raw(11);
        let second = WindowId::from_raw(22);
        let active = Some((2, second));

        assert!(!minimized_preview_owned_by(active, 1, Some(first)));
        assert!(!minimized_preview_owned_by(active, 2, Some(first)));
        assert!(minimized_preview_owned_by(active, 2, Some(second)));
        assert!(minimized_preview_owned_by(active, 2, None));
    }

    #[test]
    fn first_dock_command_of_a_new_generation_retires_the_old_preview_owner() {
        let active = Some((2, WindowId::from_raw(11)));
        let mut reconciled = HashSet::new();

        assert!(minimized_preview_generation_reconciliation_needed(
            &mut reconciled,
            active,
            Some(7),
            2,
            Some(8),
        ));
        assert!(
            !minimized_preview_generation_reconciliation_needed(
                &mut reconciled,
                active,
                Some(7),
                2,
                Some(8),
            ),
            "later geometry in the same monitor batch must not clear twice"
        );
        assert!(!minimized_preview_generation_mismatch(
            active,
            Some(7),
            1,
            Some(8),
        ));
        assert!(minimized_preview_generation_mismatch(
            active,
            None,
            2,
            Some(8),
        ));
    }

    #[test]
    fn old_generation_leave_cannot_clear_the_same_windows_new_preview() {
        let window = WindowId::from_raw(11);
        let current_generation = 8;
        let stale_leave = SharedCommand::preview_minimized(
            window.raw(),
            wm_session_id(),
            7,
            2,
            0,
            MinimizedWindowAnchor::default(),
        );

        assert!(!valid_dock_command_source(
            2,
            Some(current_generation),
            &stale_leave,
        ));
        assert!(!minimized_preview_generation_mismatch(
            Some((2, window)),
            Some(current_generation),
            2,
            Some(current_generation),
        ));
    }

    #[test]
    fn stale_renewal_and_leave_cannot_retake_or_clear_a_replaced_preview() {
        let a = (1, WindowId::from_raw(11));
        let b = (2, WindowId::from_raw(22));
        let mut active = None;
        let mut active_generation = None;

        assert!(minimized_preview_may_activate(
            active,
            active_generation,
            a.0,
            a.1,
            7,
            false,
            None
        ));
        active = Some(a);
        active_generation = Some(7);
        assert!(minimized_preview_may_activate(
            active,
            active_generation,
            a.0,
            a.1,
            7,
            true,
            None
        ));
        assert!(!minimized_preview_may_activate(
            active,
            active_generation,
            a.0,
            a.1,
            8,
            true,
            None
        ));

        // A normal enter is explicit user intent and replaces the old owner.
        assert!(minimized_preview_may_activate(
            active,
            active_generation,
            b.0,
            b.1,
            8,
            false,
            None
        ));
        active = Some(b);
        active_generation = Some(8);

        // A's timer and delayed leave arrive after B's enter. Neither may
        // mutate B's preview ownership.
        assert!(!minimized_preview_may_activate(
            active,
            active_generation,
            a.0,
            a.1,
            7,
            true,
            None
        ));
        assert!(!minimized_preview_owned_by(active, a.0, Some(a.1)));
        assert_eq!(active, Some(b));
    }

    #[test]
    fn pointer_location_rejects_cross_monitor_preview_commands() {
        let a = (1, WindowId::from_raw(11));

        // The pointer is already outside A. This rejects both a delayed
        // initial ENTER and an otherwise owner-valid timer renewal from A.
        assert!(!minimized_preview_may_activate(
            None,
            None,
            a.0,
            a.1,
            7,
            false,
            Some(false)
        ));
        assert!(!minimized_preview_may_activate(
            Some(a),
            Some(7),
            a.0,
            a.1,
            7,
            true,
            Some(false)
        ));

        assert!(minimized_preview_may_activate(
            None,
            None,
            a.0,
            a.1,
            7,
            false,
            Some(true)
        ));
        // Backends without a reliable global pointer query retain the
        // generation/ownership-only fallback.
        assert!(minimized_preview_may_activate(
            None, None, a.0, a.1, 7, false, None
        ));
    }
}
