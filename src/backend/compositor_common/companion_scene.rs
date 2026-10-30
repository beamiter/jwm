//! One opt-in compositor body. IPC and atlas raster preparation live exclusively
//! on the worker. Native renderers receive immutable RGBA, never a socket/model.
use crate::backend::update_notifier::AsyncUpdateNotifier;
use arc_swap::{ArcSwap, ArcSwapOption};
use companion_client::Client;
use companion_protocol::{Host, Rect, Reply, Request, RetirementCapability, Token};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

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
                let host = Host {
                    id: format!("jwm-scene-{}", std::process::id()),
                    incarnation: 1,
                    surface_id: "single-scene-node".into(),
                    retirement: RetirementCapability::None,
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
                loop {
                    for reply in client.poll()? {
                        match reply {
                            Reply::Accepted { .. } => accepted = true,
                            Reply::Rejected { .. } => return Ok(()),
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
                                token = Some(t);
                                cache_key.clear();
                                next_frame = Instant::now();
                            }
                            Reply::Revoke { .. } => {
                                token = None;
                                runtime.node.store(None);
                                notifier.notify();
                                hidden = true;
                                // No Retired: submitted/drawn is not a presented fence.
                            }
                            Reply::Snapshot {
                                token: t,
                                pose: p,
                                mode,
                                recovery_blocked,
                                ..
                            } => {
                                let was_hidden = presentation_hidden;
                                presentation_hidden = mode == companion_protocol::Presence::Hidden;
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
                                candidates: safe_anchor(
                                    viewport,
                                    &desktop.occupied,
                                    BODY_WIDTH,
                                    BODY_HEIGHT,
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
                    let safe = desktop
                        .viewport
                        .and_then(|v| safe_anchor(v, &desktop.occupied, BODY_WIDTH, BODY_HEIGHT));
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

/// Only exposed desktop pixels are usable until authenticated native-window
/// anchor mapping exists. Never place a body over a client's input/selection.
pub(crate) fn safe_anchor(
    viewport: Rect,
    occupied: &[Rect],
    width: f64,
    height: f64,
) -> Option<[f32; 4]> {
    if !viewport.valid() || occupied.len() > companion_protocol::MAX_REGIONS {
        return None;
    }
    let margin = 12.;
    for (x, y) in [
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
    ] {
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
