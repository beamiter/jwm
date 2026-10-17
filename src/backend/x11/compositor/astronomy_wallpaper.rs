//! Astronomy is a wallpaper material, rendered below every client window.
use super::*;
use crate::backend::compositor_common::animated_wallpaper::{
    AstronomyDraw, CelestialArcball, CelestialBody, CelestialRotation, CelestialView,
    POWER_QUERY_INTERVAL, WallpaperClock, astronomy_draw_plan, astronomy_frame_interval,
    celestial_assignments, lunar_phase_light, material_load_frame, presentation_offset,
    resolve_cloud_texture, resolve_earth_texture, resolve_moon_detail_texture,
    resolve_moon_texture, resolve_sky_texture, sky_rotation, solar_direction, transform_direction,
    wall_clock_seconds,
};
use crate::backend::compositor_common::panorama::{PanoramaDrag, PanoramaView, panorama_rotation};
use glow::HasContext;
use std::time::{Duration, Instant, SystemTime};

const VERTEX: &str = "#version 330 core\nvoid main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);gl_Position=vec4(p*2.0-1.0,0.0,1.0);}";
const FRAGMENT: &str = include_str!("shaders/animated_wallpaper.frag");
/// Pointer motion is coalesced independently of the low-power astronomy clock.
/// A stationary grab still uses the ordinary physical-light refresh cadence.
const DRAG_FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);
const MIN_BODY_ZOOM: f64 = 0.35;
const MAX_BODY_ZOOM: f64 = 8.0;
const ZOOM_LOG_STEP: f64 = 0.15;

/// Logarithmic wheel motion is reversible and independent of event grouping.
/// Requested, admitted and actually presented scales are deliberately separate:
/// a fast wheel burst must not change hit testing before its pixels are visible.
#[derive(Clone, Copy, Default)]
struct AstronomyZoom {
    requested_log: f64,
    admitted_log: f64,
    presented: Option<f32>,
}

impl AstronomyZoom {
    fn request(&mut self, steps: f64) -> bool {
        if !steps.is_finite() || steps == 0.0 {
            return false;
        }
        let next = (self.requested_log + steps.clamp(-32.0, 32.0) * ZOOM_LOG_STEP)
            .clamp(MIN_BODY_ZOOM.ln(), MAX_BODY_ZOOM.ln());
        if next == self.requested_log {
            return false;
        }
        self.requested_log = next;
        true
    }

    fn admit(&mut self) {
        self.admitted_log = self.requested_log;
    }

    fn scale(&self) -> f32 {
        self.admitted_log.exp() as f32
    }

    fn presented_scale(&self) -> f32 {
        self.presented.unwrap_or(1.0)
    }
}

struct AstronomyDrag {
    body: CelestialBody,
    canvas: AstronomyDraw,
    arcball: CelestialArcball,
    orientation: CelestialRotation,
    pending_point: [f64; 2],
    pending_before_begin: bool,
    zoom_before_begin: AstronomyZoom,
}

/// Close-up surfaces are optional and independent from the required startup
/// textures. A missing, oversized or failed detail image never hides a globe.
#[derive(Default)]
struct AstronomyDetail {
    pending: Option<wallpaper::DeferredWallpaperLoad>,
    texture: Option<glow::Texture>,
    attempted: bool,
}

struct AstronomySkyDrag {
    canvas: AstronomyDraw,
    gesture: PanoramaDrag,
    orientation: CelestialRotation,
    pending_point: [f64; 2],
    pending_before_begin: bool,
    view_before_begin: PanoramaView,
    requested_before_begin: Option<PanoramaView>,
}

struct AstronomyRelease {
    /// Continue showing this exact frame until both the release offset and a
    /// fresh physical-time sample are admitted together by update_clock().
    presented: [f32; 9],
    offset: CelestialRotation,
}

/// X11 positions identify framebuffer pixels. Use the same pixel center and
/// bottom-left coordinates as gl_FragCoord, retaining the captured canvas when
/// the pointer crosses a monitor boundary during an active grab.
fn canvas_pointer(
    canvas: &AstronomyDraw,
    root: [f64; 2],
    screen_height: u32,
) -> Option<([f64; 2], [f64; 2])> {
    if !root.iter().all(|component| component.is_finite()) {
        return None;
    }
    let pixel = [root[0] + 0.5, f64::from(screen_height) - root[1] - 0.5];
    let radius = f64::from(canvas.radius.max(1.0));
    let point = std::array::from_fn(|axis| {
        (pixel[axis] - f64::from(canvas.origin[axis]) - f64::from(canvas.size[axis]) * 0.5) / radius
    });
    point
        .iter()
        .all(|component| component.is_finite())
        .then_some((pixel, point))
}

fn panorama_pointer(
    canvas: &AstronomyDraw,
    root: [f64; 2],
    screen_height: u32,
) -> Option<[f64; 2]> {
    let (pixel, _) = canvas_pointer(canvas, root, screen_height)?;
    Some(std::array::from_fn(|axis| {
        pixel[axis] - f64::from(canvas.origin[axis])
    }))
}

