//! One opt-in compositor body. IPC and atlas raster preparation live exclusively
//! on the worker. Native renderers receive immutable RGBA, never a socket/model.
use crate::backend::update_notifier::AsyncUpdateNotifier;
use arc_swap::{ArcSwap, ArcSwapOption};
use companion_client::Client;
use companion_protocol::{Host, Rect, Reply, Request, RetirementCapability, Token};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

/// A ticket describes the immutable scene used to render one framebuffer.
/// Submission alone never consumes it; native completion must confirm it.
#[derive(Clone, Debug)]
pub(crate) struct FrameEvidence {
    pub captured_at: Instant,
    route: PresentationRoute,
    pub token: Option<Token>,
    pub body_drawn: bool,
    pub rect: Option<[f32; 4]>,
}
impl FrameEvidence {
    pub(crate) fn capture(node: Option<&Arc<RasterNode>>) -> Self {
        Self {
            captured_at: Instant::now(),
            route: route(),
            token: node.map(|n| n.token.clone()),
            body_drawn: false,
            rect: node.map(|n| n.rect),
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PresentationRoute {
    generation: u64,
    outputs: Vec<String>,
    supported: bool,
}
static ROUTE: OnceLock<ArcSwap<PresentationRoute>> = OnceLock::new();
static EVIDENCE: OnceLock<Mutex<Vec<(PresentationRoute, String, FrameEvidence, u64)>>> =
    OnceLock::new();
/// Backend-owned output identities, never producer input. A topology change
/// invalidates every queued observation from the old route.
pub(crate) fn presentation_route(mut outputs: Vec<String>, supported: bool) {
    outputs.sort();
    outputs.dedup();
    let supported = supported && outputs.len() <= 64;
    outputs.truncate(64);
    let current = ROUTE.get_or_init(|| ArcSwap::from_pointee(PresentationRoute::default()));
    let old = current.load();
    let supported = supported && !outputs.is_empty();
    if old.outputs != outputs || old.supported != supported {
        let generation = old.generation.saturating_add(1);
        current.store(Arc::new(PresentationRoute {
            generation,
            outputs,
            supported: supported && generation < u64::MAX,
        }));
    }
}
pub(crate) fn invalidate_presentation_evidence() {
    let current = ROUTE.get_or_init(|| ArcSwap::from_pointee(PresentationRoute::default()));
    let mut next = (**current.load()).clone();
    next.generation = next.generation.saturating_add(1);
    next.supported &= next.generation < u64::MAX;
    current.store(Arc::new(next));
}
fn route() -> PresentationRoute {
    (**ROUTE
        .get_or_init(|| ArcSwap::from_pointee(PresentationRoute::default()))
        .load())
    .clone()
}
pub(crate) fn confirm_frame_evidence(output: &str, ticket: FrameEvidence, serial: u64) {
    if serial == 0 {
        return;
    }
    let route = ticket.route.clone();
    if route != self::route()
        || !route.supported
        || !route.outputs.iter().any(|name| name == output)
    {
        return;
    }
    if let Ok(mut pending) = EVIDENCE.get_or_init(Default::default).try_lock() {
        // Keep only the latest observation per output, bounded independently
        // of producer rate. Losing an observation cannot manufacture proof.
        pending.retain(|(_, name, _, _)| name != output);
        if pending.len() < 64 {
            pending.push((route, output.into(), ticket, serial));
        }
    }
}
fn acknowledge_retirement(
    expected: &PresentationRoute,
    frame_route: &PresentationRoute,
    output: String,
    frame: &FrameEvidence,
    after: Instant,
    serial: u64,
    seen: &mut std::collections::HashMap<String, u64>,
) -> bool {
    if serial == 0
        || expected != frame_route
        || !expected.supported
        || frame.token.is_some()
        || frame.captured_at < after
        || !expected.outputs.contains(&output)
    {
        return false;
    }
    seen.insert(output, serial);
    expected.outputs.iter().all(|name| seen.contains_key(name))
}
pub(crate) fn native_presentation_supported() -> bool {
    route().supported
}
pub(crate) fn native_presentation_reason() -> &'static str {
    if native_presentation_supported() {
        "native_completion_callbacks_enabled; per-token frame evidence required"
    } else {
        "unsupported_backend: requires DRM_CAP_TIMESTAMP_MONOTONIC and validated KMS vblank, or GLX OML completed SBC; EGL/nested submission is insufficient"
    }
}

const BODY_WIDTH: f64 =
    (eva_character_atlas::WIDTH as u32 * crate::backend::compositor_font::GLYPH_W) as f64;
const BODY_HEIGHT: f64 =
    (eva_character_atlas::HEIGHT as u32 * crate::backend::compositor_font::GLYPH_H) as f64;
#[derive(Debug)]
pub struct RasterNode {
    pub revision: u64,
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub rect: [f32; 4],
    pub accessible_label: String,
    /// Identity/epoch and target remain attached to the raster; a later focus
    /// does not retarget a callback or grant.
    pub token: Token,
    pub target: String,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Desktop {
    pub target: String,
    pub viewport: Option<Rect>,
    /// Native window geometry from a live registered provider, never a title/PID guess.
    pub anchor: Option<Rect>,
    pub occupied: Vec<Rect>,
    pub suppressed: bool,
    pub reduced_motion: bool,
}
struct Runtime {
    desktop: ArcSwap<Desktop>,
    node: ArcSwapOption<RasterNode>,
}
static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
pub(crate) fn enabled() -> bool {
    RUNTIME.get().is_some()
}
pub(crate) fn publish(desktop: Desktop) {
    if let Some(runtime) = RUNTIME.get() {
        if **runtime.desktop.load() != desktop {
            runtime.desktop.store(Arc::new(desktop));
        }
    }
}
pub(crate) fn snapshot() -> Option<Arc<RasterNode>> {
    RUNTIME.get()?.node.load_full()
}
pub(crate) fn start(notifier: Option<AsyncUpdateNotifier>) {
    if std::env::var("JWM_COMPANION_SCENE").as_deref() != Ok("1")
        || std::env::var_os("SSH_CONNECTION").is_some()
    {
        return;
    }
    let (Some(notifier), Ok(session), Some(runtime_dir)) = (
        notifier,
        std::env::var("XDG_SESSION_ID"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    ) else {
        return;
    };
    let path = std::env::var_os("EVA_COMPANION_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(runtime_dir).join("jcompanion/eva.sock"));
    let runtime = Arc::new(Runtime {
        desktop: ArcSwap::from_pointee(Desktop::default()),
        node: ArcSwapOption::empty(),
    });
    if RUNTIME.set(runtime.clone()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("jwm-companion-scene".into())
        .spawn(move || {
            let result = (|| -> std::io::Result<()> {
                let mut incarnation = 0u64;
                'connection: loop {
                    incarnation = incarnation.checked_add(1).ok_or_else(|| {
                        std::io::Error::other("companion host incarnation exhausted")
                    })?;
                    let host = Host {
                        id: format!("jwm-scene-{}", std::process::id()),
                        incarnation,
                        surface_id: "single-scene-node".into(),
                        retirement: if native_presentation_supported() {
                            RetirementCapability::CompositorPresented
                        } else {
                            RetirementCapability::None
                        },
                    };
                    // Daemon startup order must not permanently disable the scene.
                    // Retry only pre-connection: after any possible grant, a loss
                    // remains fail-closed until genuine retirement recovery.
                    let mut delay = Duration::from_millis(250);
                    let mut client = loop {
                        match Client::connect_compositor(&path, session.clone(), host.clone()) {
                            Ok(client) => break client,
                            Err(_) => {
                                std::thread::sleep(delay);
                                delay = (delay * 2).min(Duration::from_secs(5));
                            }
                        }
                    };
                    let mut accepted = false;
                    let mut token: Option<Token> = None;
                    let mut ownership: Option<Token> = None;
                    let mut highest_epoch = 0u64;
                    let mut pose = eva_character_atlas::Pose::Idle;
                    let mut skin = eva_character_atlas::Skin::Cat;
                    let mut motion = eva_character_atlas::Motion::Calm;
                    let mut revision = 0u64;
                    let mut geometry_revision = 0u64;
                    let mut last_desktop = Desktop::default();
                    let mut started = Instant::now();
                    let mut next_frame = Instant::now();
                    let mut cache_key = String::new();
                    let mut hidden = true;
                    let mut presentation_hidden = false;
                    let mut retirement: Option<(Token, Instant)> = None;
                    let mut retired_outputs = std::collections::HashMap::new();
                    let mut presented_token: Option<Token> = None;
                    let mut observed_route = route();
                    loop {
                        for reply in client.poll()? {
                            match reply {
                                Reply::Accepted { .. } => accepted = true,
                                Reply::Rejected { .. } => {
                                    if ownership.is_none() && retirement.is_none() {
                                        std::thread::sleep(Duration::from_millis(500));
                                        continue 'connection;
                                    }
                                    return Ok(());
                                }
                                Reply::Prepare { epoch } => {
                                    client.send(Request::Prepared { epoch })?;
                                }
                                Reply::Grant { token: t, .. } => {
                                    if t.host_id != host.id
                                        || t.incarnation != host.incarnation
                                        || t.surface_id != host.surface_id
                                        || t.boot_id != client.boot_id()
                                        || t.epoch <= highest_epoch
                                    {
                                        return Ok(());
                                    }
                                    highest_epoch = t.epoch;
                                    ownership = Some(t.clone());
                                    token = Some(t);
                                    cache_key.clear();
                                    next_frame = Instant::now();
                                }
                                Reply::Revoke { token: revoked } => {
                                    if ownership.as_ref() != Some(&revoked) {
                                        continue;
                                    }
                                    retirement = ownership.take().map(|t| (t, Instant::now()));
                                    token = None;
                                    retired_outputs.clear();
                                    presented_token = None;
                                    runtime.node.store(None);
                                    notifier.notify();
                                    hidden = true;
                                    // Wait for body-free native completion on every active output.
                                }
                                Reply::Snapshot {
                                    token: t,
                                    pose: p,
                                    mode,
                                    recovery_blocked,
                                    ..
                                } => {
                                    let was_hidden = presentation_hidden;
                                    presentation_hidden =
                                        mode == companion_protocol::Presence::Hidden;
                                    if was_hidden && !presentation_hidden {
                                        next_frame = Instant::now();
                                    }
                                    // Hidden suppresses an already-authorized scene, not ownership.
                                    // Only an exact current token can survive the snapshot.
                                    if recovery_blocked
                                        || t.as_ref().is_none_or(|t| t.host_id != host.id)
                                    {
                                        token = None;
                                    } else if !snapshot_keeps_grant(&token, &t) {
                                        // Snapshot is never a grant; queued state
                                        // cannot resurrect a revoked epoch.
                                        token = None;
                                    }
                                    if let Some(p) = eva_character_atlas::Pose::from_key(&p) {
                                        if p != pose {
                                            pose = p;
                                            started = Instant::now();
                                            next_frame = Instant::now();
                                        }
                                    }
                                }
                                Reply::Appearance { skin: s, motion: m } => {
                                    if let Some(s) = eva_character_atlas::Skin::from_key(&s) {
                                        skin = s;
                                    }
                                    motion = match m.as_str() {
                                        "full" => eva_character_atlas::Motion::Full,
                                        "static" => eva_character_atlas::Motion::Static,
                                        _ => eva_character_atlas::Motion::Calm,
                                    };
                                    next_frame = Instant::now();
                                    cache_key.clear();
                                }
                                _ => {}
                            }
                        }
                        let current_route = route();
                        // Capability negotiation may restart only with no outstanding
                        // ownership. A lost/disabled backend with a grant remains
                        // fail-closed until genuine retirement is observed.
                        if ownership.is_none()
                            && retirement.is_none()
                            && current_route.supported
                                != (host.retirement != RetirementCapability::None)
                        {
                            break;
                        }
                        if current_route != observed_route {
                            retired_outputs.clear();
                            observed_route = current_route.clone();
                        }
                        let evidence = EVIDENCE
                            .get_or_init(Default::default)
                            .lock()
                            .map(|mut pending| std::mem::take(&mut *pending))
                            .unwrap_or_default();
                        for (frame_route, output, frame, serial) in evidence {
                            if host.retirement == RetirementCapability::None
                                || frame_route != current_route
                                || !current_route.supported
                            {
                                continue;
                            }
                            if let Some(ref current) = token {
                                if frame.body_drawn
                                    && frame.token.as_ref() == Some(current)
                                    && presented_token.as_ref() != Some(current)
                                {
                                    client.send(Request::Presented {
                                        token: current.clone(),
                                    })?;
                                    presented_token = Some(current.clone());
                                }
                            }
                            if let Some((ref revoked, after)) = retirement {
                                if acknowledge_retirement(
                                    &current_route,
                                    &frame_route,
                                    output,
                                    &frame,
                                    after,
                                    serial,
                                    &mut retired_outputs,
                                ) {
                                    client.send(Request::Retired {
                                        token: revoked.clone(),
                                        presentation_serial: serial,
                                    })?;
                                    retirement = None;
                                    retired_outputs.clear();
                                }
                            }
                        }
                        let desktop = runtime.desktop.load_full();
                        if accepted && *desktop != last_desktop {
                            last_desktop = (*desktop).clone();
                            cache_key.clear();
                            next_frame = Instant::now();
                            client.send(Request::Focus {
                                focused: !desktop.suppressed,
                            })?;
                            if let Some(viewport) = desktop.viewport {
                                geometry_revision = geometry_revision.saturating_add(1);
                                client.send(Request::Geometry {
                                    revision: geometry_revision,
                                    viewport,
                                    candidates: safe_anchor_near(
                                        viewport,
                                        &desktop.occupied,
                                        BODY_WIDTH,
                                        BODY_HEIGHT,
                                        desktop.anchor,
                                    )
                                    .map(|r| Rect {
                                        x: r[0] as f64,
                                        y: r[1] as f64,
                                        width: r[2] as f64,
                                        height: r[3] as f64,
                                    })
                                    .into_iter()
                                    .collect(),
                                    exclusions: desktop
                                        .occupied
                                        .iter()
                                        .take(companion_protocol::MAX_REGIONS)
                                        .copied()
                                        .collect(),
                                })?;
                            }
                        }
                        let safe = desktop.viewport.and_then(|v| {
                            safe_anchor_near(
                                v,
                                &desktop.occupied,
                                BODY_WIDTH,
                                BODY_HEIGHT,
                                desktop.anchor,
                            )
                        });
                        if token.is_none()
                            || presentation_hidden
                            || desktop.suppressed
                            || safe.is_none()
                        {
                            if !hidden {
                                runtime.node.store(None);
                                notifier.notify();
                                hidden = true;
                                cache_key.clear();
                            }
                        } else if Instant::now() >= next_frame {
                            let mut request = eva_character_atlas::Request::new(skin, pose);
                            request.motion = motion;
                            request.reduced_motion = desktop.reduced_motion;
                            request.elapsed_ms =
                                started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                            let Some(frame) = eva_character_atlas::sample(request) else {
                                continue;
                            };
                            let rect = safe.unwrap();
                            let key = format!(
                                "{}:{}:{}:{:?}:{}",
                                skin.key(),
                                pose.key(),
                                frame.frame_index,
                                rect,
                                desktop.target
                            );
                            if cache_key != key || hidden {
                                let (pixels, width, height) = rasterize(&frame, skin);
                                revision = revision.saturating_add(1);
                                runtime.node.store(Some(Arc::new(RasterNode {
                                    revision,
                                    pixels,
                                    width,
                                    height,
                                    rect,
                                    accessible_label: frame.accessible_label.clone(),
                                    token: token.clone().unwrap(),
                                    target: desktop.target.clone(),
                                })));
                                notifier.notify();
                                cache_key = key;
                                hidden = false;
                            }
                            next_frame = Instant::now()
                                + Duration::from_millis(
                                    frame.next_frame_in_ms.unwrap_or(60_000).max(40),
                                );
                        }
                        std::thread::sleep(Duration::from_millis(40));
                    }
                }
            })();
            // Disconnect never restores an independent/offline body.
            runtime.node.store(None);
            notifier.notify();
            if result.is_err() {
                log::debug!("companion scene disconnected; body suppressed");
            }
        });
}
fn snapshot_keeps_grant(current: &Option<Token>, incoming: &Option<Token>) -> bool {
    current.is_some() && current == incoming
}

/// Legacy exposed-desktop placement when no provider has a current native
/// association. It grants no window or pane identity.
#[cfg(test)]
fn safe_anchor(viewport: Rect, occupied: &[Rect], width: f64, height: f64) -> Option<[f32; 4]> {
    safe_anchor_near(viewport, occupied, width, height, None)
}

/// Place beside a trusted terminal window without covering any client input.
/// With an anchor, insufficient space hides the body instead of moving it to
/// an unrelated desktop corner. This remains geometry, not retirement proof.
fn safe_anchor_near(
    viewport: Rect,
    occupied: &[Rect],
    width: f64,
    height: f64,
    anchor: Option<Rect>,
) -> Option<[f32; 4]> {
    if !viewport.valid() || occupied.len() > companion_protocol::MAX_REGIONS {
        return None;
    }
    if !width.is_finite() || !height.is_finite() || width <= 0. || height <= 0. {
        return None;
    }
    let margin = 12.;
    let candidates = if let Some(anchor) = anchor {
        if !anchor.valid() {
            return None;
        }
        vec![
            (anchor.x + anchor.width + margin, anchor.y),
            (anchor.x - width - margin, anchor.y),
            (
                anchor.x + anchor.width + margin,
                anchor.y + anchor.height - height,
            ),
            (anchor.x - width - margin, anchor.y + anchor.height - height),
            (anchor.x, anchor.y + anchor.height + margin),
            (
                anchor.x + anchor.width - width,
                anchor.y + anchor.height + margin,
            ),
            (anchor.x, anchor.y - height - margin),
            (anchor.x + anchor.width - width, anchor.y - height - margin),
        ]
    } else {
        vec![
            (
                viewport.x + viewport.width - width - margin,
                viewport.y + margin,
            ),
            (viewport.x + margin, viewport.y + margin),
            (
                viewport.x + viewport.width - width - margin,
                viewport.y + viewport.height - height - margin,
            ),
            (
                viewport.x + margin,
                viewport.y + viewport.height - height - margin,
            ),
        ]
    };
    for (x, y) in candidates {
        let r = Rect {
            x,
            y,
            width,
            height,
        };
        if viewport.contains(r) && occupied.iter().all(|o| o.valid() && !o.overlaps(r)) {
            return Some([x as f32, y as f32, width as f32, height as f32]);
        }
    }
    None
}
fn rasterize(
    frame: &eva_character_atlas::Frame,
    skin: eva_character_atlas::Skin,
) -> (Vec<u8>, u32, u32) {
    // The built-in 6x10 printable-ASCII font is deterministic and monospace;
    // shape once per atlas frame on this worker, never entire terminal history.
    let (_, cw, ch) = crate::backend::compositor_font::render_text_to_rgba("M", 1, [255; 4]);
    let (w, h) = (cw * frame.width as u32, ch * frame.height as u32);
    let mut pixels = vec![0; w as usize * h as usize * 4];
    let palette = skin.palette(false);
    let mut glyphs = std::collections::HashMap::new();
    for (index, cell) in frame.cells.iter().enumerate() {
        if cell.glyph == ' ' {
            continue;
        }
        let color = palette.color(cell.ink);
        let (glyph, gw, gh) = glyphs.entry((cell.glyph, color)).or_insert_with(|| {
            crate::backend::compositor_font::render_text_to_rgba(
                &cell.glyph.to_string(),
                1,
                [color[0], color[1], color[2], 255],
            )
        });
        let ox = (index % frame.width) as u32 * cw;
        let oy = (index / frame.width) as u32 * ch;
        for y in 0..(*gh).min(ch) {
            for x in 0..(*gw).min(cw) {
                let src = ((y * *gw + x) * 4) as usize;
                let dst = (((oy + y) * w + ox + x) * 4) as usize;
                pixels[dst..dst + 4].copy_from_slice(&glyph[src..src + 4]);
            }
        }
    }
    (pixels, w, h)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retirement_requires_body_free_new_frame_on_every_current_output() {
        let route = PresentationRoute {
            generation: 1,
            outputs: vec!["DP-1".into(), "HDMI-1".into()],
            supported: true,
        };
        let after = Instant::now();
        let mut ticket = FrameEvidence {
            captured_at: after,
            route: route.clone(),
            token: None,
            body_drawn: false,
            rect: None,
        };
        let mut seen = std::collections::HashMap::new();
        assert!(!acknowledge_retirement(
            &route,
            &route,
            "DP-1".into(),
            &ticket,
            after,
            0,
            &mut seen
        ));
        ticket.captured_at = after - Duration::from_millis(1);
        assert!(!acknowledge_retirement(
            &route,
            &route,
            "DP-1".into(),
            &ticket,
            after,
            1,
            &mut seen
        ));
        assert!(seen.is_empty());
        ticket.captured_at = after;
        assert!(!acknowledge_retirement(
            &route,
            &route,
            "DP-1".into(),
            &ticket,
            after,
            2,
            &mut seen
        ));
        let changed = PresentationRoute {
            generation: 2,
            outputs: vec!["DP-1".into()],
            supported: true,
        };
        assert!(!acknowledge_retirement(
            &route,
            &changed,
            "HDMI-1".into(),
            &ticket,
            after,
            3,
            &mut seen
        ));
        assert!(!acknowledge_retirement(
            &route,
            &route,
            "unknown".into(),
            &ticket,
            after,
            3,
            &mut seen
        ));
        let mut stale_generation = route.clone();
        stale_generation.generation += 1;
        assert!(!acknowledge_retirement(
            &route,
            &stale_generation,
            "HDMI-1".into(),
            &ticket,
            after,
            4,
            &mut seen
        ));
        ticket.token = Some(Token {
            companion_id: "eva".into(),
            host_id: "host".into(),
            incarnation: 1,
            boot_id: "boot".into(),
            epoch: 1,
            surface_id: "scene".into(),
        });
        assert!(!acknowledge_retirement(
            &route,
            &route,
            "HDMI-1".into(),
            &ticket,
            after,
            4,
            &mut seen
        ));
        ticket.token = None;
        assert!(acknowledge_retirement(
            &route,
            &route,
            "HDMI-1".into(),
            &ticket,
            after,
            4,
            &mut seen
        ));
    }

    #[test]
    fn snapshots_cannot_grant_or_resurrect_revoked_epochs() {
        let token = Token {
            companion_id: "eva".into(),
            host_id: "jwm".into(),
            incarnation: 1,
            boot_id: "boot".into(),
            epoch: 1,
            surface_id: "scene".into(),
        };
        assert!(!snapshot_keeps_grant(&None, &Some(token.clone())));
        assert!(!snapshot_keeps_grant(&None, &None));
        assert!(snapshot_keeps_grant(
            &Some(token.clone()),
            &Some(token.clone())
        ));
        let mut stale = token.clone();
        stale.epoch += 1;
        assert!(!snapshot_keeps_grant(&Some(token), &Some(stale)));
    }
    #[test]
    fn registered_window_edges_follow_geometry_and_never_cover_input_or_another_client() {
        let viewport = Rect {
            x: -800.,
            y: 0.,
            width: 800.,
            height: 600.,
        };
        let window = Rect {
            x: -600.,
            y: 100.,
            width: 300.,
            height: 300.,
        };
        let first = safe_anchor_near(viewport, &[window], 120., 80., Some(window)).unwrap();
        assert_eq!(first, [-288., 100., 120., 80.]);
        let moved = Rect { y: 130., ..window };
        let second = safe_anchor_near(viewport, &[moved], 120., 80., Some(moved)).unwrap();
        assert_eq!(second[1], 130.);
        assert!(safe_anchor_near(viewport, &[viewport], 120., 80., Some(window)).is_none());
        assert!(
            safe_anchor_near(
                viewport,
                &[],
                120.,
                80.,
                Some(Rect {
                    width: f64::NAN,
                    ..window
                })
            )
            .is_none()
        );
    }
    #[test]
    fn no_body_over_full_client_or_outside_viewport() {
        let v = Rect {
            x: 0.,
            y: 0.,
            width: 800.,
            height: 600.,
        };
        assert!(safe_anchor(v, &[v], 240., 176.).is_none());
        assert!(safe_anchor(Rect { width: 100., ..v }, &[], 240., 176.).is_none());
        assert!(safe_anchor(v, &[], 240., 176.).is_some());
    }
    #[test]
    fn every_skin_pose_raster_fits_its_safe_rectangle_without_static_ticks() {
        for skin in eva_character_atlas::Skin::ALL {
            for pose in eva_character_atlas::Pose::ALL {
                let mut request = eva_character_atlas::Request::new(skin, pose);
                request.motion = eva_character_atlas::Motion::Static;
                let frame = eva_character_atlas::sample(request).unwrap();
                assert!(frame.next_frame_in_ms.is_none());
                let (p, w, h) = rasterize(&frame, skin);
                assert_eq!((w as f64, h as f64), (BODY_WIDTH, BODY_HEIGHT));
                assert!(p.chunks_exact(4).any(|p| p[3] > 0));
            }
        }
    }
    #[test]
    fn raster_has_one_atlas_frame_and_transparent_background() {
        let f = eva_character_atlas::sample(eva_character_atlas::Request::new(
            eva_character_atlas::Skin::Cat,
            eva_character_atlas::Pose::Idle,
        ))
        .unwrap();
        let (p, w, h) = rasterize(&f, eva_character_atlas::Skin::Cat);
        assert_eq!(p.len(), (w * h * 4) as usize);
        assert_eq!((w as f64, h as f64), (BODY_WIDTH, BODY_HEIGHT));
        assert!(p.chunks_exact(4).any(|p| p[3] == 0));
        assert!(p.chunks_exact(4).any(|p| p[3] > 0));
        if let Ok(path) = std::env::var("EVA_COMPANION_RASTER_PREVIEW") {
            let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
            for px in p.chunks_exact(4) {
                let a = px[3] as u32;
                for (i, bg) in [13u32, 17, 27].into_iter().enumerate() {
                    ppm.push(((px[i] as u32 * a + bg * (255 - a)) / 255) as u8);
                }
            }
            std::fs::write(path, ppm).unwrap();
        }
    }
}