fn decode_astronomy_sky(
    path: &str,
    max_edge: u32,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Option<WallpaperImageData> {
    use std::sync::atomic::Ordering;
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let img = match crate::backend::compositor_common::image_source::open_regular_image_bounded(
        path,
        4096,
        2048,
        32 * 1024 * 1024,
    ) {
        Ok(img) => img,
        Err(error) => {
            log::warn!(
                "astronomy wallpaper: optional sky unavailable; using procedural sky: {error}"
            );
            return None;
        }
    };
    if cancelled.load(Ordering::Acquire) || img.width() != img.height().saturating_mul(2) {
        return None;
    }
    let edge = if max_edge == 0 {
        4096
    } else {
        max_edge.min(4096) & !1
    };
    if edge == 0 {
        return None;
    }
    let img = if img.width() > edge {
        // These are independent data channels. Inverse constellation intensity
        // is not opacity: preserve its RGB even where alpha is zero. image's
        // resampler filters channels independently and does not premultiply.
        img.resize(edge, edge / 2, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let rgba = img.to_rgba8();
    Some(WallpaperImageData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw().into(),
        mode: WallpaperMode::Stretch,
    })
}

fn configured_views(behavior: &crate::config::BehaviorConfig) -> (CelestialView, CelestialView) {
    (
        CelestialView {
            rotating: behavior.wallpaper_earth_view == "rotating",
            latitude: behavior.wallpaper_earth_latitude,
            longitude: behavior.wallpaper_earth_longitude,
            roll: behavior.wallpaper_earth_roll,
            axis: behavior.wallpaper_earth_rotation_axis,
        },
        CelestialView {
            rotating: behavior.wallpaper_moon_view == "rotating",
            latitude: behavior.wallpaper_moon_latitude,
            longitude: behavior.wallpaper_moon_longitude,
            roll: behavior.wallpaper_moon_roll,
            axis: behavior.wallpaper_moon_rotation_axis,
        },
    )
}

/// Uniform locations belong to one successfully linked program. Keeping them
/// with its handle prevents stale locations from surviving program recreation.
struct AstronomyProgram {
    program: glow::Program,
    sun_direction: Option<glow::UniformLocation>,
    view_to_body: Option<glow::UniformLocation>,
    earth_map: Option<glow::UniformLocation>,
    moon_material: Option<glow::UniformLocation>,
    cloud_map: Option<glow::UniformLocation>,
    clouds_enabled: Option<glow::UniformLocation>,
    sky_map: Option<glow::UniformLocation>,
    sky_enabled: Option<glow::UniformLocation>,
    sky_rotation: Option<glow::UniformLocation>,
    panorama_enabled: Option<glow::UniformLocation>,
    panorama_view: Option<glow::UniformLocation>,
    panorama_fov: Option<glow::UniformLocation>,
    canvas_origin: Option<glow::UniformLocation>,
    resolution: Option<glow::UniformLocation>,
    sphere_radius: Option<glow::UniformLocation>,
    surface_detail: Option<glow::UniformLocation>,
}

impl AstronomyProgram {
    unsafe fn new(gl: &glow::Context, program: glow::Program) -> Self {
        unsafe {
            Self {
                program,
                sun_direction: gl.get_uniform_location(program, "sun_direction"),
                view_to_body: gl.get_uniform_location(program, "view_to_body"),
                earth_map: gl.get_uniform_location(program, "earth_map"),
                moon_material: gl.get_uniform_location(program, "moon_material"),
                cloud_map: gl.get_uniform_location(program, "cloud_map"),
                clouds_enabled: gl.get_uniform_location(program, "clouds_enabled"),
                sky_map: gl.get_uniform_location(program, "sky_map"),
                sky_enabled: gl.get_uniform_location(program, "sky_enabled"),
                sky_rotation: gl.get_uniform_location(program, "sky_rotation"),
                panorama_enabled: gl.get_uniform_location(program, "panorama_enabled"),
                panorama_view: gl.get_uniform_location(program, "panorama_view"),
                panorama_fov: gl.get_uniform_location(program, "panorama_fov"),
                canvas_origin: gl.get_uniform_location(program, "canvas_origin"),
                resolution: gl.get_uniform_location(program, "resolution"),
                sphere_radius: gl.get_uniform_location(program, "sphere_radius"),
                surface_detail: gl.get_uniform_location(program, "surface_detail"),
            }
        }
    }
}

#[derive(Default)]
pub(super) struct AstronomyWallpaper {
    cache: procedural_wallpaper_cache::ProceduralFrameCache,
    enabled: bool,
    path: String,
    configured_path: String,
    pending: Option<wallpaper::DeferredWallpaperLoad>,
    texture: Option<glow::Texture>,
    moon_configured_path: String,
    moon_pending: Option<wallpaper::DeferredWallpaperLoad>,
    moon_texture: Option<glow::Texture>,
    cloud_pending: Option<wallpaper::DeferredWallpaperLoad>,
    cloud_texture: Option<glow::Texture>,
    sky_pending: Option<wallpaper::DeferredWallpaperLoad>,
    sky_texture: Option<glow::Texture>,
    sky_path: String,
    dual_material: bool,
    sky_period: u32,
    earth_period: u32,
    earth_view: CelestialView,
    moon_view: CelestialView,
    moon_period: u32,
    // One session-only orientation per body, shared by its output assignments.
    // None preserves the original no-interaction f32 matrix bit-for-bit.
    earth_offset: Option<CelestialRotation>,
    moon_offset: Option<CelestialRotation>,
    earth_release: Option<AstronomyRelease>,
    moon_release: Option<AstronomyRelease>,
    earth_zoom: AstronomyZoom,
    moon_zoom: AstronomyZoom,
    earth_detail: AstronomyDetail,
    moon_detail: AstronomyDetail,
    earth_width: u32,
    moon_width: u32,
    max_texture_edge: u32,
    presented_earth: Option<[f32; 9]>,
    presented_moon: Option<[f32; 9]>,
    drag: Option<AstronomyDrag>,
    sky_drag: Option<AstronomySkyDrag>,
    panorama: PanoramaView,
    requested_panorama: Option<PanoramaView>,
    sky_offset: CelestialRotation,
    sky_release: Option<AstronomyRelease>,
    presented_sky: Option<PanoramaView>,
    interaction_pending: bool,
    last_interaction_frame: Option<Instant>,
    interaction_revision: u64,
    output_identities: Vec<(String, i32, i32, u32, u32)>,
    identities_changed: bool,
    body_overrides: std::collections::BTreeMap<String, String>,
    bodies: Vec<CelestialBody>,
    retired: Vec<glow::Texture>,
    program: Option<AstronomyProgram>,
    failed: bool,
    powered: bool,
    last_power_query: Option<Instant>,
    clock: WallpaperClock,
    sample_unix: f64,
    sample_offset: i32,
    frame_changed: bool,
    layout: String,
    draw_plan: Vec<AstronomyDraw>,
    layout_screen: (u32, u32),
    layout_outputs: Vec<(i32, i32, u32, u32)>,
}

impl AstronomyWallpaper {
    fn config_matches(&self, behavior: &crate::config::BehaviorConfig) -> bool {
        let (earth, moon) = configured_views(behavior);
        self.enabled
            && behavior.wallpaper_effect == "astronomy"
            && self.layout == behavior.wallpaper_astronomy_layout
            && self.configured_path == behavior.wallpaper_earth_texture
            && self.moon_configured_path == behavior.wallpaper_moon_texture
            && self.body_overrides == behavior.wallpaper_astronomy_bodies
            && self.earth_view == earth
            && self.moon_view == moon
            && self.earth_period == behavior.wallpaper_earth_rotation_minutes
            && self.moon_period == behavior.wallpaper_moon_rotation_minutes
            && self.sky_period == behavior.wallpaper_sky_rotation_minutes
    }

    fn has_drag(&self) -> bool {
        self.drag.is_some() || self.sky_drag.is_some()
    }

    fn sky_view(&self, unix: f64) -> PanoramaView {
        let orientation = if let Some(drag) = &self.sky_drag {
            drag.orientation
        } else if let Some(release) = &self.sky_release {
            CelestialRotation::from_matrix(&release.presented)
        } else {
            panorama_rotation(unix, self.sky_period).compose(self.sky_offset)
        };
        let mut view = self.panorama;
        view.set_orientation(orientation);
        view
    }

    fn reset_sky(&mut self) {
        self.sky_drag = None;
        self.sky_offset = CelestialRotation::default();
        self.sky_release = None;
        self.presented_sky = None;
        self.panorama = PanoramaView::default();
        self.requested_panorama = None;
        self.interaction_revision = self.interaction_revision.wrapping_add(1);
    }

    fn reset_orientation(&mut self, body: CelestialBody) {
        match body {
            CelestialBody::Earth => {
                self.earth_offset = None;
                self.earth_release = None;
                self.presented_earth = None;
                self.earth_zoom = AstronomyZoom::default();
            }
            CelestialBody::Moon => {
                self.moon_offset = None;
                self.moon_release = None;
                self.presented_moon = None;
                self.moon_zoom = AstronomyZoom::default();
            }
        }
        if self.drag.as_ref().is_some_and(|drag| drag.body == body) {
            self.drag = None;
        }
        self.interaction_revision = self.interaction_revision.wrapping_add(1);
    }

    fn base_rotation(&self, body: CelestialBody, unix: f64) -> CelestialRotation {
        let basis = match body {
            CelestialBody::Earth => self.earth_view.matrix(unix, self.earth_period),
            CelestialBody::Moon => self.moon_view.matrix(unix, self.moon_period),
        };
        CelestialRotation::from_matrix(&basis)
    }

    fn body_matrix(&self, body: CelestialBody, unix: f64) -> [f32; 9] {
        if let Some(drag) = self.drag.as_ref().filter(|drag| drag.body == body) {
            return drag.orientation.matrix();
        }
        let (view, period, offset, release) = match body {
            CelestialBody::Earth => (
                self.earth_view,
                self.earth_period,
                self.earth_offset,
                self.earth_release.as_ref(),
            ),
            CelestialBody::Moon => (
                self.moon_view,
                self.moon_period,
                self.moon_offset,
                self.moon_release.as_ref(),
            ),
        };
        if let Some(release) = release {
            return release.presented;
        }
        let basis = view.matrix(unix, period);
        offset.map_or(basis, |offset| {
            CelestialRotation::from_matrix(&basis)
                .compose(offset)
                .matrix()
        })
    }

    fn hit_test(&self, root: [f64; 2]) -> Option<(CelestialBody, AstronomyDraw, [f64; 2])> {
        // The complete topmost canvas (including its sky) hides earlier canvases.
        // Do not select a lower globe through the background of an overlapping output.
        for (index, base_canvas) in self.draw_plan.iter().enumerate().rev() {
            let body = self.canvas_body(index);
            let mut canvas = base_canvas.clone();
            canvas.radius *= self.body_zoom(body).presented_scale();
            let (pixel, point) = canvas_pointer(&canvas, root, self.layout_screen.1)?;
            let (x, y, w, h) = canvas.clip;
            if pixel[0] < f64::from(x)
                || pixel[1] < f64::from(y)
                || pixel[0] >= f64::from(x) + f64::from(w)
                || pixel[1] >= f64::from(y) + f64::from(h)
            {
                continue;
            }
            if point[0].hypot(point[1]) > 1.0 {
                return None;
            }
            return Some((body, canvas, point));
        }
        None
    }

    fn canvas_body(&self, index: usize) -> CelestialBody {
        if self.dual_material && self.bodies.get(index) == Some(&CelestialBody::Moon) {
            CelestialBody::Moon
        } else {
            CelestialBody::Earth
        }
    }

    fn body_zoom(&self, body: CelestialBody) -> &AstronomyZoom {
        match body {
            CelestialBody::Earth => &self.earth_zoom,
            CelestialBody::Moon => &self.moon_zoom,
        }
    }

    fn zoom(&mut self, root: [f64; 2], steps: f64) -> bool {
        if self.has_drag() || !self.enabled || self.failed || !self.powered {
            return false;
        }
        let Some((body, _, _)) = self.hit_test(root) else {
            if self.sky_hit_test(root).is_none() || self.presented_sky.is_none() {
                return false;
            }
            let mut view = self.requested_panorama.unwrap_or(self.panorama);
            if !view.zoom(steps) {
                return false;
            }
            self.requested_panorama = Some(view);
            self.interaction_pending = true;
            return true;
        };
        let (zoom, presented) = match body {
            CelestialBody::Earth => (&mut self.earth_zoom, self.presented_earth),
            CelestialBody::Moon => (&mut self.moon_zoom, self.presented_moon),
        };
        if presented.is_none() || !zoom.request(steps) {
            return false;
        }
        self.interaction_pending = true;
        true
    }

    fn begin_drag(&mut self, root: [f64; 2]) -> bool {
        if self.has_drag() || !self.enabled || self.failed || !self.powered {
            return false;
        }
        let Some((body, canvas, point)) = self.hit_test(root) else {
            return self.begin_sky_drag(root);
        };
        let Some(presented) = (match body {
            CelestialBody::Earth => self.presented_earth,
            CelestialBody::Moon => self.presented_moon,
        }) else {
            // Texture loading and static fallback never capture desktop clicks.
            return false;
        };
        let orientation = CelestialRotation::from_matrix(&presented);
        let Some(arcball) = CelestialArcball::new(point, orientation) else {
            return false;
        };
        let zoom = match body {
            CelestialBody::Earth => &mut self.earth_zoom,
            CelestialBody::Moon => &mut self.moon_zoom,
        };
        let zoom_before_begin = *zoom;
        // Grabbing the visible globe supersedes a not-yet-presented wheel step.
        // Freeze both its visible orientation and its visible size together.
        // abort_drag restores that step if native pointer acquisition fails.
        zoom.admitted_log = f64::from(zoom.presented_scale()).ln();
        zoom.requested_log = zoom.admitted_log;
        self.drag = Some(AstronomyDrag {
            body,
            canvas,
            arcball,
            orientation,
            pending_point: point,
            pending_before_begin: self.interaction_pending,
            zoom_before_begin,
        });
        self.interaction_pending = true;
        true
    }

    /// Pick only the background of the topmost covering canvas. A visible
    /// globe always wins; empty pixels cannot expose a lower monitor's scene.
    fn sky_hit_test(&self, root: [f64; 2]) -> Option<AstronomyDraw> {
        for (index, base_canvas) in self.draw_plan.iter().enumerate().rev() {
            let mut canvas = base_canvas.clone();
            canvas.radius *= self.body_zoom(self.canvas_body(index)).presented_scale();
            let (pixel, point) = canvas_pointer(&canvas, root, self.layout_screen.1)?;
            let (x, y, w, h) = canvas.clip;
            if pixel[0] < f64::from(x)
                || pixel[1] < f64::from(y)
                || pixel[0] >= f64::from(x) + f64::from(w)
                || pixel[1] >= f64::from(y) + f64::from(h)
            {
                continue;
            }
            return (point[0].hypot(point[1]) > 1.0).then_some(canvas);
        }
        None
    }

    fn begin_sky_drag(&mut self, root: [f64; 2]) -> bool {
        let Some(canvas) = self.sky_hit_test(root) else {
            return false;
        };
        let Some(presented) = self.presented_sky else {
            return false;
        };
        let Some(point) = panorama_pointer(&canvas, root, self.layout_screen.1) else {
            return false;
        };
        let Some(gesture) = presented.begin_drag(point, canvas.size.map(f64::from)) else {
            return false;
        };
        self.sky_drag = Some(AstronomySkyDrag {
            canvas,
            gesture,
            orientation: CelestialRotation::from_matrix(&presented.matrix()),
            pending_point: point,
            pending_before_begin: self.interaction_pending,
            view_before_begin: self.panorama,
            requested_before_begin: self.requested_panorama,
        });
        self.panorama = presented;
        self.requested_panorama = None;
        self.interaction_pending = true;
        true
    }

    /// Undo an immediately refused pointer-grab acquisition. No frame can have
    /// consumed this just-created gesture; restore any earlier queued release.
    fn abort_drag(&mut self) {
        if let Some(drag) = self.drag.take() {
            self.interaction_pending = drag.pending_before_begin;
            match drag.body {
                CelestialBody::Earth => self.earth_zoom = drag.zoom_before_begin,
                CelestialBody::Moon => self.moon_zoom = drag.zoom_before_begin,
            }
        }
        if let Some(drag) = self.sky_drag.take() {
            self.interaction_pending = drag.pending_before_begin;
            self.panorama = drag.view_before_begin;
            self.requested_panorama = drag.requested_before_begin;
        }
    }

    fn drag_motion(&mut self, root: [f64; 2]) -> bool {
        if let Some(drag) = &mut self.sky_drag {
            let Some(point) = panorama_pointer(&drag.canvas, root, self.layout_screen.1) else {
                return false;
            };
            if point != drag.pending_point {
                drag.pending_point = point;
                self.interaction_pending = true;
            }
            return true;
        }
        let Some(drag) = self.drag.as_mut() else {
            return false;
        };
        let Some((_, point)) = canvas_pointer(&drag.canvas, root, self.layout_screen.1) else {
            return false;
        };
        if point != drag.pending_point {
            drag.pending_point = point;
            self.interaction_pending = true;
        }
        true
    }

    /// Resume decorative motion from the released pose while leaving the real
    /// Sun/phase clock untouched, even after a long hold or wall-clock correction.
    fn end_drag(&mut self, unix: f64) -> bool {
        if let Some(drag) = self.sky_drag.take() {
            let orientation = drag
                .gesture
                .orientation_at(drag.pending_point)
                .unwrap_or(drag.orientation);
            self.sky_release = Some(AstronomyRelease {
                presented: self
                    .presented_sky
                    .map_or(drag.orientation.matrix(), |view| view.matrix()),
                offset: panorama_rotation(unix, self.sky_period)
                    .inverse()
                    .compose(orientation),
            });
            self.interaction_pending = true;
            return true;
        }
        let Some(drag) = self.drag.take() else {
            return false;
        };
        let orientation = drag
            .arcball
            .orientation_at(drag.pending_point)
            .unwrap_or(drag.orientation);
        let offset = self
            .base_rotation(drag.body, unix)
            .inverse()
            .compose(orientation);
        let presented = match drag.body {
            CelestialBody::Earth => self.presented_earth,
            CelestialBody::Moon => self.presented_moon,
        }
        .unwrap_or_else(|| drag.orientation.matrix());
        let release = AstronomyRelease { presented, offset };
        match drag.body {
            CelestialBody::Earth => self.earth_release = Some(release),
            CelestialBody::Moon => self.moon_release = Some(release),
        }
        self.interaction_pending = true;
        true
    }

    fn commit_releases(&mut self) {
        let mut changed = false;
        if let Some(release) = self.earth_release.take() {
            self.earth_offset = Some(release.offset);
            changed = true;
        }
        if let Some(release) = self.moon_release.take() {
            self.moon_offset = Some(release.offset);
            changed = true;
        }
        if let Some(release) = self.sky_release.take() {
            self.sky_offset = release.offset;
            changed = true;
        }
        if changed {
            self.interaction_revision = self.interaction_revision.wrapping_add(1);
        }
    }

    fn interaction_deadline(&self, now: Instant) -> Option<Duration> {
        (self.interaction_pending && self.powered && !self.draw_plan.is_empty()).then(|| {
            self.last_interaction_frame.map_or(Duration::ZERO, |last| {
                DRAG_FRAME_INTERVAL.saturating_sub(now.saturating_duration_since(last))
            })
        })
    }

    fn consume_interaction(&mut self, now: Instant) -> bool {
        if self.interaction_deadline(now) != Some(Duration::ZERO) {
            return false;
        }
        self.commit_releases();
        self.earth_zoom.admit();
        self.moon_zoom.admit();
        if let Some(view) = self.requested_panorama.take() {
            self.panorama = view;
        }
        if let Some(drag) = self.drag.as_mut()
            && let Some(orientation) = drag.arcball.orientation_at(drag.pending_point)
        {
            drag.orientation = orientation;
        }
        if let Some(drag) = self.sky_drag.as_mut()
            && let Some(orientation) = drag.gesture.orientation_at(drag.pending_point)
        {
            drag.orientation = orientation;
        }
        self.interaction_pending = false;
        self.last_interaction_frame = Some(now);
        self.interaction_revision = self.interaction_revision.wrapping_add(1);
        true
    }

    fn clear_interaction_wake(&mut self) {
        // Pausing/teardown must not strand the release in a permanently frozen
        // state. Rendering resumes with a fresh time sample and this offset.
        self.commit_releases();
        self.earth_zoom.admit();
        self.moon_zoom.admit();
        if let Some(view) = self.requested_panorama.take() {
            self.panorama = view;
        }
        self.interaction_pending = false;
        self.last_interaction_frame = None;
    }

    /// Camera-only updates do not reload surfaces or reset animation clocks.
    fn update_views(&mut self, earth: CelestialView, moon: CelestialView) -> bool {
        let changed = self.earth_view != earth || self.moon_view != moon;
        if self.earth_view != earth {
            self.reset_orientation(CelestialBody::Earth);
        }
        if self.moon_view != moon {
            self.reset_orientation(CelestialBody::Moon);
        }
        self.earth_view = earth;
        self.moon_view = moon;
        changed
    }

    /// Only visible, rotating bodies contribute to the animation cadence.
    /// A hidden fast Moon must not keep an Earth-only desktop at 15 Hz.
    fn frame_interval(&self) -> Duration {
        let mut earth_visible = false;
        let mut moon_visible = false;
        let mut radius = 0.0_f32;
        for (index, canvas) in self.draw_plan.iter().enumerate() {
            radius = radius.max(canvas.radius * self.body_zoom(self.canvas_body(index)).scale());
            if self.dual_material && self.bodies.get(index) == Some(&CelestialBody::Moon) {
                moon_visible = true;
            } else {
                earth_visible = true;
            }
        }
        astronomy_frame_interval(
            radius,
            (earth_visible
                && self.earth_view.rotating
                && !self
                    .drag
                    .as_ref()
                    .is_some_and(|drag| drag.body == CelestialBody::Earth))
            .then_some(self.earth_period),
            (moon_visible
                && self.moon_view.rotating
                && !self
                    .drag
                    .as_ref()
                    .is_some_and(|drag| drag.body == CelestialBody::Moon))
            .then_some(self.moon_period),
            if self.sky_drag.is_some() {
                0
            } else {
                self.sky_period
            },
        )
    }

    /// Sample one real instant for both bodies on every material repaint.
    /// Timezone changes invalidate the frame, never offset the UTC light clock.
    fn update_clock(
        &mut self,
        now: Instant,
        unix: f64,
        offset_seconds: i32,
        occluded: bool,
        invalidated: bool,
    ) -> bool {
        let timezone_changed = self.sample_offset != offset_seconds;
        self.sample_offset = offset_seconds;
        if !self.enabled || !self.powered || occluded || self.draw_plan.is_empty() {
            self.end_drag(unix);
            self.clear_interaction_wake();
        }
        let interaction = self.consume_interaction(now);
        self.clock.set_interval(self.frame_interval());
        self.clock
            .set_paused(!self.enabled || !self.powered || occluded || self.draw_plan.is_empty());
        let tick = self.clock.tick(now);
        self.frame_changed = invalidated || timezone_changed || tick || interaction;
        if self.frame_changed {
            self.sample_unix = unix;
            self.cache.invalidate();
        }
        self.frame_changed
    }

    /// Material identity for cached wallpaper-only bar measurements. Animation
    /// time is deliberately excluded; samples piggyback on animation frames.
    pub(super) fn bar_metric_source(&self) -> (u64, bool) {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        (
            self.enabled,
            self.failed,
            self.powered,
            self.texture,
            self.moon_texture,
            self.cloud_texture,
            self.sky_texture,
            &self.layout,
            &self.layout_outputs,
            &self.body_overrides,
            &self.output_identities,
        )
            .hash(&mut hash);
        (self.earth_detail.texture, self.moon_detail.texture).hash(&mut hash);
        for view in [self.earth_view, self.moon_view] {
            view.rotating.hash(&mut hash);
            for degrees in [view.latitude, view.longitude, view.roll] {
                degrees.to_bits().hash(&mut hash);
            }
            for component in view.axis {
                component.to_bits().hash(&mut hash);
            }
        }
        (self.earth_period, self.moon_period).hash(&mut hash);
        self.interaction_revision.hash(&mut hash);
        (hash.finish(), self.enabled && self.powered && !self.failed)
    }

    /// Rebuild only on material layout/topology changes, never on ordinary damage.
    fn update_layout(
        &mut self,
        enabled: bool,
        layout: &str,
        screen: (u32, u32),
        outputs: impl Iterator<Item = (i32, i32, u32, u32)> + Clone,
    ) -> bool {
        let a = self;
        let per_output = matches!(layout, "per-output" | "earth-moon");
        let output_count = if enabled && per_output {
            outputs.clone().take(129).count()
        } else {
            0
        };
        let topology_changed = a.layout_outputs.len() != output_count
            || (enabled
                && per_output
                && a.layout_outputs
                    .iter()
                    .zip(outputs.clone())
                    .any(|(old, m)| *old != m));
        let mut layout_changed = a.layout != layout || a.layout_screen != screen;
        if layout_changed || topology_changed || a.enabled != enabled {
            a.layout_outputs.clear();
            if enabled && per_output {
                a.layout_outputs.extend(outputs.take(129));
            }
            let plan = if enabled {
                astronomy_draw_plan(screen, &a.layout_outputs, per_output, None)
            } else {
                Vec::new()
            };
            layout_changed |= a.draw_plan != plan;
            a.draw_plan = plan;
            a.layout.clear();
            a.layout.push_str(layout);
            a.layout_screen = screen;
        }
        layout_changed
    }
    fn update_bodies(
        &mut self,
        overrides: &std::collections::BTreeMap<String, String>,
        layout_changed: bool,
    ) -> bool {
        let a = self;
        let bodies_changed =
            a.identities_changed || a.body_overrides != *overrides || layout_changed;
        if bodies_changed {
            let names: Vec<_> = a
                .draw_plan
                .iter()
                .map(|canvas| {
                    a.output_identities
                        .iter()
                        .find(|(_, x, y, w, h)| {
                            canvas.origin
                                == [*x as f32, a.layout_screen.1 as f32 - *y as f32 - *h as f32]
                                && canvas.size == [*w as f32, *h as f32]
                        })
                        .map(|o| o.0.clone())
                        .unwrap_or_else(|| {
                            format!("geometry:{:?}:{:?}", canvas.origin, canvas.size)
                        })
                })
                .collect();
            a.bodies = celestial_assignments(&names, overrides);
            a.body_overrides = overrides.clone();
            a.identities_changed = false;
        }
        bodies_changed
    }
    fn observe_power(&mut self, now: Instant, reply: Option<bool>) -> bool {
        let powered = reply == Some(true);
        let changed = self.powered != powered;
        self.powered = powered;
        self.last_power_query = Some(now);
        changed
    }

    /// Request each optional high-resolution image at most once per material.
    /// The shared worker bounds concurrent decoders and never performs I/O on
    /// pointer events. Do not upsample a small custom map or a low-capacity GPU.
    fn request_details(&mut self) {
        if !self.enabled || self.failed || !self.powered || self.draw_plan.is_empty() {
            return;
        }
        let edge = self.max_texture_edge.min(8192) & !1;
        if edge == 0 {
            return;
        }
        let earth_visible = !self.dual_material || self.bodies.contains(&CelestialBody::Earth);
        let moon_visible = self.dual_material && self.bodies.contains(&CelestialBody::Moon);
        if earth_visible && self.earth_zoom.scale() > 1.3 && !self.earth_detail.attempted {
            self.earth_detail.attempted = true;
            if edge > self.earth_width && !self.path.is_empty() {
                self.earth_detail.pending = Some(
                    crate::backend::compositor_common::wallpaper_decode::load_wallpaper(
                        &self.path,
                        edge,
                        edge / 2,
                        WallpaperMode::Stretch,
                    ),
                );
            }
        }
        if moon_visible && self.moon_zoom.scale() > 1.3 && !self.moon_detail.attempted {
            self.moon_detail.attempted = true;
            if edge > self.moon_width
                && let Some(path) = resolve_moon_detail_texture(&self.moon_configured_path)
            {
                self.moon_detail.pending = Some(
                    crate::backend::compositor_common::wallpaper_decode::load_wallpaper(
                        &path,
                        edge,
                        edge / 2,
                        WallpaperMode::Stretch,
                    ),
                );
            }
        }
    }
    pub(super) fn deadline(&self, now: Instant) -> Option<Duration> {
        if !self.enabled || self.failed {
            return None;
        }
        let query = self.last_power_query.map_or(Duration::ZERO, |last| {
            POWER_QUERY_INTERVAL.saturating_sub(now.saturating_duration_since(last))
        });
        Some(
            [
                Some(query),
                self.clock.deadline(now),
                self.interaction_deadline(now),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(query),
        )
    }
    pub(super) fn due(&self) -> bool {
        self.deadline(Instant::now()) == Some(Duration::ZERO)
    }
    fn take_failed_material(&mut self) -> [Option<glow::Texture>; 6] {
        self.end_drag(self.sample_unix);
        self.clear_interaction_wake();
        self.presented_earth = None;
        self.presented_moon = None;
        self.presented_sky = None;
        self.pending = None;
        self.moon_pending = None;
        self.cloud_pending = None;
        self.sky_pending = None;
        self.earth_detail.pending = None;
        self.moon_detail.pending = None;
        self.failed = true;
        [
            self.texture.take(),
            self.moon_texture.take(),
            self.cloud_texture.take(),
            self.sky_texture.take(),
            self.earth_detail.texture.take(),
            self.moon_detail.texture.take(),
        ]
    }

    /// A terminal required-image failure must not strand GPU allocations once wakes stop.
    unsafe fn fail_material(&mut self, gl: &glow::Context) {
        unsafe {
            self.cache.cleanup(gl);
        }
        for texture in self.take_failed_material().into_iter().flatten() {
            unsafe {
                gl.delete_texture(texture);
            }
        }
    }

    pub(super) unsafe fn cleanup(&mut self, gl: &glow::Context) {
        self.drag = None;
        self.sky_drag = None;
        self.clear_interaction_wake();
        self.presented_earth = None;
        self.presented_moon = None;
        self.presented_sky = None;
        unsafe {
            self.cache.cleanup(gl);
        }
        self.pending = None;
        self.moon_pending = None;
        self.cloud_pending = None;
        self.sky_pending = None;
        self.earth_detail.pending = None;
        self.moon_detail.pending = None;
        unsafe {
            for tex in self
                .retired
                .drain(..)
                .chain(self.texture.take())
                .chain(self.moon_texture.take())
                .chain(self.cloud_texture.take())
                .chain(self.sky_texture.take())
                .chain(self.earth_detail.texture.take())
                .chain(self.moon_detail.texture.take())
            {
                gl.delete_texture(tex);
            }
            if let Some(program) = self.program.take() {
                gl.delete_program(program.program);
            }
        }
    }
}

impl<C: CompositorConnection> Compositor<C> {
    fn astronomy_interaction_available(&self) -> bool {
        let a = &self.astronomy_wallpaper;
        a.enabled
            && !a.failed
            && a.powered
            && !a.identities_changed
            && a.layout_screen == (self.screen_w, self.screen_h)
            && a.config_matches(crate::config::CONFIG.load().behavior())
            && (!matches!(a.layout.as_str(), "per-output" | "earth-moon")
                || self
                    .monitor_wallpapers
                    .iter()
                    .take(129)
                    .map(|m| (m.mon_x, m.mon_y, m.mon_w, m.mon_h))
                    .eq(a.layout_outputs.iter().copied()))
            && !self.overview_active
            && !self.overview_closing
            && !self.expose_active
            && self.expose_entries.is_empty()
            && self.genie_active.is_empty()
            && !self.transition_active()
    }

    /// The WM admits unmodified primary-button desktop presses and owns the
    /// pointer grab. The renderer selects the body actually visible at that point.
    pub(crate) fn astronomy_begin_drag(&mut self, root_x: f64, root_y: f64) -> bool {
        self.astronomy_interaction_available()
            && self.astronomy_wallpaper.begin_drag([root_x, root_y])
    }

    pub(crate) fn astronomy_drag_active(&self) -> bool {
        self.astronomy_wallpaper.has_drag() && self.astronomy_interaction_available()
    }

    pub(crate) fn astronomy_drag_motion(&mut self, root_x: f64, root_y: f64) -> bool {
        if !self.astronomy_drag_active() || !self.astronomy_wallpaper.drag_motion([root_x, root_y])
        {
            self.astronomy_end_drag();
            return false;
        }
        // deadline()/due() already join the compositor's wakeup and unchanged
        // gates. Do not set needs_render for every high-frequency MotionNotify.
        true
    }

    pub(crate) fn astronomy_end_drag(&mut self) {
        self.astronomy_wallpaper
            .end_drag(wall_clock_seconds(SystemTime::now()));
    }

    pub(crate) fn astronomy_abort_drag(&mut self) {
        self.astronomy_wallpaper.abort_drag();
    }

    /// Positive wheel detents approach the globe under the pointer. Texture
    /// loading, ordinary desktop bindings and active grabs keep their priority.
    pub(crate) fn astronomy_zoom(&mut self, root_x: f64, root_y: f64, steps: f64) -> bool {
        self.astronomy_interaction_available()
            && self.astronomy_wallpaper.zoom([root_x, root_y], steps)
    }

    fn poll_astronomy_detail(
        gl: &glow::Context,
        detail: &mut AstronomyDetail,
        base_width: u32,
    ) -> bool {
        let Some(load) = detail.pending.as_ref() else {
            return false;
        };
        match load.try_recv() {
            Ok(data) => {
                detail.pending = None;
                if data.width != data.height.saturating_mul(2) || data.width <= base_width {
                    return false;
                }
                let Some((texture, _, _)) = Self::upload_wallpaper_texture(gl, &data) else {
                    log::warn!("astronomy wallpaper: detail upload unavailable; keeping base map");
                    return false;
                };
                if !unsafe {
                    procedural_wallpaper_cache::prepare_equirectangular_texture(gl, texture)
                } {
                    // A valid base-level image remains usable without mipmaps.
                    log::warn!("astronomy wallpaper: detail mipmaps unavailable");
                }
                detail.texture = Some(texture);
                true
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                detail.pending = None;
                log::warn!("astronomy wallpaper: detail image unavailable; keeping base map");
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
        }
    }

    pub(super) fn load_astronomy_sky_async(
        path: String,
        max_edge: u32,
    ) -> wallpaper::DeferredWallpaperLoad {
        wallpaper::DeferredWallpaperLoad::new(move |cancelled| {
            decode_astronomy_sky(&path, max_edge, cancelled)
        })
    }

    pub(crate) fn set_astronomy_output_identities(
        &mut self,
        outputs: &[crate::backend::api::OutputInfo],
    ) {
        let mut identities: Vec<_> = outputs
            .iter()
            .take(128)
            .filter(|o| o.width > 0 && o.height > 0)
            .map(|o| (o.name.clone(), o.x, o.y, o.width as u32, o.height as u32))
            .collect();
        // Mirrors with different connector names choose the first name consistently.
        identities.sort_unstable();
        let a = &mut self.astronomy_wallpaper;
        if a.output_identities != identities {
            a.output_identities = identities;
            a.identities_changed = true;
            self.needs_render = true;
        }
    }

    /// CPU/X11-only maintenance, deliberately before fullscreen/unchanged gates.
    pub(super) fn prepare_astronomy_wallpaper(&mut self, occluded: bool) -> bool {
        let now = Instant::now();
        let cfg = crate::config::CONFIG.load();
        let enabled = cfg.behavior().wallpaper_effect == "astronomy";
        let configured = &cfg.behavior().wallpaper_earth_texture;
        let layout = &cfg.behavior().wallpaper_astronomy_layout;
        let sample_unix = if enabled {
            wall_clock_seconds(SystemTime::now())
        } else {
            0.0
        };
        let use_system_zone = enabled && cfg.behavior().wallpaper_astronomy_timezone == "system";
        let system_offset = if use_system_zone {
            chrono::TimeZone::timestamp_opt(&chrono::Local, sample_unix.floor() as i64, 0)
                .single()
                .map(|time| time.offset().local_minus_utc())
        } else {
            None
        };
        let offset_seconds = presentation_offset(
            use_system_zone,
            cfg.behavior().wallpaper_timezone_offset_hours,
            system_offset,
        );
        let a = &mut self.astronomy_wallpaper;
        if a.has_drag() && (!a.config_matches(cfg.behavior()) || occluded) {
            a.end_drag(wall_clock_seconds(SystemTime::now()));
        }
        let layout_changed = a.update_layout(
            enabled,
            layout,
            (self.screen_w, self.screen_h),
            self.monitor_wallpapers
                .iter()
                .map(|m| (m.mon_x, m.mon_y, m.mon_w, m.mon_h)),
        );
        let dual = layout == "earth-moon";
        let moon_configured = &cfg.behavior().wallpaper_moon_texture;
        let overrides = &cfg.behavior().wallpaper_astronomy_bodies;
        let bodies_changed = a.update_bodies(overrides, layout_changed);
        if layout_changed || bodies_changed {
            a.end_drag(sample_unix);
            a.presented_earth = None;
            a.presented_moon = None;
            a.presented_sky = None;
        }
        let (earth_view, moon_view) = configured_views(cfg.behavior());
        let view_changed = a.update_views(earth_view, moon_view);
        let sky_changed = a.sky_period != cfg.behavior().wallpaper_sky_rotation_minutes;
        if sky_changed {
            a.reset_sky();
        }
        a.sky_period = cfg.behavior().wallpaper_sky_rotation_minutes;
        let earth_period_changed =
            a.earth_period != cfg.behavior().wallpaper_earth_rotation_minutes;
        let moon_period_changed = a.moon_period != cfg.behavior().wallpaper_moon_rotation_minutes;
        let periods_changed = sky_changed || earth_period_changed || moon_period_changed;
        if earth_period_changed {
            a.reset_orientation(CelestialBody::Earth);
        }
        if moon_period_changed {
            a.reset_orientation(CelestialBody::Moon);
        }
        a.earth_period = cfg.behavior().wallpaper_earth_rotation_minutes;
        a.moon_period = cfg.behavior().wallpaper_moon_rotation_minutes;
        let changed = a.enabled != enabled
            || a.configured_path != *configured
            || a.moon_configured_path != *moon_configured
            || a.dual_material != dual;
        if changed {
            a.end_drag(sample_unix);
            a.presented_earth = None;
            a.presented_moon = None;
            a.presented_sky = None;
            a.cache.invalidate();
            a.enabled = enabled;
            a.configured_path = configured.clone();
            a.moon_configured_path = moon_configured.clone();
            a.dual_material = dual;
            a.moon_pending = None;
            a.retired.extend(a.moon_texture.take());
            a.cloud_pending = None;
            a.retired.extend(a.cloud_texture.take());
            a.sky_pending = None;
            a.retired.extend(a.sky_texture.take());
            a.sky_path.clear();
            a.retired.extend(a.earth_detail.texture.take());
            a.retired.extend(a.moon_detail.texture.take());
            a.earth_detail = AstronomyDetail::default();
            a.moon_detail = AstronomyDetail::default();
            a.earth_width = 0;
            a.moon_width = 0;
            a.path = if enabled {
                resolve_earth_texture(configured).unwrap_or_default()
            } else {
                String::new()
            };
            a.failed = false;
            a.pending = None;
            a.retired.extend(a.texture.take());
            a.last_power_query = None;
            a.clock = WallpaperClock::default();
            if enabled && !a.path.is_empty() {
                a.pending = Some(Self::load_wallpaper_async(
                    &a.path,
                    4096,
                    2048,
                    WallpaperMode::Stretch,
                ));
            } else if enabled {
                a.failed = true;
                log::warn!(
                    "astronomy wallpaper: Earth texture unavailable; keeping static wallpaper"
                );
            }
            // The optional historic cloud composite is aligned with the bundled Earth map.
            // Do not paint it over an explicitly configured user texture.
            if enabled && configured.is_empty() {
                if let Some(path) = resolve_cloud_texture() {
                    a.cloud_pending = Some(Self::load_wallpaper_async(
                        &path,
                        2048,
                        1024,
                        WallpaperMode::Stretch,
                    ));
                }
            }
            // Resolve/decode once per material change; never perform file I/O per frame.
            if enabled {
                if let Some(path) = resolve_sky_texture() {
                    a.sky_path = path.clone();
                    a.sky_pending = Some(Self::load_astronomy_sky_async(path, a.max_texture_edge));
                }
            }
            if enabled && dual {
                if let Some(path) = resolve_moon_texture(moon_configured) {
                    a.moon_pending = Some(Self::load_wallpaper_async(
                        &path,
                        2048,
                        1024,
                        WallpaperMode::Stretch,
                    ));
                } else {
                    a.failed = true;
                    log::warn!(
                        "astronomy wallpaper: Moon texture unavailable; keeping static wallpaper"
                    );
                }
            }
        }
        if !a.enabled || a.failed {
            a.clear_interaction_wake();
            return changed || layout_changed || bodies_changed || periods_changed || view_changed;
        }
        let power_changed = if a
            .last_power_query
            .is_none_or(|last| now.saturating_duration_since(last) >= POWER_QUERY_INTERVAL)
        {
            a.observe_power(now, self.graphics.wallpaper_power_allows_animation())
        } else {
            false
        };

        let invalidated = changed
            || layout_changed
            || bodies_changed
            || periods_changed
            || view_changed
            || power_changed;
        let changed = a.update_clock(now, sample_unix, offset_seconds, occluded, invalidated);
        if !occluded {
            a.request_details();
        }
        changed
    }

    /// Called with a current context, after the normal static wallpaper pass.
    pub(super) fn draw_astronomy_wallpaper(&mut self, repair: Option<(i32, i32, i32, i32)>) {
        let cache_ten_bit = procedural_wallpaper_cache::uses_ten_bit(
            self.hdr_enabled,
            self.hdr_output_10bit,
            self.needs_postprocess() && self.postprocess_fbo.is_some(),
        );
        let a = &mut self.astronomy_wallpaper;
        unsafe {
            for tex in a.retired.drain(..) {
                self.gl.delete_texture(tex);
            }
        }
        if a.failed {
            unsafe {
                a.fail_material(&self.gl);
            }
            return;
        }
        if !a.enabled {
            unsafe {
                a.cache.cleanup(&self.gl);
            }
            return;
        }
        if !a.powered {
            return;
        }
        // First adoption changes the entire material, so wait for a frame
        // whose prepare step already requested full repair/blur invalidation.
        // Unrelated client damage between animation ticks must stay static.
        let (may_draw, poll_optional) = material_load_frame(
            a.pending.is_some() || a.moon_pending.is_some(),
            a.cloud_pending.is_some()
                || a.sky_pending.is_some()
                || a.earth_detail.pending.is_some()
                || a.moon_detail.pending.is_some(),
            a.frame_changed,
        );
        if !may_draw {
            return;
        }
        // Upload helpers bind the active unit; compositor convention is unit zero.
        unsafe {
            self.gl.active_texture(glow::TEXTURE0);
            if a.max_texture_edge == 0 {
                a.max_texture_edge =
                    self.gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(0) as u32;
            }
        }
        if let Some(load) = a.pending.as_ref() {
            match load.try_recv() {
                Ok(data) => {
                    a.cache.invalidate();
                    a.pending = None;
                    if data.width != data.height.saturating_mul(2) {
                        log::warn!(
                            "astronomy wallpaper: Earth texture must be equirectangular 2:1; keeping static wallpaper"
                        );
                        unsafe {
                            a.fail_material(&self.gl);
                        }
                        return;
                    }
                    a.texture = Self::upload_wallpaper_texture(&self.gl, &data).map(|v| v.0);
                    if a.texture.is_none() {
                        unsafe {
                            a.fail_material(&self.gl);
                        }
                        return;
                    }
                    a.earth_width = data.width;
                    if let Some(texture) = a.texture
                        && !unsafe {
                            procedural_wallpaper_cache::prepare_equirectangular_texture(
                                &self.gl, texture,
                            )
                        }
                    {
                        log::warn!(
                            "astronomy wallpaper: mipmaps unavailable; retaining base texture"
                        );
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    a.pending = None;
                    unsafe {
                        a.fail_material(&self.gl);
                    }
                    log::warn!("astronomy wallpaper: Earth image failed; keeping static wallpaper");
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
            }
        }
        if let Some(load) = a.moon_pending.as_ref() {
            match load.try_recv() {
                Ok(data) => {
                    a.cache.invalidate();
                    a.moon_pending = None;
                    if data.width != data.height.saturating_mul(2) {
                        log::warn!(
                            "astronomy wallpaper: Moon texture must be equirectangular 2:1; keeping static wallpaper"
                        );
                        unsafe {
                            a.fail_material(&self.gl);
                        }
                        return;
                    }
                    a.moon_texture = Self::upload_wallpaper_texture(&self.gl, &data).map(|v| v.0);
                    if a.moon_texture.is_none() {
                        unsafe {
                            a.fail_material(&self.gl);
                        }
                        return;
                    }
                    a.moon_width = data.width;
                    if let Some(texture) = a.moon_texture
                        && !unsafe {
                            procedural_wallpaper_cache::prepare_equirectangular_texture(
                                &self.gl, texture,
                            )
                        }
                    {
                        log::warn!(
                            "astronomy wallpaper: mipmaps unavailable; retaining base texture"
                        );
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    a.moon_pending = None;
                    unsafe {
                        a.fail_material(&self.gl);
                    }
                    log::warn!("astronomy wallpaper: Moon image failed; keeping static wallpaper");
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
            }
        }
        if let Some(load) = a.cloud_pending.as_ref().filter(|_| poll_optional) {
            match load.try_recv() {
                Ok(data) => {
                    a.cache.invalidate();
                    a.cloud_pending = None;
                    if data.width == data.height.saturating_mul(2) {
                        a.cloud_texture =
                            Self::upload_wallpaper_texture(&self.gl, &data).map(|v| v.0);
                        if let Some(texture) = a.cloud_texture
                            && !unsafe {
                                procedural_wallpaper_cache::prepare_equirectangular_texture(
                                    &self.gl, texture,
                                )
                            }
                        {
                            log::warn!(
                                "astronomy wallpaper: mipmaps unavailable; retaining base texture"
                            );
                        }
                    }
                    // Optional clouds may fail without disabling either required surface.
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    a.cloud_pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(load) = a.sky_pending.as_ref().filter(|_| poll_optional) {
            match load.try_recv() {
                Ok(data) => {
                    a.cache.invalidate();
                    a.sky_pending = None;
                    if a.max_texture_edge != 0 && data.width > a.max_texture_edge {
                        // A first decode may precede the first current GL
                        // context. Requeue once at the observed GPU limit;
                        // do not resize or reopen the source on the render thread.
                        a.sky_pending = Some(Self::load_astronomy_sky_async(
                            a.sky_path.clone(),
                            a.max_texture_edge,
                        ));
                    } else if data.width == data.height.saturating_mul(2) {
                        a.sky_texture =
                            Self::upload_wallpaper_texture(&self.gl, &data).map(|v| v.0);
                        if let Some(texture) = a.sky_texture
                            && !unsafe {
                                procedural_wallpaper_cache::prepare_equirectangular_texture(
                                    &self.gl, texture,
                                )
                            }
                        {
                            log::warn!(
                                "astronomy wallpaper: mipmaps unavailable; retaining base texture"
                            );
                        }
                    }
                    // Optional sky may fail without disabling either required surface.
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    a.sky_pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if poll_optional {
            let earth_changed =
                Self::poll_astronomy_detail(&self.gl, &mut a.earth_detail, a.earth_width);
            let moon_changed =
                Self::poll_astronomy_detail(&self.gl, &mut a.moon_detail, a.moon_width);
            if earth_changed || moon_changed {
                a.cache.invalidate();
            }
        }
        let Some(texture) = a.texture else {
            return;
        };
        if a.dual_material && a.moon_texture.is_none() {
            return;
        }
        if a.program.is_none() {
            match unsafe {
                Self::create_program(
                    &self.gl,
                    VERTEX,
                    &crate::backend::compositor_common::panorama::cosmic_shader_source(FRAGMENT),
                )
            } {
                Ok(program) => {
                    a.program = Some(unsafe { AstronomyProgram::new(&self.gl, program) });
                }
                Err(error) => {
                    // Failure stops future animation wakes. Release the uploaded
                    // image now, while this draw owns a current GL context,
                    // rather than waiting indefinitely for another frame.
                    unsafe {
                        a.fail_material(&self.gl);
                    }
                    log::warn!(
                        "astronomy wallpaper shader failed; keeping static wallpaper: {error}"
                    );
                    return;
                }
            }
        }
        let Some(program) = a.program.as_ref() else {
            return;
        };
        let cached = unsafe {
            a.cache
                .ensure(&self.gl, self.screen_w, self.screen_h, cache_ten_bit)
        };
        if cached && a.cache.valid() {
            unsafe {
                a.cache
                    .present(&self.gl, a.draw_plan.iter().map(|canvas| canvas.clip));
            }
            return;
        }
        let cache_state = cached.then(|| unsafe { a.cache.begin(&self.gl) });
        let repair = if cached { None } else { repair };
        let sun = solar_direction(a.sample_unix);
        // Compute each body's orthonormal basis once per material frame, not
        // once per output or fragment. Decorative motion never advances UTC.
        let earth_basis = a.body_matrix(CelestialBody::Earth, a.sample_unix);
        let moon_frame = (a.dual_material && a.bodies.contains(&CelestialBody::Moon)).then(|| {
            let basis = a.body_matrix(CelestialBody::Moon, a.sample_unix);
            // Retain the real Earth-observer phase while presenting any
            // lunar surface orientation: dot(M*n, M*light) == dot(n, light).
            let light = transform_direction(&basis, lunar_phase_light(a.sample_unix));
            (basis, light)
        });
        let earth_frame = (earth_basis, sun);
        let sky_view = a.sky_view(a.sample_unix);
        unsafe {
            let gl = &self.gl;
            gl.use_program(Some(program.program));
            gl.uniform_1_i32(program.earth_map.as_ref(), 0);
            gl.uniform_1_i32(program.cloud_map.as_ref(), 1);
            gl.uniform_1_i32(
                program.clouds_enabled.as_ref(),
                i32::from(a.cloud_texture.is_some()),
            );
            let roll = sky_rotation(a.sample_unix, a.sky_period);
            gl.uniform_2_f32(program.sky_rotation.as_ref(), roll[0], roll[1]);
            gl.uniform_1_i32(program.panorama_enabled.as_ref(), 1);
            gl.uniform_matrix_3_f32_slice(
                program.panorama_view.as_ref(),
                false,
                &sky_view.matrix(),
            );
            gl.uniform_1_f32(program.panorama_fov.as_ref(), sky_view.fov_degrees());
            gl.uniform_1_i32(program.sky_map.as_ref(), 2);
            gl.uniform_1_i32(
                program.sky_enabled.as_ref(),
                i32::from(a.sky_texture.is_some()),
            );
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(glow::TEXTURE_2D, a.sky_texture);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, a.cloud_texture);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.enable(glow::SCISSOR_TEST);
            for (index, canvas) in a.draw_plan.iter().enumerate() {
                let moon = a.dual_material && a.bodies.get(index) == Some(&CelestialBody::Moon);
                gl.uniform_1_i32(program.moon_material.as_ref(), i32::from(moon));
                let (basis, light) = if moon {
                    moon_frame.as_ref().unwrap_or(&earth_frame)
                } else {
                    &earth_frame
                };
                gl.uniform_matrix_3_f32_slice(program.view_to_body.as_ref(), false, basis);
                gl.uniform_3_f32(program.sun_direction.as_ref(), light[0], light[1], light[2]);
                gl.bind_texture(
                    glow::TEXTURE_2D,
                    if moon {
                        a.moon_detail.texture.or(a.moon_texture)
                    } else {
                        a.earth_detail.texture.or(Some(texture))
                    },
                );
                let clip = if let Some(repair) = repair {
                    let Some(clip) = canvas.repair_clip(Some(repair)) else {
                        continue;
                    };
                    clip
                } else {
                    canvas.clip
                };
                gl.scissor(clip.0, clip.1, clip.2, clip.3);
                gl.uniform_2_f32(
                    program.canvas_origin.as_ref(),
                    canvas.origin[0],
                    canvas.origin[1],
                );
                gl.uniform_2_f32(program.resolution.as_ref(), canvas.size[0], canvas.size[1]);
                let zoom = a.body_zoom(a.canvas_body(index)).scale();
                gl.uniform_1_f32(program.sphere_radius.as_ref(), canvas.radius * zoom);
                gl.uniform_1_f32(program.surface_detail.as_ref(), zoom);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            if let Some(clip) = repair {
                gl.scissor(clip.0, clip.1, clip.2, clip.3);
            } else {
                gl.disable(glow::SCISSOR_TEST);
            }
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_vertex_array(None);
            gl.use_program(None);
        }
        drop(cache_state);
        if cached {
            a.cache.finish();
            unsafe {
                a.cache
                    .present(&self.gl, a.draw_plan.iter().map(|canvas| canvas.clip));
            }
        }
        a.presented_earth = Some(earth_basis);
        a.presented_moon = moon_frame.map(|(basis, _)| basis);
        a.earth_zoom.presented = Some(a.earth_zoom.scale());
        a.moon_zoom.presented = a.presented_moon.map(|_| a.moon_zoom.scale());
        a.presented_sky = Some(sky_view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_wallpaper() -> AstronomyWallpaper {
        AstronomyWallpaper {
            enabled: true,
            powered: true,
            earth_period: 20,
            moon_period: 30,
            draw_plan: astronomy_draw_plan((1920, 1080), &[], false, None),
            ..Default::default()
        }
    }

    fn draggable_wallpaper() -> AstronomyWallpaper {
        let mut a = active_wallpaper();
        a.layout_screen = (1920, 1080);
        a.layout = "span".into();
        a.sample_unix = 1792108800.0;
        a.presented_earth = Some(a.body_matrix(CelestialBody::Earth, a.sample_unix));
        a.presented_moon = Some(a.body_matrix(CelestialBody::Moon, a.sample_unix));
        a
    }

    fn panoramic_wallpaper() -> AstronomyWallpaper {
        let mut a = draggable_wallpaper();
        a.presented_sky = Some(a.sky_view(a.sample_unix));
        a
    }

    fn pointer_at(canvas: &AstronomyDraw, height: u32, point: [f64; 2]) -> [f64; 2] {
        let x = f64::from(canvas.origin[0])
            + f64::from(canvas.size[0]) * 0.5
            + f64::from(canvas.radius.max(1.0)) * point[0];
        let y = f64::from(canvas.origin[1])
            + f64::from(canvas.size[1]) * 0.5
            + f64::from(canvas.radius.max(1.0)) * point[1];
        [x - 0.5, f64::from(height) - y - 0.5]
    }

    fn assert_same_basis(actual: [f32; 9], expected: [f32; 9]) {
        for (actual, expected) in actual.into_iter().zip(expected) {
            assert!((actual - expected).abs() < 2e-6, "{actual} != {expected}");
        }
    }

    #[test]
    fn wheel_zoom_is_coalesced_bounded_and_hit_tests_only_presented_pixels() {
        let now = Instant::now();
        let mut a = draggable_wallpaper();
        let base = a.draw_plan[0].clone();
        let center = pointer_at(&base, a.layout_screen.1, [0.0, 0.0]);
        let outside = pointer_at(&base, a.layout_screen.1, [1.3, 0.0]);
        let unix = a.sample_unix;
        assert!(a.zoom(center, 4.0));
        assert_eq!(a.earth_zoom.scale(), 1.0);
        assert_eq!(a.moon_zoom.scale(), 1.0);
        assert!(a.hit_test(outside).is_none());
        assert!(a.consume_interaction(now));
        assert!((a.earth_zoom.scale() - (4.0 * ZOOM_LOG_STEP).exp() as f32).abs() < 1e-6);
        assert!(a.hit_test(outside).is_none());
        a.earth_zoom.presented = Some(a.earth_zoom.scale());
        assert_eq!(a.hit_test(outside).unwrap().0, CelestialBody::Earth);
        assert!(
            (a.hit_test(outside).unwrap().1.radius - base.radius * a.earth_zoom.scale()).abs()
                < 1e-5
        );
        assert_eq!(a.sample_unix, unix);
        assert!(a.zoom(center, -4.0));
        assert!(a.consume_interaction(now + DRAG_FRAME_INTERVAL));
        assert!((a.earth_zoom.scale() - 1.0).abs() < 1e-6);
        assert!(a.interaction_deadline(now).is_none());
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0] {
            assert!(!a.zoom(center, invalid));
        }
        assert!(!a.zoom([f64::NAN, 0.0], 1.0));
        assert!(a.zoom(center, 1e100));
        assert!(a.consume_interaction(now + DRAG_FRAME_INTERVAL * 2));
        assert_eq!(a.earth_zoom.scale(), MAX_BODY_ZOOM as f32);
        assert!(!a.zoom(center, 1.0));
        assert!(a.zoom(center, -1e100));
        assert!(a.consume_interaction(now + DRAG_FRAME_INTERVAL * 3));
        assert_eq!(a.earth_zoom.scale(), MIN_BODY_ZOOM as f32);
        assert!(!a.zoom(center, -1.0));
    }

    #[test]
    fn grabbing_visible_zoom_freezes_geometry_and_refusal_restores_queued_wheel() {
        let now = Instant::now();
        let mut a = draggable_wallpaper();
        let center = pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.0, 0.0]);
        assert!(a.zoom(center, 3.0));
        assert!(a.consume_interaction(now));
        let requested = a.earth_zoom.requested_log;
        let admitted = a.earth_zoom.admitted_log;
        assert!(a.begin_drag(center));
        assert_eq!(a.earth_zoom.scale(), 1.0);
        assert!(!a.zoom(center, 1.0));
        a.abort_drag();
        assert_eq!(a.earth_zoom.requested_log, requested);
        assert_eq!(a.earth_zoom.admitted_log, admitted);
        assert!(!a.has_drag());
        a.earth_zoom.presented = Some(a.earth_zoom.scale());
        assert!(a.begin_drag(center));
        let canvas = a.drag.as_ref().unwrap().canvas.clone();
        let target = pointer_at(&canvas, a.layout_screen.1, [0.4, 0.3]);
        assert!(a.drag_motion(target));
        assert!(a.consume_interaction(now + DRAG_FRAME_INTERVAL));
        let expected = CelestialArcball::new(
            [0.0, 0.0],
            CelestialRotation::from_matrix(&a.presented_earth.unwrap()),
        )
        .unwrap()
        .orientation_at([0.4, 0.3])
        .unwrap()
        .matrix();
        assert_same_basis(a.body_matrix(CelestialBody::Earth, a.sample_unix), expected);
    }

    #[test]
    fn earth_and_moon_zoom_are_independent_and_reset_only_matching_configuration() {
        let now = Instant::now();
        let mut a = draggable_wallpaper();
        a.layout_screen = (200, 100);
        a.draw_plan = astronomy_draw_plan(
            a.layout_screen,
            &[(0, 0, 100, 100), (100, 0, 100, 100)],
            true,
            None,
        );
        a.dual_material = true;
        a.bodies = vec![CelestialBody::Earth, CelestialBody::Moon];
        let earth = pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.0, 0.0]);
        let moon = pointer_at(&a.draw_plan[1], a.layout_screen.1, [0.0, 0.0]);
        assert!(a.zoom(earth, 2.0));
        assert!(a.zoom(moon, 5.0));
        assert!(a.consume_interaction(now));
        let moon_scale = a.moon_zoom.scale();
        assert!(moon_scale > a.earth_zoom.scale());
        assert!(!a.update_views(a.earth_view, a.moon_view));
        assert_eq!(a.moon_zoom.scale(), moon_scale);
        let mut earth_view = a.earth_view;
        earth_view.roll += 45.0;
        assert!(a.update_views(earth_view, a.moon_view));
        assert_eq!(a.earth_zoom.scale(), 1.0);
        assert_eq!(a.moon_zoom.scale(), moon_scale);
    }

    #[test]
    fn panorama_drag_freezes_presented_sky_and_keeps_true_light_and_release_endpoint() {
        let now = Instant::now();
        let mut a = panoramic_wallpaper();
        let unix = a.sample_unix;
        a.sky_period = 1;
        let displayed = a.sky_view(unix - 0.7);
        a.presented_sky = Some(displayed);
        let start = [8.0, 8.0];
        let endpoint = [2220.0, 1450.0];
        let body = a.body_matrix(CelestialBody::Earth, unix);
        assert!(a.begin_drag(start));
        assert!(a.drag.is_none() && a.sky_drag.is_some());
        let capture = a.sky_drag.as_ref().unwrap();
        let expected = capture
            .gesture
            .orientation_at(panorama_pointer(&capture.canvas, endpoint, a.layout_screen.1).unwrap())
            .unwrap();
        assert!(a.update_clock(now, unix, 28800, false, false));
        assert_same_basis(a.sky_view(unix).matrix(), displayed.matrix());
        assert_eq!(a.frame_interval(), Duration::from_secs(1));
        assert!(a.drag_motion([200.0, 500.0]));
        assert!(a.drag_motion(endpoint));
        assert!(a.update_clock(
            now + DRAG_FRAME_INTERVAL,
            unix + 7200.0,
            28800,
            false,
            false
        ));
        assert_same_basis(a.sky_view(a.sample_unix).matrix(), expected.matrix());
        assert_same_basis(a.body_matrix(CelestialBody::Earth, a.sample_unix), body);
        assert_ne!(solar_direction(unix), solar_direction(a.sample_unix));
        assert_ne!(lunar_phase_light(unix), lunar_phase_light(a.sample_unix));
        a.presented_sky = Some(a.sky_view(a.sample_unix));
        let old_sample = a.sample_unix;
        // Releasing without another MotionNotify still incorporates its endpoint.
        let final_point = [-1100.0, -720.0];
        let capture = a.sky_drag.as_ref().unwrap();
        let final_pose = capture
            .gesture
            .orientation_at(
                panorama_pointer(&capture.canvas, final_point, a.layout_screen.1).unwrap(),
            )
            .unwrap();
        assert!(a.drag_motion(final_point));
        assert!(a.end_drag(unix + 7210.0));
        assert_same_basis(a.sky_view(old_sample).matrix(), expected.matrix());
        assert!(a.sky_release.is_some());
        assert!(a.update_clock(
            now + DRAG_FRAME_INTERVAL * 2,
            unix + 7210.0,
            28800,
            false,
            false
        ));
        assert_same_basis(a.sky_view(a.sample_unix).matrix(), final_pose.matrix());
        assert!(a.sky_release.is_none());
        assert!(a.interaction_deadline(now).is_none());
    }

    #[test]
    fn panorama_wheel_changes_only_sky_lens_and_grab_refusal_preserves_pending_state() {
        let now = Instant::now();
        let mut a = panoramic_wallpaper();
        let corner = [8.0, 8.0];
        let initial = a.panorama;
        assert!(a.zoom(corner, 3.0));
        let requested = a.requested_panorama.unwrap();
        assert!(requested.fov_degrees() < initial.fov_degrees());
        assert_eq!(a.panorama, initial);
        assert_eq!(a.earth_zoom.scale(), 1.0);
        assert_eq!(a.moon_zoom.scale(), 1.0);
        assert!(a.begin_drag(corner));
        assert!(a.requested_panorama.is_none());
        assert!(!a.zoom(corner, 1.0));
        a.abort_drag();
        assert!(a.sky_drag.is_none());
        assert_eq!(a.panorama, initial);
        assert_eq!(a.requested_panorama, Some(requested));
        assert!(a.interaction_pending);
        assert!(a.consume_interaction(now));
        assert_eq!(a.panorama.fov_degrees(), requested.fov_degrees());
        assert_eq!(
            a.presented_sky.unwrap().fov_degrees(),
            initial.fov_degrees()
        );
        assert!(!a.zoom([-100.0, -100.0], 1.0));
        assert!(!a.zoom(corner, f64::NAN));
    }

    #[test]
    fn panorama_body_priority_and_lifecycle_retire_all_interaction_wakes() {
        let now = Instant::now();
        for reason in 0..3 {
            let mut a = panoramic_wallpaper();
            let center = pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.0, 0.0]);
            assert!(a.begin_drag(center));
            assert!(a.drag.is_some() && a.sky_drag.is_none());
            a.abort_drag();
            assert!(a.begin_drag([8.0, 8.0]));
            assert!(a.drag_motion([600.0, 500.0]));
            match reason {
                0 => {
                    a.powered = false;
                    a.update_clock(now, a.sample_unix, 0, false, false);
                }
                1 => {
                    a.update_clock(now, a.sample_unix, 0, true, false);
                }
                _ => {
                    a.take_failed_material();
                }
            }
            assert!(!a.has_drag());
            assert!(a.sky_release.is_none());
            assert!(!a.interaction_pending);
            assert!(a.interaction_deadline(now).is_none());
        }
        let mut a = panoramic_wallpaper();
        assert!(a.zoom([8.0, 8.0], 2.0));
        assert!(a.consume_interaction(now));
        a.reset_sky();
        assert_eq!(a.panorama, PanoramaView::default());
        assert!(a.requested_panorama.is_none());
        assert!(a.presented_sky.is_none());
    }

    #[test]
    fn detail_loading_is_lazy_gpu_bounded_and_attempted_once_without_blocking_base() {
        let mut a = draggable_wallpaper();
        a.path = "/unused/detail-source.jpg".into();
        a.earth_width = 4096;
        a.earth_zoom.request(4.0);
        a.earth_zoom.admit();
        a.request_details();
        assert!(!a.earth_detail.attempted);
        a.max_texture_edge = 4096;
        a.request_details();
        assert!(a.earth_detail.attempted);
        assert!(a.earth_detail.pending.is_none());
        a.earth_detail = AstronomyDetail::default();
        a.max_texture_edge = 16384;
        a.request_details();
        assert!(a.earth_detail.pending.is_some());
        assert!(!a.moon_detail.attempted);
        // A cancelled/failed optional result must not start a decoder each tick.
        a.earth_detail.pending = None;
        a.request_details();
        assert!(a.earth_detail.pending.is_none());
        assert!(!a.failed);
        assert!(a.presented_earth.is_some());
        let mut ordinary = draggable_wallpaper();
        ordinary.max_texture_edge = 8192;
        ordinary.earth_width = 4096;
        ordinary.path = a.path;
        ordinary.request_details();
        assert!(!ordinary.earth_detail.attempted);
        ordinary.earth_zoom.request(4.0);
        ordinary.earth_zoom.admit();
        ordinary.powered = false;
        ordinary.request_details();
        assert!(!ordinary.earth_detail.attempted);
    }

    #[test]
    fn terminal_material_failure_releases_optional_detail_textures_too() {
        let earth = glow::NativeTexture(std::num::NonZeroU32::new(101).unwrap());
        let moon = glow::NativeTexture(std::num::NonZeroU32::new(102).unwrap());
        let mut a = panoramic_wallpaper();
        a.earth_detail.texture = Some(earth);
        a.moon_detail.texture = Some(moon);
        assert_eq!(
            a.take_failed_material(),
            [None, None, None, None, Some(earth), Some(moon)]
        );
        assert_eq!(a.take_failed_material(), [None; 6]);
        assert!(a.presented_sky.is_none());
    }

    #[test]
    fn sky_decoder_preserves_packed_rgba_and_gpu_downscale_never_uses_alpha_as_opacity() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "jwm-packed-sky-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("sky.png");
        let fixture = image::RgbaImage::from_fn(16, 8, |x, _| {
            image::Rgba([80, 140, 190, if x < 8 { 0 } else { 255 }])
        });
        fixture.save(&path).unwrap();
        let cancelled = AtomicBool::new(false);
        let native = decode_astronomy_sky(path.to_str().unwrap(), 0, &cancelled).unwrap();
        assert_eq!((native.width, native.height), (16, 8));
        assert_eq!(&*native.rgba, fixture.as_raw());
        let reduced = decode_astronomy_sky(path.to_str().unwrap(), 8, &cancelled).unwrap();
        assert_eq!((reduced.width, reduced.height), (8, 4));
        for pixel in reduced.rgba.chunks_exact(4) {
            for (actual, expected) in pixel[..3].iter().zip([80, 140, 190]) {
                assert!((i16::from(*actual) - expected).abs() <= 1);
            }
        }
        assert!(reduced.rgba.chunks_exact(4).any(|pixel| pixel[3] == 0));
        assert!(reduced.rgba.chunks_exact(4).any(|pixel| pixel[3] == 255));
        assert!(decode_astronomy_sky(path.to_str().unwrap(), 1, &cancelled).is_none());
        let legacy = directory.join("legacy.jpg");
        image::RgbImage::from_pixel(16, 8, image::Rgb([80, 140, 190]))
            .save(&legacy)
            .unwrap();
        let legacy = decode_astronomy_sky(legacy.to_str().unwrap(), 8, &cancelled).unwrap();
        assert!(legacy.rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));
        image::RgbaImage::new(3, 2).save(&path).unwrap();
        assert!(decode_astronomy_sky(path.to_str().unwrap(), 4096, &cancelled).is_none());
        cancelled.store(true, Ordering::Release);
        assert!(decode_astronomy_sky(path.to_str().unwrap(), 4096, &cancelled).is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pointer_hit_testing_matches_rendered_globe_clip_origin_and_painter_order() {
        let mut a = draggable_wallpaper();
        a.layout_screen = (200, 100);
        a.draw_plan = astronomy_draw_plan(
            a.layout_screen,
            &[(0, 0, 100, 100), (100, 0, 100, 100)],
            true,
            None,
        );
        a.dual_material = true;
        a.bodies = vec![CelestialBody::Earth, CelestialBody::Moon];
        for (index, body) in [CelestialBody::Earth, CelestialBody::Moon]
            .into_iter()
            .enumerate()
        {
            let point = [0.3, -0.2];
            let root = pointer_at(&a.draw_plan[index], a.layout_screen.1, point);
            let hit = a.hit_test(root).unwrap();
            assert_eq!(hit.0, body);
            assert_eq!(hit.1, a.draw_plan[index]);
            assert!((hit.2[0] - point[0]).abs() < 1e-12);
            assert!((hit.2[1] - point[1]).abs() < 1e-12);
        }
        assert!(a.hit_test([0.0, 0.0]).is_none());
        assert!(a.hit_test([-1.0, 50.0]).is_none());
        assert!(a.hit_test([f64::NAN, 50.0]).is_none());

        a.layout_screen = (200, 200);
        a.draw_plan = astronomy_draw_plan(
            a.layout_screen,
            &[(0, 0, 200, 200), (120, 0, 40, 200)],
            true,
            None,
        );
        // This point is on the lower Earth, but covered by the upper canvas sky.
        assert!(a.hit_test([129.5, 159.5]).is_none());
        assert_eq!(a.hit_test([139.5, 99.5]).unwrap().0, CelestialBody::Moon);

        a.layout_screen = (100, 100);
        a.draw_plan = astronomy_draw_plan(a.layout_screen, &[(-20, -20, 200, 200)], true, None);
        a.bodies = vec![CelestialBody::Earth];
        assert_eq!(a.hit_test([79.5, 79.5]).unwrap().2, [0.0, 0.0]);
        // The original sphere extends beyond the root; its clipped pixels cannot be grabbed.
        assert!(a.hit_test([-4.5, 79.5]).is_none());
    }

    #[test]
    fn press_freezes_the_presented_pose_while_true_sun_and_phase_keep_advancing() {
        let now = Instant::now();
        let mut a = draggable_wallpaper();
        a.earth_view.rotating = true;
        a.earth_period = 1;
        let displayed = a.earth_view.matrix(a.sample_unix - 0.9, a.earth_period);
        a.presented_earth = Some(displayed);
        let root = pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.0, 0.0]);
        assert!(a.begin_drag(root));
        let unix = a.sample_unix;
        assert!(a.update_clock(now, unix, 28800, false, false));
        assert_same_basis(a.body_matrix(CelestialBody::Earth, unix), displayed);
        assert_eq!(a.frame_interval(), Duration::from_secs(1));
        let sun = solar_direction(a.sample_unix);
        let phase = lunar_phase_light(a.sample_unix);
        assert!(a.update_clock(
            now + Duration::from_secs(7200),
            unix + 7200.0,
            28800,
            false,
            false
        ));
        assert_eq!(a.sample_unix, unix + 7200.0);
        assert_ne!(solar_direction(a.sample_unix), sun);
        assert_ne!(lunar_phase_light(a.sample_unix), phase);
        assert_same_basis(
            a.body_matrix(CelestialBody::Earth, a.sample_unix),
            displayed,
        );
        assert!(
            a.interaction_deadline(now + Duration::from_secs(7200))
                .is_none()
        );
    }

    #[test]
    fn queued_mouse_motion_is_coalesced_and_stationary_grabs_stop_fast_wakes() {
        let now = Instant::now();
        let mut a = draggable_wallpaper();
        a.last_power_query = Some(now);
        let start = pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.0, 0.0]);
        let first = pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.1, 0.2]);
        let last = pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.65, -0.4]);
        let unix = a.sample_unix;
        assert!(a.begin_drag(start));
        assert!(a.update_clock(now, unix, 0, false, false));
        let before = a.body_matrix(CelestialBody::Earth, unix);
        let key = a.bar_metric_source().0;
        assert!(a.drag_motion(first));
        assert!(!a.update_clock(
            now + Duration::from_millis(1),
            unix + 0.001,
            0,
            false,
            false
        ));
        assert_eq!(a.body_matrix(CelestialBody::Earth, unix), before);
        assert_eq!(a.bar_metric_source().0, key);
        assert_eq!(
            a.deadline(now + Duration::from_millis(1)),
            Some(DRAG_FRAME_INTERVAL - Duration::from_millis(1))
        );
        assert!(a.drag_motion(last));
        assert!(a.update_clock(
            now + DRAG_FRAME_INTERVAL,
            unix + 1.0 / 60.0,
            0,
            false,
            false
        ));
        let expected = CelestialArcball::new([0.0, 0.0], CelestialRotation::from_matrix(&before))
            .unwrap()
            .orientation_at([0.65, -0.4])
            .unwrap()
            .matrix();
        assert_same_basis(a.body_matrix(CelestialBody::Earth, a.sample_unix), expected);
        assert_ne!(a.bar_metric_source().0, key);
        assert!(a.interaction_deadline(now + DRAG_FRAME_INTERVAL).is_none());
        assert!(!a.update_clock(
            now + Duration::from_millis(80),
            unix + 0.08,
            0,
            false,
            false
        ));
        // Repeated identical motion events must not re-arm the fast clock.
        assert!(a.drag_motion(last));
        assert!(!a.interaction_pending);
    }

    #[test]
    fn releasing_unrendered_final_motion_rebases_without_snapping_after_clock_correction() {
        let mut a = draggable_wallpaper();
        a.earth_view = CelestialView {
            rotating: true,
            latitude: 77.0,
            longitude: 168.0,
            roll: -65.0,
            axis: [0.2, -1.0, 0.3],
        };
        a.presented_earth = Some(a.body_matrix(CelestialBody::Earth, a.sample_unix));
        let press_basis = CelestialRotation::from_matrix(&a.presented_earth.unwrap());
        let start = [0.2, -0.3];
        let end = [2.5, -1.8];
        assert!(a.begin_drag(pointer_at(&a.draw_plan[0], a.layout_screen.1, start)));
        // Captured motion remains valid outside the starting globe and output.
        assert!(a.drag_motion(pointer_at(&a.draw_plan[0], a.layout_screen.1, end)));
        let released = a.sample_unix - 3600.0;
        let target = CelestialArcball::new(start, press_basis)
            .unwrap()
            .orientation_at(end)
            .unwrap();
        assert!(a.end_drag(released));
        assert!(!a.end_drag(released));
        assert!(a.interaction_pending);
        assert!(a.consume_interaction(Instant::now()));
        assert_same_basis(
            a.body_matrix(CelestialBody::Earth, released),
            target.matrix(),
        );
        let later = released + 0.1;
        let expected = a
            .base_rotation(CelestialBody::Earth, later)
            .compose(a.base_rotation(CelestialBody::Earth, released).inverse())
            .compose(target);
        assert_same_basis(
            a.body_matrix(CelestialBody::Earth, later),
            expected.matrix(),
        );
        assert!(a.interaction_deadline(Instant::now()).is_none());
    }

    #[test]
    fn release_and_time_sample_commit_together_even_without_a_framebuffer_cache() {
        let now = Instant::now();
        let mut a = draggable_wallpaper();
        a.earth_view.rotating = true;
        a.earth_period = 1;
        let unix = a.sample_unix;
        a.presented_earth = Some(a.body_matrix(CelestialBody::Earth, unix));
        let press = CelestialRotation::from_matrix(&a.presented_earth.unwrap());
        assert!(a.begin_drag(pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.0, 0.0])));
        assert!(a.update_clock(now, unix, 0, false, false));
        assert!(a.drag_motion(pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.3, 0.2])));
        let last_frame = now + DRAG_FRAME_INTERVAL;
        assert!(a.update_clock(
            last_frame,
            unix + DRAG_FRAME_INTERVAL.as_secs_f64(),
            0,
            false,
            false
        ));
        // Record the exact pose produced by an actual material draw.
        let presented = a.body_matrix(CelestialBody::Earth, a.sample_unix);
        a.presented_earth = Some(presented);
        let last_sample = a.sample_unix;
        let endpoint = [-0.4, 0.55];
        assert!(a.drag_motion(pointer_at(&a.draw_plan[0], a.layout_screen.1, endpoint)));
        // A wall-clock adjustment makes accidental use of the old time obvious.
        let released = last_sample + 15.008;
        assert!(a.end_drag(released));
        assert!(!a.update_clock(
            last_frame + Duration::from_millis(8),
            released,
            0,
            false,
            false
        ));
        assert_eq!(a.sample_unix, last_sample);
        assert_eq!(a.body_matrix(CelestialBody::Earth, last_sample), presented);
        assert!(a.earth_offset.is_none());
        assert!(a.earth_release.is_some());

        // Unrelated client damage can draw directly without a cache before the
        // input deadline. It must see the previously presented complete frame.
        assert!(!a.cache.valid());
        assert_eq!(
            a.body_matrix(CelestialBody::Earth, a.sample_unix),
            presented
        );
        let next_sample = released + DRAG_FRAME_INTERVAL.as_secs_f64() - 0.008;
        assert!(a.update_clock(
            last_frame + DRAG_FRAME_INTERVAL,
            next_sample,
            0,
            false,
            false
        ));
        let final_pose = CelestialArcball::new([0.0, 0.0], press)
            .unwrap()
            .orientation_at(endpoint)
            .unwrap();
        let expected = a
            .base_rotation(CelestialBody::Earth, next_sample)
            .compose(a.base_rotation(CelestialBody::Earth, released).inverse())
            .compose(final_pose);
        assert_eq!(a.sample_unix, next_sample);
        assert!(a.earth_release.is_none());
        assert_same_basis(
            a.body_matrix(CelestialBody::Earth, a.sample_unix),
            expected.matrix(),
        );
    }

    #[test]
    fn refused_grab_restores_unchanged_state_and_preserves_an_earlier_queued_release() {
        let now = Instant::now();
        let mut a = draggable_wallpaper();
        a.last_power_query = Some(now);
        let unix = a.sample_unix;
        assert!(a.update_clock(now, unix, 0, false, false));
        let center = pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.0, 0.0]);
        let previous = (
            a.earth_offset,
            a.moon_offset,
            a.deadline(now),
            a.sample_unix,
            a.interaction_revision,
            a.last_interaction_frame,
        );
        assert!(a.begin_drag(center));
        a.abort_drag();
        assert!(a.drag.is_none());
        assert!(!a.interaction_pending);
        assert_eq!(
            (
                a.earth_offset,
                a.moon_offset,
                a.deadline(now),
                a.sample_unix,
                a.interaction_revision,
                a.last_interaction_frame,
            ),
            previous,
        );

        assert!(a.begin_drag(center));
        assert!(a.drag_motion(pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.4, -0.3])));
        assert!(a.end_drag(unix + 0.01));
        let release = a.earth_release.as_ref().unwrap();
        let previous_release = (release.presented, release.offset);
        let deadline = a.deadline(now);
        assert!(a.begin_drag(center));
        // An immediate regrab starts from pixels already displayed, not from
        // the invisible endpoint of the still-queued previous gesture.
        assert_same_basis(
            a.body_matrix(CelestialBody::Earth, unix),
            a.presented_earth.unwrap(),
        );
        a.abort_drag();
        let release = a.earth_release.as_ref().unwrap();
        assert_eq!((release.presented, release.offset), previous_release);
        assert!(a.interaction_pending);
        assert_eq!(a.deadline(now), deadline);
        assert!(a.consume_interaction(now));
        assert_eq!(a.earth_offset, Some(previous_release.1));
        assert!(a.earth_release.is_none());
    }

    #[test]
    fn earth_and_moon_have_independent_session_offsets_and_targeted_config_resets() {
        let mut a = draggable_wallpaper();
        a.layout_screen = (200, 100);
        a.draw_plan = astronomy_draw_plan(
            a.layout_screen,
            &[(0, 0, 100, 100), (100, 0, 100, 100)],
            true,
            None,
        );
        a.dual_material = true;
        a.bodies = vec![CelestialBody::Earth, CelestialBody::Moon];
        for index in [1, 0] {
            let other = if index == 1 {
                CelestialBody::Earth
            } else {
                CelestialBody::Moon
            };
            let previous = a.body_matrix(other, a.sample_unix);
            assert!(a.begin_drag(pointer_at(
                &a.draw_plan[index],
                a.layout_screen.1,
                [0.0, 0.0]
            )));
            assert!(a.drag_motion(pointer_at(
                &a.draw_plan[index],
                a.layout_screen.1,
                [-0.3, 0.55]
            )));
            assert!(a.end_drag(a.sample_unix));
            assert_same_basis(a.body_matrix(other, a.sample_unix), previous);
        }
        // Both bodies can finish a quick gesture before one coalesced frame.
        assert!(a.earth_release.is_some() && a.moon_release.is_some());
        assert!(a.consume_interaction(Instant::now()));
        assert!(a.earth_offset.is_some());
        let moon_offset = a.moon_offset.unwrap().matrix();
        assert!(!a.update_views(a.earth_view, a.moon_view));
        assert!(a.earth_offset.is_some());
        let mut earth = a.earth_view;
        earth.roll = 45.0;
        assert!(a.update_views(earth, a.moon_view));
        assert!(a.earth_offset.is_none());
        assert_eq!(a.moon_offset.unwrap().matrix(), moon_offset);
        let fixed_moon = a.body_matrix(CelestialBody::Moon, a.sample_unix);
        assert_eq!(
            a.body_matrix(CelestialBody::Moon, a.sample_unix + 86400.0),
            fixed_moon
        );
    }

    #[test]
    fn camera_changes_discard_only_the_matching_bodys_queued_release() {
        let mut a = draggable_wallpaper();
        a.layout_screen = (200, 100);
        a.draw_plan = astronomy_draw_plan(
            a.layout_screen,
            &[(0, 0, 100, 100), (100, 0, 100, 100)],
            true,
            None,
        );
        a.dual_material = true;
        a.bodies = vec![CelestialBody::Earth, CelestialBody::Moon];
        for index in [0, 1] {
            assert!(a.begin_drag(pointer_at(
                &a.draw_plan[index],
                a.layout_screen.1,
                [0.0, 0.0]
            )));
            assert!(a.drag_motion(pointer_at(
                &a.draw_plan[index],
                a.layout_screen.1,
                [0.3, 0.2]
            )));
            assert!(a.end_drag(a.sample_unix));
        }
        let moon_offset = a.moon_release.as_ref().unwrap().offset;
        let mut earth = a.earth_view;
        earth.roll = 45.0;
        assert!(a.update_views(earth, a.moon_view));
        assert!(a.earth_release.is_none());
        assert!(a.moon_release.is_some());
        assert!(a.consume_interaction(Instant::now()));
        assert!(a.earth_offset.is_none());
        assert_eq!(a.moon_offset, Some(moon_offset));
    }

    #[test]
    fn lost_power_occlusion_and_failed_material_clear_capture_and_fast_deadlines() {
        for reason in 0..3 {
            let now = Instant::now();
            let mut a = draggable_wallpaper();
            let unix = a.sample_unix;
            assert!(a.begin_drag(pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.0, 0.0])));
            assert!(a.drag_motion(pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.5, 0.2])));
            if reason == 2 {
                a.take_failed_material();
                assert!(a.deadline(now).is_none());
                assert!(a.presented_earth.is_none() && a.presented_moon.is_none());
            } else {
                if reason == 0 {
                    a.powered = false;
                }
                a.update_clock(now, unix + 1.0, 0, reason == 1, false);
                assert!(a.clock.deadline(now).is_none());
            }
            assert!(a.drag.is_none());
            assert!(a.earth_offset.is_some());
            assert!(!a.interaction_pending);
            assert!(a.interaction_deadline(now).is_none());
        }
    }

    #[test]
    fn unpresented_fallback_and_stale_camera_configuration_reject_capture() {
        let mut a = draggable_wallpaper();
        let center = pointer_at(&a.draw_plan[0], a.layout_screen.1, [0.0, 0.0]);
        a.presented_earth = None;
        assert!(!a.begin_drag(center));
        let mut behavior = crate::config::Config::default().behavior().clone();
        behavior.wallpaper_effect = "astronomy".into();
        a.layout = behavior.wallpaper_astronomy_layout.clone();
        a.configured_path = behavior.wallpaper_earth_texture.clone();
        a.moon_configured_path = behavior.wallpaper_moon_texture.clone();
        a.body_overrides = behavior.wallpaper_astronomy_bodies.clone();
        (a.earth_view, a.moon_view) = configured_views(&behavior);
        a.earth_period = behavior.wallpaper_earth_rotation_minutes;
        a.moon_period = behavior.wallpaper_moon_rotation_minutes;
        a.sky_period = behavior.wallpaper_sky_rotation_minutes;
        assert!(a.config_matches(&behavior));
        behavior.wallpaper_moon_roll += 1.0;
        assert!(!a.config_matches(&behavior));
        behavior.wallpaper_moon_roll -= 1.0;
        behavior.wallpaper_effect = "none".into();
        assert!(!a.config_matches(&behavior));
    }

    #[test]
    fn camera_reload_changes_only_framing_and_requests_one_repaint() {
        let mut a = AstronomyWallpaper {
            sample_unix: 1792108800.0,
            texture: Some(glow::NativeTexture(std::num::NonZeroU32::new(1).unwrap())),
            moon_texture: Some(glow::NativeTexture(std::num::NonZeroU32::new(2).unwrap())),
            ..active_wallpaper()
        };
        let textures = (a.texture, a.moon_texture);
        let mut earth = CelestialView {
            latitude: 25.0,
            longitude: 120.0,
            ..Default::default()
        };
        let mut moon = CelestialView::default();
        assert!(a.update_views(earth, moon));
        assert!(!a.update_views(earth, moon));
        let key = a.bar_metric_source().0;
        earth.roll = 45.0;
        earth.axis = [1.0, 1.0, -0.5];
        earth.rotating = true;
        assert!(a.update_views(earth, moon));
        assert_ne!(a.bar_metric_source().0, key);
        assert!(!a.update_views(earth, moon));
        moon.latitude = -90.0;
        moon.longitude = 180.0;
        moon.roll = -120.0;
        moon.axis = [-1.0, 0.0, 0.0];
        moon.rotating = true;
        assert!(a.update_views(earth, moon));
        assert!(!a.update_views(earth, moon));
        assert_eq!(
            (a.sample_unix, a.earth_period, a.moon_period),
            (1792108800.0, 20, 30)
        );
        assert_eq!((a.texture, a.moon_texture), textures);
        assert!(a.pending.is_none() && a.moon_pending.is_none());
        assert!(a.retired.is_empty());
    }

    #[test]
    fn fixed_and_hidden_bodies_do_not_raise_render_cadence() {
        let mut a = active_wallpaper();
        a.earth_period = 1;
        a.moon_period = 1;
        a.moon_view.rotating = true;
        a.bodies = vec![CelestialBody::Moon];
        // A Moon assignment is ignored outside earth-moon layout.
        assert_eq!(a.frame_interval(), Duration::from_secs(1));
        a.earth_view.rotating = true;
        assert_eq!(
            a.frame_interval(),
            crate::backend::compositor_common::animated_wallpaper::FRAME_INTERVAL
        );
        a.dual_material = true;
        a.moon_view.rotating = false;
        assert_eq!(a.frame_interval(), Duration::from_secs(1));
        a.moon_view.rotating = true;
        a.moon_period = 39343;
        assert_eq!(a.frame_interval(), Duration::from_secs(1));
        a.bodies = vec![CelestialBody::Earth];
        a.earth_view.rotating = false;
        a.moon_period = 1;
        assert_eq!(a.frame_interval(), Duration::from_secs(1));
        a.sky_period = 1;
        assert_eq!(
            a.frame_interval(),
            crate::backend::compositor_common::animated_wallpaper::FRAME_INTERVAL
        );
    }

    #[test]
    fn timezone_and_midnight_changes_preserve_one_real_lighting_instant() {
        let mut a = active_wallpaper();
        let now = Instant::now();
        a.earth_view = CelestialView {
            rotating: true,
            latitude: 80.0,
            longitude: -175.0,
            roll: 67.0,
            axis: [0.4, -0.3, 1.0],
        };
        let local_times = [
            "2026-10-17T00:05:00+08:00",
            "2026-10-16T16:05:00Z",
            "2026-10-16T12:05:00-04:00",
            "2026-10-16T21:50:00+05:45",
        ];
        let mut first_frame = None;
        for (index, local) in local_times.into_iter().enumerate() {
            let local = chrono::DateTime::parse_from_rfc3339(local).unwrap();
            assert!(a.update_clock(
                now + Duration::from_millis(index as u64 * 10),
                local.timestamp() as f64,
                local.offset().local_minus_utc(),
                false,
                false,
            ));
            let frame = (
                a.sample_unix,
                a.earth_view.matrix(a.sample_unix, a.earth_period),
                solar_direction(a.sample_unix),
                lunar_phase_light(a.sample_unix),
            );
            if let Some(expected) = first_frame {
                assert_eq!(frame, expected);
            } else {
                first_frame = Some(frame);
            }
        }
        // A timezone repaint never resets the capped monotonic frame clock.
        assert!(!a.update_clock(
            now + Duration::from_millis(31),
            a.sample_unix + 0.031,
            a.sample_offset,
            false,
            false,
        ));
    }

    #[test]
    fn repeated_dst_local_hour_advances_the_sun_by_the_real_elapsed_hour() {
        let mut a = active_wallpaper();
        let now = Instant::now();
        let before = chrono::DateTime::parse_from_rfc3339("2026-11-01T01:30:00-04:00").unwrap();
        let after = chrono::DateTime::parse_from_rfc3339("2026-11-01T01:30:00-05:00").unwrap();
        assert_eq!(after.timestamp() - before.timestamp(), 3600);
        assert!(a.update_clock(
            now,
            before.timestamp() as f64,
            before.offset().local_minus_utc(),
            false,
            false,
        ));
        let sun = solar_direction(a.sample_unix);
        assert!(a.update_clock(
            now + Duration::from_secs(3600),
            after.timestamp() as f64,
            after.offset().local_minus_utc(),
            false,
            false,
        ));
        assert_eq!(a.sample_unix, after.timestamp() as f64);
        assert_ne!(sun, solar_direction(a.sample_unix));
        assert_eq!(
            solar_direction(a.sample_unix),
            solar_direction(before.timestamp() as f64 + 3600.0)
        );
    }

    #[test]
    fn control_updates_and_resume_take_fresh_time_without_catchup_frames() {
        let mut a = active_wallpaper();
        let now = Instant::now();
        let unix = 1792108800.0;
        assert!(a.update_clock(now, unix, 28800, false, false));
        assert!(!a.update_clock(
            now + Duration::from_millis(10),
            unix + 0.01,
            28800,
            false,
            false
        ));
        // A hot camera edit before the next tick must not render stale sunlight.
        let mut earth = a.earth_view;
        earth.roll = 90.0;
        let edited = a.update_views(earth, a.moon_view);
        assert!(a.update_clock(
            now + Duration::from_millis(20),
            unix + 0.02,
            28800,
            false,
            edited
        ));
        assert_eq!(a.sample_unix, unix + 0.02);
        // Cover occlusion, backwards wall-clock correction and a long suspend.
        assert!(!a.update_clock(now + Duration::from_secs(1), unix + 1.0, 28800, true, false));
        assert!(a.update_clock(
            now + Duration::from_secs(2),
            unix - 3600.0,
            28800,
            false,
            false
        ));
        assert_eq!(a.sample_unix, unix - 3600.0);
        assert!(!a.update_clock(
            now + Duration::from_secs(3),
            unix - 3599.0,
            28800,
            true,
            false
        ));
        assert!(a.update_clock(
            now + Duration::from_secs(7200),
            unix + 7200.0,
            28800,
            false,
            false
        ));
        assert_eq!(a.sample_unix, unix + 7200.0);
        assert!(!a.update_clock(
            now + Duration::from_secs(7200),
            unix + 7200.0,
            28800,
            false,
            false
        ));
    }

    #[test]
    fn layout_updates_preserve_image_lifecycle_and_ignore_stable_frames() {
        let mut a = AstronomyWallpaper::default();
        a.path = "loaded-earth.jpg".into();
        a.configured_path = "configured-earth.jpg".into();
        a.failed = true;
        let outputs = [(0, 0, 100, 100), (100, 0, 100, 100)];
        assert!(a.update_layout(true, "span", (200, 100), outputs.into_iter()));
        a.enabled = true;
        let ptr = a.draw_plan.as_ptr();
        assert!(!a.update_layout(true, "span", (200, 100), outputs.into_iter()));
        assert_eq!(ptr, a.draw_plan.as_ptr());
        assert!(a.update_layout(true, "per-output", (200, 100), outputs.into_iter()));
        assert_eq!(a.draw_plan.len(), 2);
        let old = a.draw_plan.clone();
        assert!(!a.update_layout(true, "per-output", (200, 100), outputs.into_iter().rev()));
        assert_eq!(a.draw_plan, old);
        assert!(a.update_layout(
            true,
            "per-output",
            (200, 100),
            [(0, 0, 200, 100)].into_iter()
        ));
        assert!(a.update_layout(
            true,
            "per-output",
            (200, 200),
            [(0, 0, 200, 100)].into_iter()
        ));
        assert_eq!(a.path, "loaded-earth.jpg");
        assert_eq!(a.configured_path, "configured-earth.jpg");
        assert!(a.failed);
        assert!(a.update_layout(false, "per-output", (200, 200), outputs.into_iter()));
        assert!(a.draw_plan.is_empty());
    }

    #[test]
    fn identities_follow_outputs_after_move_mirror_and_hotplug() {
        use CelestialBody::*;
        let mut a = AstronomyWallpaper::default();
        let overrides = std::collections::BTreeMap::from([
            ("DP-1".into(), "earth".into()),
            ("HDMI-1".into(), "moon".into()),
        ]);
        a.output_identities = vec![
            ("DP-1".into(), 100, 0, 100, 100),
            ("HDMI-1".into(), 0, 0, 100, 100),
        ];
        a.update_layout(
            true,
            "earth-moon",
            (200, 100),
            [(0, 0, 100, 100), (100, 0, 100, 100)].into_iter(),
        );
        assert!(a.update_bodies(&overrides, true));
        assert_eq!(a.bodies, [Moon, Earth]);
        let ptr = a.bodies.as_ptr();
        assert!(!a.update_bodies(&overrides, false));
        assert_eq!(ptr, a.bodies.as_ptr());
        a.output_identities = vec![("HDMI-1".into(), 0, 0, 100, 100)];
        a.update_layout(
            true,
            "earth-moon",
            (100, 100),
            [(0, 0, 100, 100)].into_iter(),
        );
        a.update_bodies(&overrides, true);
        assert_eq!(a.bodies, [Moon]);
        // Duplicate geometries (mirrors) retain one deterministic canvas.
        a.output_identities = vec![
            ("DP-1".into(), 0, 0, 100, 100),
            ("HDMI-1".into(), 0, 0, 100, 100),
        ];
        a.update_layout(
            true,
            "earth-moon",
            (100, 100),
            [(0, 0, 100, 100), (0, 0, 100, 100)].into_iter(),
        );
        a.update_bodies(&overrides, true);
        assert_eq!(a.bodies, [Earth]);
    }

    #[test]
    fn terminal_failure_releases_all_partial_materials_exactly_once() {
        let earth = glow::NativeTexture(std::num::NonZeroU32::new(1).unwrap());
        let moon = glow::NativeTexture(std::num::NonZeroU32::new(2).unwrap());
        let cloud = glow::NativeTexture(std::num::NonZeroU32::new(3).unwrap());
        let sky = glow::NativeTexture(std::num::NonZeroU32::new(4).unwrap());
        for (e, m, c, s) in [
            (Some(earth), None, None, Some(sky)),
            (None, Some(moon), None, None),
            (Some(earth), Some(moon), Some(cloud), Some(sky)),
        ] {
            let mut a = AstronomyWallpaper {
                enabled: true,
                texture: e,
                moon_texture: m,
                cloud_texture: c,
                sky_texture: s,
                ..Default::default()
            };
            assert_eq!(a.take_failed_material(), [e, m, c, s, None, None]);
            assert_eq!(a.take_failed_material(), [None; 6]);
            assert!(a.failed);
            assert_eq!(a.deadline(Instant::now()), None);
            assert!(
                a.pending.is_none()
                    && a.moon_pending.is_none()
                    && a.cloud_pending.is_none()
                    && a.sky_pending.is_none()
            );
        }
    }

    #[test]
    fn power_loss_or_query_error_requests_one_fallback_repaint() {
        let now = Instant::now();
        let mut a = AstronomyWallpaper::default();
        assert!(a.observe_power(now, Some(true)));
        assert!(!a.observe_power(now, Some(true)));
        assert!(a.observe_power(now, Some(false)));
        assert!(!a.observe_power(now, None));
        assert!(a.observe_power(now, Some(true)));
        assert!(a.observe_power(now, None));
        assert!(!a.powered);
        assert_eq!(a.last_power_query, Some(now));
    }

    #[test]
    fn disabled_and_failed_materials_do_not_schedule_wakes() {
        let now = Instant::now();
        let mut a = AstronomyWallpaper::default();
        assert_eq!(a.deadline(now), None);
        a.enabled = true;
        a.failed = true;
        assert_eq!(a.deadline(now), None);
    }
    #[test]
    fn paused_or_unknown_power_keeps_only_the_recovery_deadline() {
        let now = Instant::now();
        let mut a = AstronomyWallpaper {
            enabled: true,
            ..Default::default()
        };
        assert_eq!(a.deadline(now), Some(Duration::ZERO));
        a.last_power_query = Some(now);
        assert_eq!(a.deadline(now), Some(POWER_QUERY_INTERVAL));
        assert_eq!(a.deadline(now + POWER_QUERY_INTERVAL), Some(Duration::ZERO));
        a.last_power_query = Some(now + POWER_QUERY_INTERVAL);
        assert_eq!(
            a.deadline(now + POWER_QUERY_INTERVAL),
            Some(POWER_QUERY_INTERVAL)
        );
    }
    #[test]
    fn animation_deadline_is_capped_and_power_check_cannot_starve() {
        let now = Instant::now();
        let mut a = AstronomyWallpaper {
            enabled: true,
            last_power_query: Some(now),
            ..Default::default()
        };
        a.clock.set_paused(false);
        assert!(a.clock.tick(now));
        assert_eq!(
            a.deadline(now),
            Some(crate::backend::compositor_common::animated_wallpaper::FRAME_INTERVAL)
        );
        a.clock.set_paused(true);
        assert_eq!(a.deadline(now), Some(POWER_QUERY_INTERVAL));
        a.clock.set_paused(false);
        assert!(a.clock.tick(now + POWER_QUERY_INTERVAL));
        assert_eq!(a.deadline(now + POWER_QUERY_INTERVAL), Some(Duration::ZERO));
    }
}
