//! Fixed-cost solar system; optional bundled surface/cloud/sky maps load off-thread.
use super::*;
use crate::backend::compositor_common::{
    animated_wallpaper::{
        AstronomyDraw, CelestialArcball, CelestialBody, CelestialRotation, POWER_QUERY_INTERVAL,
        WallpaperClock, resolve_planet_texture,
    },
    celestial_layout::{CelestialCanvas, CelestialOutput, resolve_celestial_canvases},
    panorama::{PanoramaDrag, PanoramaView},
    solar_ephemeris::{SolarDateClock, SolarEphemeris},
    solar_system::{
        BODY_RADII, SolarCamera, SolarProjection, SolarSpinClock, body_center, scene_phases,
        scene_spin_phases,
    },
};
use glow::HasContext;
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};
const VERTEX: &str = "#version 330 core\nvoid main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);gl_Position=vec4(p*2.0-1.0,0.0,1.0);}";
const INTERACTION_INTERVAL: Duration = Duration::from_nanos(16_666_667);
const PLANET_MAPS: [(&str, CelestialBody); 7] = [
    ("mercury", CelestialBody::Mercury),
    ("venus", CelestialBody::Venus),
    ("mars", CelestialBody::Mars),
    ("jupiter", CelestialBody::Jupiter),
    ("saturn", CelestialBody::Saturn),
    ("uranus", CelestialBody::Uranus),
    ("neptune", CelestialBody::Neptune),
];

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SolarNavigation {
    camera: SolarCamera,
    orbit: Option<CelestialRotation>,
    panorama: PanoramaView,
    focus: Option<u8>,
}

impl SolarNavigation {
    fn orbit(&self) -> CelestialRotation {
        self.orbit
            .unwrap_or_else(|| CelestialRotation::from_matrix(&self.camera.orbit_matrix()))
    }

    fn projection(&self, viewport: [u32; 2], phases: [f32; 8]) -> SolarProjection {
        self.projection_with_geometry(viewport, phases, None)
    }

    fn projection_with_geometry(
        &self,
        viewport: [u32; 2],
        phases: [f32; 8],
        geometry: Option<&SolarEphemeris>,
    ) -> SolarProjection {
        let centers = geometry.map(|g| g.centers);
        let target = self
            .focus
            .and_then(|body| {
                centers
                    .as_ref()
                    .and_then(|c| c.get(usize::from(body)).copied())
                    .or_else(|| body_center(body, &phases))
            })
            .unwrap_or([0.0; 3]);
        let default_lens = (PanoramaView::default().fov_degrees().to_radians() * 0.5).tan();
        let lens = (self.panorama.fov_degrees().to_radians() * 0.5).tan();
        let mut projection = self.camera.projection(
            target,
            self.orbit
                .map_or_else(|| self.camera.orbit_matrix(), CelestialRotation::matrix),
            self.panorama.matrix(),
            viewport,
            phases,
            default_lens / lens,
        );
        projection.centers = centers;
        projection.body_axes = geometry.map(|g| g.axes);
        projection.focus = self.focus;
        projection
    }
}

#[derive(Clone, Copy, Debug)]
struct SolarPresented {
    navigation: SolarNavigation,
    projection: SolarProjection,
}

#[derive(Clone, Copy, Debug)]
enum SolarGesture {
    Panorama(PanoramaDrag),
    Orbit {
        ball: CelestialArcball,
        center: [f64; 2],
        radius: f64,
        frame: CelestialRotation,
    },
}

#[derive(Clone, Copy, Debug)]
struct SolarDrag {
    gesture: SolarGesture,
    presented: SolarPresented,
    candidate: Option<u8>,
    origin: [f64; 2],
    moved: bool,
}

struct SolarProgram {
    program: glow::Program,
    resolution: Option<glow::UniformLocation>,
    canvas_origin: Option<glow::UniformLocation>,
    ephemeris_enabled: Option<glow::UniformLocation>,
    body_centers: Option<glow::UniformLocation>,
    body_axes: Option<glow::UniformLocation>,
    ellipse_centers: Option<glow::UniformLocation>,
    ellipse_u: Option<glow::UniformLocation>,
    ellipse_v: Option<glow::UniformLocation>,
    scientific: Option<glow::UniformLocation>,
    sky_scientific: Option<glow::UniformLocation>,
    focus_enabled: Option<glow::UniformLocation>,
    focused_body: Option<glow::UniformLocation>,
    eye: Option<glow::UniformLocation>,
    basis: Option<glow::UniformLocation>,
    basis_enabled: Option<glow::UniformLocation>,
    focal: Option<glow::UniformLocation>,
    phases: Option<glow::UniformLocation>,
    surface: Option<glow::UniformLocation>,
    spins: Option<glow::UniformLocation>,
    earth_map: Option<glow::UniformLocation>,
    earth_enabled: Option<glow::UniformLocation>,
    cloud_map: Option<glow::UniformLocation>,
    cloud_enabled: Option<glow::UniformLocation>,
    sky_map: Option<glow::UniformLocation>,
    sky_enabled: Option<glow::UniformLocation>,
    planet_maps: [Option<glow::UniformLocation>; 7],
    planet_enabled: [Option<glow::UniformLocation>; 7],
}
#[derive(Default)]
pub(super) struct SolarSystemWallpaper {
    cache: procedural_wallpaper_cache::ProceduralFrameCache,
    enabled: bool,
    powered: bool,
    failed: bool,
    rendered: bool,
    program: Option<SolarProgram>,
    views: BTreeMap<String, SolarView>,
    canvases: Vec<CelestialCanvas>,
    presentations: Vec<SolarCanvasPresentation>,
    output_identities: Vec<CelestialOutput>,
    layout_screen: (u32, u32),
    active_drag: Option<(String, AstronomyDraw)>,
    date_clock: SolarDateClock,
    ephemeris: Option<SolarEphemeris>,
    scientific: bool,
    orbit_mode: String,
    clock: WallpaperClock,
    last_power_query: Option<Instant>,
    epoch: Option<Instant>,
    sample_seconds: f64,
    spin_clock: SolarSpinClock,
    sample_spin_seconds: f64,
    earth_pending: Option<wallpaper::DeferredWallpaperLoad>,
    earth_texture: Option<glow::Texture>,
    earth_detail_requested: bool,
    cloud_pending: Option<wallpaper::DeferredWallpaperLoad>,
    cloud_texture: Option<glow::Texture>,
    sky_pending: Option<wallpaper::DeferredWallpaperLoad>,
    sky_texture: Option<glow::Texture>,
    sky_path: Option<String>,
    planet_requested: bool,
    planet_pending: [Option<wallpaper::DeferredWallpaperLoad>; 7],
    planet_textures: [Option<glow::Texture>; 7],
    texture_max_edge: u32,
    frame_changed: bool,
}
#[derive(Default)]
struct SolarView {
    navigation: SolarNavigation,
    frame_navigation: SolarNavigation,
    overview: Option<SolarNavigation>,
    presented: Option<SolarPresented>,
    drag: Option<SolarDrag>,
    interaction_pending: bool,
    interaction_after: Option<Instant>,
    camera_dirty: bool,
}

#[derive(Clone)]
struct SolarCanvasPresentation {
    group_key: String,
    draw: AstronomyDraw,
    presented: SolarPresented,
}

impl SolarView {
    fn interaction_ready(&self) -> bool {
        self.interaction_pending && self.drag.is_none_or(|drag| drag.moved)
    }

    fn request_interaction(&mut self) {
        self.interaction_pending = true;
    }

    fn advance_interaction(&mut self, now: Instant) -> bool {
        if !self.interaction_ready()
            || self
                .interaction_after
                .is_some_and(|deadline| now < deadline)
        {
            return false;
        }
        self.frame_navigation = self.navigation;
        self.interaction_pending = false;
        self.interaction_after = Some(now + INTERACTION_INTERVAL);
        true
    }

    fn begin_drag(&mut self, point: [f64; 2]) -> bool {
        let Some(presented) = self.presented else {
            return false;
        };
        if self.drag.is_some() || presented.projection.ray_at(point[0], point[1]).is_none() {
            return false;
        }
        let candidate = presented.projection.pick(point[0], point[1]);
        let viewport = presented.projection.viewport.map(f64::from);
        let gesture = if candidate.is_some() {
            let target = presented.navigation.focus;
            let center_world = target
                .and_then(|body| presented.projection.center(body))
                .unwrap_or([0.0; 3]);
            let center = presented
                .projection
                .project(center_world)
                .unwrap_or([viewport[0] * 0.5 - 0.5, viewport[1] * 0.5 - 0.5]);
            let radius = target
                .map_or(viewport[0].min(viewport[1]) * 0.42, |body| {
                    f64::from(
                        BODY_RADII[usize::from(body)] * presented.projection.focal
                            / presented.navigation.camera.distance,
                    ) * viewport[0].min(viewport[1])
                })
                .max(8.0);
            let Some(ball) = CelestialArcball::new(
                [
                    (point[0] - center[0]) / radius,
                    (center[1] - point[1]) / radius,
                ],
                CelestialRotation::default(),
            ) else {
                return false;
            };
            let mut outward = presented.projection.basis;
            for component in &mut outward[6..9] {
                *component = -*component;
            }
            SolarGesture::Orbit {
                ball,
                center,
                radius,
                frame: CelestialRotation::from_matrix(&outward),
            }
        } else {
            // PanoramaView uses +Y up and a lens measured against viewport
            // height. The solar lens retains its original min-dimension framing.
            let lens_height = 2.0
                * (f64::from(presented.navigation.panorama.fov_degrees()).to_radians() * 0.5).tan()
                * f64::from(presented.projection.focal)
                * viewport[0].min(viewport[1]);
            let Some(drag) = presented
                .navigation
                .panorama
                .begin_drag([point[0], -point[1]], [viewport[0], lens_height])
            else {
                return false;
            };
            SolarGesture::Panorama(drag)
        };
        // No navigation is changed until a captured MotionNotify arrives. A
        // refused native grab can therefore discard this gesture atomically.
        self.drag = Some(SolarDrag {
            gesture,
            presented,
            candidate,
            origin: point,
            moved: false,
        });
        true
    }

    fn drag_motion(&mut self, point: [f64; 2]) -> bool {
        let Some(mut drag) = self.drag else {
            return false;
        };
        if !drag.moved && point == drag.origin {
            return true;
        }
        let mut navigation = drag.presented.navigation;
        match drag.gesture {
            SolarGesture::Panorama(panorama) => {
                let Some(rotation) = panorama.orientation_at([point[0], -point[1]]) else {
                    return false;
                };
                navigation.panorama.set_orientation(rotation);
            }
            SolarGesture::Orbit {
                ball,
                center,
                radius,
                frame,
            } => {
                let Some(rotation) = ball.orientation_at([
                    (point[0] - center[0]) / radius,
                    (center[1] - point[1]) / radius,
                ]) else {
                    return false;
                };
                navigation.orbit = Some(
                    frame
                        .compose(rotation)
                        .compose(frame.inverse())
                        .compose(navigation.orbit()),
                );
            }
        }
        if drag.presented.navigation.focus.is_none() {
            // A new gesture deliberately takes over the overview actually
            // displayed, even if an earlier click was waiting to enter a body.
            self.overview = None;
        }
        if navigation != self.navigation {
            self.navigation = navigation;
            self.request_interaction();
        }
        drag.moved = true;
        self.drag = Some(drag);
        true
    }

    fn focus_body(&mut self, body: u8, phases: &[f32; 8]) -> bool {
        if usize::from(body) >= BODY_RADII.len() {
            return false;
        }
        let mut navigation = self.navigation;
        let focused =
            if let Some(centers) = self.presented.and_then(|shown| shown.projection.centers) {
                navigation.camera.focus_at(body, centers[usize::from(body)])
            } else {
                navigation.camera.focus(body, phases)
            };
        if !focused {
            return false;
        }
        if self.overview.is_none() {
            self.overview = Some(self.navigation);
        }
        navigation.focus = Some(body);
        navigation.orbit = None;
        navigation.panorama = PanoramaView::default();
        self.navigation = navigation;
        self.request_interaction();
        true
    }

    fn end_drag(&mut self, activate_click: bool) {
        if let Some(drag) = self.drag.take()
            && activate_click
            && let Some(body) = drag.candidate
            && self.navigation.focus != Some(body)
        {
            self.focus_body(body, &drag.presented.projection.phases);
        }
    }

    fn abort_drag(&mut self) {
        self.drag = None;
    }

    fn leave_closeup(&mut self) -> bool {
        if let Some(overview) = self.overview.take() {
            self.drag = None;
            self.navigation = overview;
            self.request_interaction();
            true
        } else {
            false
        }
    }

    fn zoom(&mut self, point: [f64; 2], steps: f64) -> bool {
        if !steps.is_finite()
            || self.drag.is_some()
            || self
                .presented
                .is_none_or(|presented| presented.projection.ray_at(point[0], point[1]).is_none())
        {
            return false;
        }
        let hit = self
            .presented
            .and_then(|presented| presented.projection.pick(point[0], point[1]));
        let changed = if let Some(body) = self.navigation.focus.filter(|body| Some(*body) == hit) {
            self.navigation.camera.zoom_body(body, steps)
        } else {
            self.navigation.panorama.zoom(steps)
        };
        if changed {
            self.request_interaction();
        }
        true
    }

    fn suspend_interaction(&mut self) {
        self.drag = None;
        self.frame_navigation = self.navigation;
        self.interaction_pending = false;
        self.interaction_after = None;
    }

    fn interaction_deadline(&self, now: Instant) -> Option<Duration> {
        self.interaction_ready().then(|| {
            self.interaction_after.map_or(Duration::ZERO, |deadline| {
                deadline.saturating_duration_since(now)
            })
        })
    }
}

impl SolarSystemWallpaper {
    fn resolve_canvases(
        screen: (u32, u32),
        outputs: &[CelestialOutput],
        behavior: &crate::config::BehaviorConfig,
    ) -> Vec<CelestialCanvas> {
        let mut canvases = match behavior.wallpaper_effect.as_str() {
            "solar-system" => {
                let mut legacy = behavior.clone();
                legacy.wallpaper_astronomy_scene = "solar-system".into();
                legacy.wallpaper_astronomy_layout = "span".into();
                resolve_celestial_canvases(screen, outputs, &legacy)
            }
            "astronomy" => resolve_celestial_canvases(screen, outputs, behavior),
            _ => Vec::new(),
        };
        canvases.retain(|canvas| canvas.scene == "solar-system");
        canvases
    }

    fn suspend_interaction(&mut self) {
        self.active_drag = None;
        for view in self.views.values_mut() {
            view.suspend_interaction();
        }
    }

    fn presented_at(&self, point: [f64; 2]) -> Option<&SolarCanvasPresentation> {
        if !point.iter().all(|v| v.is_finite()) {
            return None;
        }
        let gl_y = f64::from(self.layout_screen.1) - point[1] - 0.5;
        self.presentations.iter().find(|entry| {
            let (x, y, w, h) = entry.draw.clip;
            point[0] + 0.5 >= f64::from(x)
                && point[0] + 0.5 < f64::from(x) + f64::from(w)
                && gl_y >= f64::from(y)
                && gl_y < f64::from(y) + f64::from(h)
        })
    }

    fn local_point(&self, draw: &AstronomyDraw, point: [f64; 2]) -> [f64; 2] {
        [
            point[0] - f64::from(draw.origin[0]),
            point[1]
                - (f64::from(self.layout_screen.1)
                    - f64::from(draw.origin[1])
                    - f64::from(draw.size[1])),
        ]
    }

    fn begin_drag(&mut self, point: [f64; 2]) -> bool {
        if self.active_drag.is_some() {
            return false;
        }
        let Some(entry) = self.presented_at(point).cloned() else {
            return false;
        };
        let local = self.local_point(&entry.draw, point);
        let Some(view) = self.views.get_mut(&entry.group_key) else {
            return false;
        };
        view.presented = Some(entry.presented);
        if !view.begin_drag(local) {
            return false;
        }
        self.active_drag = Some((entry.group_key, entry.draw));
        true
    }

    fn drag_motion(&mut self, point: [f64; 2]) -> bool {
        let Some((key, draw)) = self.active_drag.clone() else {
            return false;
        };
        let local = self.local_point(&draw, point);
        self.views
            .get_mut(&key)
            .is_some_and(|view| view.drag_motion(local))
    }

    fn end_drag(&mut self, activate_click: bool) {
        if let Some((key, _)) = self.active_drag.take()
            && let Some(view) = self.views.get_mut(&key)
        {
            view.end_drag(activate_click);
        }
    }

    fn abort_drag(&mut self) {
        if let Some((key, _)) = self.active_drag.take()
            && let Some(view) = self.views.get_mut(&key)
        {
            view.abort_drag();
        }
    }

    fn zoom(&mut self, point: [f64; 2], steps: f64) -> bool {
        if self.active_drag.is_some() {
            return false;
        }
        let Some(entry) = self.presented_at(point).cloned() else {
            return false;
        };
        let local = self.local_point(&entry.draw, point);
        let Some(view) = self.views.get_mut(&entry.group_key) else {
            return false;
        };
        view.presented = Some(entry.presented);
        view.zoom(local, steps)
    }

    fn leave_closeup_at(&mut self, point: [f64; 2]) -> bool {
        if self.active_drag.is_some() {
            return false;
        }
        let Some(key) = self
            .presented_at(point)
            .map(|entry| entry.group_key.clone())
        else {
            return false;
        };
        let Some(view) = self.views.get_mut(&key) else {
            return false;
        };
        if !view.leave_closeup() {
            view.navigation = SolarNavigation::default();
            view.request_interaction();
        }
        true
    }

    fn sample_spin_for_frame(&mut self) {
        if self.frame_changed {
            self.sample_spin_seconds = self.spin_clock.seconds;
        }
    }
    pub(super) fn deadline(&self, now: Instant) -> Option<Duration> {
        if !self.enabled || self.failed {
            return None;
        }
        let query = self.last_power_query.map_or(Duration::ZERO, |last| {
            POWER_QUERY_INTERVAL.saturating_sub(now.saturating_duration_since(last))
        });
        let ordinary = self
            .clock
            .deadline(now)
            .map_or(query, |frame| frame.min(query));
        let interaction = self
            .powered
            .then(|| {
                self.views
                    .values()
                    .filter_map(|view| view.interaction_deadline(now))
                    .min()
            })
            .flatten();
        Some(interaction.map_or(ordinary, |frame| frame.min(ordinary)))
    }

    pub(super) fn due(&self) -> bool {
        self.deadline(Instant::now()) == Some(Duration::ZERO)
    }
    pub(super) fn bar_metric_source(&self) -> (u64, bool) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            self.enabled,
            self.powered,
            self.failed,
            self.earth_texture,
            self.cloud_texture,
            self.sky_texture,
            self.scientific,
        )
            .hash(&mut h);
        self.planet_textures.hash(&mut h);
        for (key, view) in &self.views {
            key.hash(&mut h);
            view.frame_navigation.camera.distance.to_bits().hash(&mut h);
            view.frame_navigation.focus.hash(&mut h);
            view.frame_navigation
                .orbit()
                .matrix()
                .map(f32::to_bits)
                .hash(&mut h);
            view.frame_navigation
                .panorama
                .matrix()
                .map(f32::to_bits)
                .hash(&mut h);
            view.frame_navigation
                .panorama
                .fov_degrees()
                .to_bits()
                .hash(&mut h);
        }
        (h.finish(), self.enabled && self.powered && !self.failed)
    }
    fn take_materials(&mut self) -> Vec<glow::Texture> {
        self.earth_pending = None;
        self.earth_detail_requested = false;
        self.cloud_pending = None;
        self.sky_pending = None;
        self.planet_requested = false;
        self.planet_pending = Default::default();
        [
            self.earth_texture.take(),
            self.cloud_texture.take(),
            self.sky_texture.take(),
        ]
        .into_iter()
        .chain(self.planet_textures.iter_mut().map(Option::take))
        .flatten()
        .collect()
    }
    // A partial repair must neither start a decode nor adopt a completed material.
    fn poll_material<T>(frame_changed: bool, receive: impl FnOnce() -> T) -> Option<T> {
        frame_changed.then(receive)
    }
    pub(super) unsafe fn cleanup(&mut self, gl: &glow::Context) {
        unsafe {
            self.cache.cleanup(gl);
        }
        if let Some(p) = self.program.take() {
            unsafe {
                gl.delete_program(p.program);
            }
        }
        for texture in self.take_materials() {
            unsafe {
                gl.delete_texture(texture);
            }
        }
        self.rendered = false;
        self.presentations.clear();
        self.suspend_interaction();
    }
}
impl<C: CompositorConnection> Compositor<C> {
    pub(crate) fn solar_system_interactive(&self) -> bool {
        let a = &self.solar_system_wallpaper;
        let cfg = crate::config::CONFIG.load();
        a.canvases
            == SolarSystemWallpaper::resolve_canvases(
                (self.screen_w, self.screen_h),
                &a.output_identities,
                cfg.behavior(),
            )
            && a.enabled
            && a.powered
            && !a.failed
            && a.rendered
            && !a.presentations.is_empty()
            && a.layout_screen == (self.screen_w, self.screen_h)
            && !self.overview_active
            && !self.overview_closing
            && !self.expose_active
            && self.expose_entries.is_empty()
            && self.genie_active.is_empty()
            && !self.transition_active()
    }
    pub(crate) fn solar_system_begin_drag(&mut self, x: f64, y: f64) -> bool {
        self.solar_system_interactive() && self.solar_system_wallpaper.begin_drag([x, y])
    }
    pub(crate) fn solar_system_abort_drag(&mut self) {
        self.solar_system_wallpaper.abort_drag();
    }
    pub(crate) fn solar_system_drag_active(&self) -> bool {
        self.solar_system_interactive() && self.solar_system_wallpaper.active_drag.is_some()
    }
    pub(crate) fn solar_system_drag_motion(&mut self, x: f64, y: f64) -> bool {
        if !self.solar_system_drag_active() || !self.solar_system_wallpaper.drag_motion([x, y]) {
            self.solar_system_wallpaper.end_drag(false);
            return false;
        }
        true
    }
    pub(crate) fn solar_system_end_drag(&mut self, activate_click: bool) {
        let activate_click = activate_click && self.solar_system_interactive();
        self.solar_system_wallpaper.end_drag(activate_click);
    }
    pub(crate) fn solar_system_zoom(&mut self, x: f64, y: f64, steps: f64) -> bool {
        self.solar_system_interactive() && self.solar_system_wallpaper.zoom([x, y], steps)
    }
    pub(crate) fn solar_system_leave_closeup(&mut self, x: f64, y: f64) -> bool {
        self.solar_system_interactive() && self.solar_system_wallpaper.leave_closeup_at([x, y])
    }

    pub(crate) fn solar_system_camera_delta(
        &mut self,
        azimuth: f32,
        elevation: f32,
        zoom_steps: f32,
    ) -> bool {
        if !self.solar_system_interactive() {
            return false;
        }
        if !azimuth.is_finite() || !elevation.is_finite() || !zoom_steps.is_finite() {
            return false;
        }
        // IPC camera deltas intentionally affect all solar views; direct mouse
        // gestures are routed only to the connector/group under the pointer.
        for view in self.solar_system_wallpaper.views.values_mut() {
            if view.navigation.camera.delta(azimuth, elevation, zoom_steps) {
                view.navigation.orbit = None;
                view.camera_dirty = true;
                self.needs_render = true;
            }
        }
        true
    }

    pub(crate) fn set_solar_output_identities(
        &mut self,
        outputs: &[crate::backend::api::OutputInfo],
    ) {
        let mut identities: Vec<_> = outputs
            .iter()
            .take(128)
            .filter(|o| o.width > 0 && o.height > 0)
            .map(|o| {
                (
                    if o.identity.connector.is_empty() {
                        o.name.clone()
                    } else {
                        o.identity.connector.clone()
                    },
                    o.x,
                    o.y,
                    o.width as u32,
                    o.height as u32,
                )
            })
            .collect();
        identities.sort_unstable();
        let a = &mut self.solar_system_wallpaper;
        if a.output_identities != identities {
            a.output_identities = identities;
            a.presentations.clear();
            a.suspend_interaction();
            a.cache.invalidate();
            self.needs_render = true;
        }
    }

    pub(super) fn prepare_solar_system_wallpaper(&mut self, occluded: bool) -> bool {
        let now = Instant::now();
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        let a = &mut self.solar_system_wallpaper;
        let screen = (self.screen_w, self.screen_h);
        let canvases =
            SolarSystemWallpaper::resolve_canvases(screen, &a.output_identities, behavior);
        let enabled = !canvases.is_empty();
        let toggled = a.enabled != enabled;
        let mut changed = toggled || a.canvases != canvases || a.layout_screen != screen;
        if a.canvases != canvases || a.layout_screen != screen {
            a.suspend_interaction();
            a.presentations.clear();
            a.views
                .retain(|key, _| canvases.iter().any(|canvas| &canvas.group_key == key));
            for canvas in &canvases {
                a.views.entry(canvas.group_key.clone()).or_default();
            }
            a.canvases = canvases;
            a.layout_screen = screen;
        }
        let scientific = behavior.wallpaper_astronomy_presentation == "scientific";
        changed |=
            a.scientific != scientific || a.orbit_mode != behavior.wallpaper_solar_orbit_mode;
        if a.orbit_mode != behavior.wallpaper_solar_orbit_mode
            && behavior.wallpaper_solar_orbit_mode == "ephemeris"
        {
            log::debug!(
                "solar-system: using date-derived JPL approximation ({})",
                crate::backend::compositor_common::kepler_ephemeris::EPHEMERIS_SOURCE_URL
            );
        }
        a.scientific = scientific;
        a.orbit_mode
            .clone_from(&behavior.wallpaper_solar_orbit_mode);
        if toggled {
            a.cache.invalidate();
            a.enabled = enabled;
            a.failed = false;
            a.rendered = false;
            a.presentations.clear();
            a.suspend_interaction();
            a.last_power_query = None;
            a.clock = WallpaperClock::default();
            a.planet_requested = false;
            a.planet_pending = Default::default();
            a.earth_pending = None;
            if enabled && a.earth_texture.is_none() {
                if let Some(path) =
                    crate::backend::compositor_common::animated_wallpaper::resolve_earth_texture("")
                {
                    a.earth_pending = Some(Self::load_wallpaper_async(
                        &path,
                        2048,
                        1024,
                        WallpaperMode::Stretch,
                    ));
                }
            }
            a.cloud_pending = None;
            if enabled && a.cloud_texture.is_none() {
                if let Some(path) =
                    crate::backend::compositor_common::animated_wallpaper::resolve_cloud_texture()
                {
                    a.cloud_pending = Some(Self::load_wallpaper_async(
                        &path,
                        2048,
                        1024,
                        WallpaperMode::Stretch,
                    ));
                }
            }
            a.sky_pending = None;
            if enabled && a.sky_texture.is_none() {
                if let Some(path) =
                    crate::backend::compositor_common::animated_wallpaper::resolve_sky_texture()
                {
                    a.sky_path = Some(path.clone());
                    a.sky_pending = Some(Self::load_astronomy_sky_async(path, a.texture_max_edge));
                }
            }
        }
        if !enabled || a.failed {
            a.suspend_interaction();
            if let Some(epoch) = a.epoch {
                a.spin_clock
                    .sample(now.saturating_duration_since(epoch).as_secs_f64(), 0.0);
            }
            return changed;
        }
        if a.last_power_query
            .is_none_or(|last| now.saturating_duration_since(last) >= POWER_QUERY_INTERVAL)
        {
            let powered = self.graphics.wallpaper_power_allows_animation() == Some(true);
            changed |= a.powered != powered;
            a.powered = powered;
            a.last_power_query = Some(now);
        }
        let epoch = *a.epoch.get_or_insert(now);
        let spin_rate = if a.powered && !occluded {
            crate::config::CONFIG
                .load()
                .behavior()
                .wallpaper_solar_spin_speed
        } else {
            0.0
        };
        changed |= a.spin_clock.sample(
            now.saturating_duration_since(epoch).as_secs_f64(),
            spin_rate,
        );
        a.clock.set_paused(!a.powered || occluded);
        if !a.powered || occluded {
            a.suspend_interaction();
        }
        if a.clock.tick(now) {
            let epoch = *a.epoch.get_or_insert(now);
            a.sample_seconds = now.saturating_duration_since(epoch).as_secs_f64();
            changed = true;
        }
        for view in a.views.values_mut() {
            if std::mem::take(&mut view.camera_dirty) {
                view.frame_navigation = view.navigation;
                changed = true;
            }
            if a.powered && !occluded {
                changed |= view.advance_interaction(now);
            }
        }
        let unix = crate::backend::compositor_common::animated_wallpaper::wall_clock_seconds(
            SystemTime::now(),
        );
        changed |= a.date_clock.sample(
            now.saturating_duration_since(epoch).as_secs_f64(),
            unix,
            behavior.wallpaper_solar_time_scale,
            behavior.wallpaper_solar_time_offset_days,
        );
        if changed {
            a.ephemeris = (a.orbit_mode == "ephemeris")
                .then(|| SolarEphemeris::at_unix(a.date_clock.unix_seconds()))
                .flatten();
        }
        a.frame_changed = changed;
        a.sample_spin_for_frame();
        if a.frame_changed {
            a.cache.invalidate();
        }
        a.frame_changed
    }
    pub(super) fn draw_solar_system_wallpaper(&mut self, repair: Option<(i32, i32, i32, i32)>) {
        let cache_ten_bit = procedural_wallpaper_cache::uses_ten_bit(
            self.hdr_enabled,
            self.hdr_output_10bit,
            self.needs_postprocess() && self.postprocess_fbo.is_some(),
        );
        let a = &mut self.solar_system_wallpaper;
        if !a.enabled || a.failed {
            unsafe {
                a.cache.cleanup(&self.gl);
            }
            return;
        }
        if !a.powered {
            return;
        }
        if a.texture_max_edge == 0 {
            a.texture_max_edge = unsafe { self.gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) }
                .clamp(2, 16384) as u32
                & !1;
        }
        // Adopt only on a full material frame, never on unrelated partial client damage.
        if a.frame_changed {
            if !a.planet_requested {
                a.planet_requested = true;
                for (index, (_, body)) in PLANET_MAPS.into_iter().enumerate() {
                    if a.planet_textures[index].is_none() {
                        a.planet_pending[index] = resolve_planet_texture(body).map(|path| {
                            Self::load_celestial_map_async(path, a.texture_max_edge.min(4096))
                        });
                    }
                }
            }
            for index in 0..PLANET_MAPS.len() {
                let Some(load) = a.planet_pending[index].as_ref() else {
                    continue;
                };
                match load.try_recv() {
                    Ok(data) => {
                        a.planet_pending[index] = None;
                        if data.width != data.height.saturating_mul(2)
                            || data.width > a.texture_max_edge
                        {
                            continue;
                        }
                        unsafe {
                            self.gl.active_texture(glow::TEXTURE0);
                        }
                        if let Some((texture, _, _)) =
                            Self::upload_wallpaper_texture(&self.gl, &data)
                        {
                            unsafe {
                                procedural_wallpaper_cache::prepare_equirectangular_texture(
                                    &self.gl, texture,
                                );
                                if let Some(previous) = a.planet_textures[index].replace(texture) {
                                    self.gl.delete_texture(previous);
                                }
                            }
                            a.cache.invalidate();
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        a.planet_pending[index] = None
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
            if let Some(load) = a.earth_pending.as_ref() {
                match SolarSystemWallpaper::poll_material(a.frame_changed, || load.try_recv()) {
                    Some(Ok(data)) => {
                        a.cache.invalidate();
                        a.earth_pending = None;
                        if data.width == data.height.saturating_mul(2) {
                            unsafe {
                                self.gl.active_texture(glow::TEXTURE0);
                            }
                            let uploaded =
                                Self::upload_wallpaper_texture(&self.gl, &data).map(|v| v.0);
                            if let Some(texture) = uploaded
                                && !unsafe {
                                    procedural_wallpaper_cache::prepare_equirectangular_texture(
                                        &self.gl, texture,
                                    )
                                }
                            {
                                log::warn!(
                                    "solar-system wallpaper: mipmaps unavailable; retaining base texture"
                                );
                            }
                            if let Some(texture) = uploaded {
                                if let Some(previous) = a.earth_texture.replace(texture) {
                                    unsafe {
                                        self.gl.delete_texture(previous);
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                        a.earth_pending = None
                    }
                    Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
                }
            }
            // Keep the light 2K overview. Only approaching Earth requests the
            // bundled 5400-pixel map; the previous complete mip pyramid stays
            // visible until the off-thread decode and upload both succeed.
            if a.views
                .values()
                .any(|view| view.frame_navigation.focus == Some(3))
                && !a.earth_detail_requested
                && a.earth_pending.is_none()
            {
                a.earth_detail_requested = true;
                if let Some(path) =
                    crate::backend::compositor_common::animated_wallpaper::resolve_earth_texture("")
                {
                    let width = 5400.min(a.texture_max_edge);
                    a.earth_pending = Some(Self::load_wallpaper_async(
                        &path,
                        width,
                        width / 2,
                        WallpaperMode::Stretch,
                    ));
                }
            }
            if let Some(load) = a.cloud_pending.as_ref() {
                match SolarSystemWallpaper::poll_material(a.frame_changed, || load.try_recv()) {
                    Some(Ok(data)) => {
                        a.cache.invalidate();
                        a.cloud_pending = None;
                        if data.width == data.height.saturating_mul(2) {
                            unsafe {
                                self.gl.active_texture(glow::TEXTURE0);
                            }
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
                                    "solar-system wallpaper: mipmaps unavailable; retaining base texture"
                                );
                            }
                        }
                    }
                    Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                        a.cloud_pending = None
                    }
                    Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
                }
            }
            if let Some(load) = a.sky_pending.as_ref() {
                match SolarSystemWallpaper::poll_material(a.frame_changed, || load.try_recv()) {
                    Some(Ok(data)) => {
                        a.cache.invalidate();
                        a.sky_pending = None;
                        if data.width > a.texture_max_edge {
                            if let Some(path) = a.sky_path.clone() {
                                a.sky_pending =
                                    Some(Self::load_astronomy_sky_async(path, a.texture_max_edge));
                            }
                        } else if data.width == data.height.saturating_mul(2) {
                            unsafe {
                                self.gl.active_texture(glow::TEXTURE0);
                            }
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
                                    "solar-system wallpaper: mipmaps unavailable; retaining base texture"
                                );
                            }
                        }
                    }
                    Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => a.sky_pending = None,
                    Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
                }
            }
        }
        if a.program.is_none() {
            match unsafe {
                Self::create_program(
                    &self.gl,
                    VERTEX,
                    &crate::backend::compositor_common::panorama::cosmic_shader_source(
                        include_str!("shaders/solar_system.frag"),
                    ),
                )
            } {
                Ok(program) => {
                    a.program = Some(unsafe {
                        SolarProgram {
                            program,
                            resolution: self.gl.get_uniform_location(program, "resolution"),
                            canvas_origin: self.gl.get_uniform_location(program, "canvas_origin"),
                            ephemeris_enabled: self
                                .gl
                                .get_uniform_location(program, "ephemeris_enabled"),
                            body_centers: self.gl.get_uniform_location(program, "body_centers[0]"),
                            body_axes: self.gl.get_uniform_location(program, "body_axes[0]"),
                            ellipse_centers: self
                                .gl
                                .get_uniform_location(program, "ellipse_centers[0]"),
                            ellipse_u: self.gl.get_uniform_location(program, "ellipse_u[0]"),
                            ellipse_v: self.gl.get_uniform_location(program, "ellipse_v[0]"),
                            scientific: self.gl.get_uniform_location(program, "scientific_mode"),
                            sky_scientific: self
                                .gl
                                .get_uniform_location(program, "cosmic_scientific"),
                            focus_enabled: self.gl.get_uniform_location(program, "focus_enabled"),
                            focused_body: self.gl.get_uniform_location(program, "focused_body"),
                            eye: self.gl.get_uniform_location(program, "camera_eye"),
                            basis: self.gl.get_uniform_location(program, "camera_basis"),
                            basis_enabled: self
                                .gl
                                .get_uniform_location(program, "camera_basis_enabled"),
                            focal: self.gl.get_uniform_location(program, "camera_focal"),
                            phases: self.gl.get_uniform_location(program, "orbit_phase[0]"),
                            surface: self.gl.get_uniform_location(program, "surface_phase"),
                            spins: self.gl.get_uniform_location(program, "spin_phase[0]"),
                            earth_map: self.gl.get_uniform_location(program, "earth_map"),
                            earth_enabled: self.gl.get_uniform_location(program, "earth_enabled"),
                            cloud_map: self.gl.get_uniform_location(program, "cloud_map"),
                            cloud_enabled: self.gl.get_uniform_location(program, "cloud_enabled"),
                            sky_map: self.gl.get_uniform_location(program, "sky_map"),
                            sky_enabled: self.gl.get_uniform_location(program, "sky_enabled"),
                            planet_maps: std::array::from_fn(|i| {
                                self.gl.get_uniform_location(
                                    program,
                                    &format!("{}_map", PLANET_MAPS[i].0),
                                )
                            }),
                            planet_enabled: std::array::from_fn(|i| {
                                self.gl.get_uniform_location(
                                    program,
                                    &format!("{}_enabled", PLANET_MAPS[i].0),
                                )
                            }),
                        }
                    });
                }
                Err(error) => {
                    for texture in a.take_materials() {
                        unsafe {
                            self.gl.delete_texture(texture);
                        }
                    }
                    a.failed = true;
                    a.rendered = false;
                    a.presentations.clear();
                    a.suspend_interaction();
                    log::warn!("solar-system shader failed; retaining static wallpaper: {error}");
                    return;
                }
            }
        }
        let cached = unsafe {
            a.cache
                .ensure(&self.gl, self.screen_w, self.screen_h, cache_ten_bit)
        };
        if cached && a.cache.valid() {
            unsafe {
                a.cache
                    .present(&self.gl, a.canvases.iter().map(|entry| entry.canvas.clip));
            }
            a.rendered = true;
            return;
        }
        let cache_state = cached.then(|| unsafe { a.cache.begin(&self.gl) });
        let active_repair = if cached { None } else { repair };
        let p = a.program.as_ref().unwrap();
        let (phases, surface) = scene_phases(a.sample_seconds);
        let geometry = a.ephemeris.as_ref();
        let mut presentations = Vec::with_capacity(a.canvases.len());
        unsafe {
            let gl = &self.gl;
            gl.use_program(Some(p.program));
            gl.uniform_1_i32(p.basis_enabled.as_ref(), 1);
            gl.uniform_1_f32_slice(p.phases.as_ref(), &phases);
            gl.uniform_1_f32(p.surface.as_ref(), surface);
            gl.uniform_1_f32_slice(p.spins.as_ref(), &scene_spin_phases(a.sample_spin_seconds));
            gl.uniform_1_i32(
                p.ephemeris_enabled.as_ref(),
                i32::from(a.ephemeris.is_some()),
            );
            gl.uniform_1_i32(p.scientific.as_ref(), i32::from(a.scientific));
            gl.uniform_1_i32(p.sky_scientific.as_ref(), i32::from(a.scientific));
            if let Some(geometry) = &a.ephemeris {
                gl.uniform_3_f32_slice(p.body_centers.as_ref(), geometry.centers.as_flattened());
                gl.uniform_3_f32_slice(p.body_axes.as_ref(), geometry.axes.as_flattened());
                gl.uniform_3_f32_slice(
                    p.ellipse_centers.as_ref(),
                    geometry.ellipse_centers.as_flattened(),
                );
                gl.uniform_3_f32_slice(p.ellipse_u.as_ref(), geometry.ellipse_u.as_flattened());
                gl.uniform_3_f32_slice(p.ellipse_v.as_ref(), geometry.ellipse_v.as_flattened());
            }
            for (unit, map, enabled, texture) in [
                (
                    0,
                    p.earth_map.as_ref(),
                    p.earth_enabled.as_ref(),
                    a.earth_texture,
                ),
                (
                    1,
                    p.cloud_map.as_ref(),
                    p.cloud_enabled.as_ref(),
                    a.cloud_texture,
                ),
                (2, p.sky_map.as_ref(), p.sky_enabled.as_ref(), a.sky_texture),
            ] {
                gl.active_texture(glow::TEXTURE0 + unit);
                gl.bind_texture(glow::TEXTURE_2D, texture);
                gl.uniform_1_i32(map, unit as i32);
                gl.uniform_1_i32(enabled, i32::from(texture.is_some()));
            }
            for (index, texture) in a.planet_textures.iter().copied().enumerate() {
                let unit = 3 + index as u32;
                gl.active_texture(glow::TEXTURE0 + unit);
                gl.bind_texture(glow::TEXTURE_2D, texture);
                gl.uniform_1_i32(p.planet_maps[index].as_ref(), unit as i32);
                gl.uniform_1_i32(
                    p.planet_enabled[index].as_ref(),
                    i32::from(texture.is_some()),
                );
            }
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.enable(glow::SCISSOR_TEST);
            for entry in &a.canvases {
                let Some(view) = a.views.get_mut(&entry.group_key) else {
                    continue;
                };
                let draw = entry.canvas.clone();
                let viewport = draw.size.map(|v| v as u32);
                let projection = if geometry.is_some() {
                    view.frame_navigation
                        .projection_with_geometry(viewport, phases, geometry)
                } else {
                    view.frame_navigation.projection(viewport, phases)
                };
                if let Some((x, y, w, h)) = draw.repair_clip(active_repair) {
                    gl.scissor(x, y, w, h);
                    gl.uniform_2_f32(p.canvas_origin.as_ref(), draw.origin[0], draw.origin[1]);
                    gl.uniform_2_f32(p.resolution.as_ref(), draw.size[0], draw.size[1]);
                    gl.uniform_3_f32(
                        p.eye.as_ref(),
                        projection.eye[0],
                        projection.eye[1],
                        projection.eye[2],
                    );
                    gl.uniform_matrix_3_f32_slice(p.basis.as_ref(), false, &projection.basis);
                    gl.uniform_1_f32(p.focal.as_ref(), projection.focal);
                    gl.uniform_1_i32(
                        p.focus_enabled.as_ref(),
                        i32::from(projection.focus.is_some()),
                    );
                    gl.uniform_1_i32(
                        p.focused_body.as_ref(),
                        i32::from(projection.focus.unwrap_or(0)),
                    );
                    gl.draw_arrays(glow::TRIANGLES, 0, 3);
                }
                let presented = SolarPresented {
                    navigation: view.frame_navigation,
                    projection,
                };
                view.presented = Some(presented);
                presentations.push(SolarCanvasPresentation {
                    group_key: entry.group_key.clone(),
                    draw,
                    presented,
                });
            }
            if let Some((x, y, w, h)) = repair {
                gl.scissor(x, y, w, h);
            } else {
                gl.disable(glow::SCISSOR_TEST);
            }
            gl.bind_vertex_array(None);
            for unit in (0..10).rev() {
                gl.active_texture(glow::TEXTURE0 + unit);
                gl.bind_texture(glow::TEXTURE_2D, None);
            }
            gl.use_program(None);
        }
        drop(cache_state);
        if cached {
            a.cache.finish();
            unsafe {
                a.cache
                    .present(&self.gl, a.canvases.iter().map(|entry| entry.canvas.clip));
            }
        }
        a.rendered = true;
        a.presentations = presentations;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible_scene() -> SolarView {
        let navigation = SolarNavigation::default();
        SolarView {
            presented: Some(SolarPresented {
                navigation,
                projection: navigation.projection([800, 600], scene_phases(0.0).0),
            }),
            ..Default::default()
        }
    }

    fn show_navigation(a: &mut SolarView, now: Instant, seconds: f64) {
        a.advance_interaction(now);
        a.presented = Some(SolarPresented {
            navigation: a.frame_navigation,
            projection: a
                .frame_navigation
                .projection([800, 600], scene_phases(seconds).0),
        });
    }

    fn multiple_outputs(layout: &str, left_scene: &str, group: &str) -> SolarSystemWallpaper {
        let behavior = crate::config::BehaviorConfig {
            wallpaper_effect: "astronomy".into(),
            wallpaper_astronomy_layout: layout.into(),
            wallpaper_astronomy_scene: "solar-system".into(),
            wallpaper_astronomy_outputs: [
                (
                    "DP-1".into(),
                    crate::config::AstronomyOutputConfig {
                        scene: left_scene.into(),
                        group: group.into(),
                    },
                ),
                (
                    "HDMI-1".into(),
                    crate::config::AstronomyOutputConfig {
                        scene: "solar-system".into(),
                        group: group.into(),
                    },
                ),
            ]
            .into(),
            ..crate::config::Config::default().behavior().clone()
        };
        let screen = (1050, 500);
        let outputs = vec![
            ("DP-1".into(), 0, 0, 400, 300),
            ("HDMI-1".into(), 450, 100, 600, 400),
        ];
        let canvases = SolarSystemWallpaper::resolve_canvases(screen, &outputs, &behavior);
        let mut a = SolarSystemWallpaper {
            layout_screen: screen,
            output_identities: outputs,
            canvases,
            sample_seconds: 125.0,
            ..Default::default()
        };
        for entry in &a.canvases {
            let view = a.views.entry(entry.group_key.clone()).or_default();
            let presented = SolarPresented {
                navigation: view.navigation,
                projection: view
                    .navigation
                    .projection(entry.canvas.size.map(|v| v as u32), scene_phases(0.0).0),
            };
            view.presented = Some(presented);
            a.presentations.push(SolarCanvasPresentation {
                group_key: entry.group_key.clone(),
                draw: entry.canvas.clone(),
                presented,
            });
        }
        a
    }

    #[test]
    fn independent_output_capture_and_zoom_do_not_move_another_solar_view() {
        let mut a = multiple_outputs("independent", "solar-system", "");
        assert_eq!(a.views.len(), 2);
        let left = a.presented_at([20.0, 20.0]).unwrap().group_key.clone();
        let right = a.presented_at([500.0, 120.0]).unwrap().group_key.clone();
        let original_right = a.views[&right].navigation;
        assert!(a.zoom([20.0, 20.0], 3.0));
        assert_ne!(a.views[&left].navigation.panorama, original_right.panorama);
        assert_eq!(a.views[&right].navigation, original_right);
        assert!(a.begin_drag([20.0, 20.0]));
        assert!(a.drag_motion([900.0, 400.0])); // Native capture follows outside the initiating screen.
        assert_eq!(a.active_drag.as_ref().unwrap().0, left);
        assert_eq!(a.views[&right].navigation, original_right);
        assert!(!a.zoom([500.0, 120.0], 1.0)); // No competing wheel gesture during capture.
        a.end_drag(false);
        assert!(a.active_drag.is_none());
        assert_eq!(a.sample_seconds, 125.0);
    }

    #[test]
    fn solar_routing_excludes_other_themes_and_real_desktop_gaps() {
        let mut a = multiple_outputs("independent", "earth", "");
        assert_eq!(a.views.len(), 1);
        for point in [
            [20.0, 20.0],
            [425.0, 200.0],
            [451.0, 80.0],
            [1050.0, 300.0],
            [f64::NAN, 200.0],
        ] {
            assert!(a.presented_at(point).is_none());
            assert!(!a.begin_drag(point));
            assert!(!a.zoom(point, 1.0));
            assert!(!a.leave_closeup_at(point));
        }
        assert!(a.begin_drag([500.0, 120.0]));
        a.abort_drag();
        assert!(a.active_drag.is_none());
    }

    #[test]
    fn mirrored_screens_share_navigation_but_pick_their_own_presented_lens() {
        let mut a = multiple_outputs("mirror", "solar-system", "");
        assert_eq!(a.views.len(), 1);
        let entry = a.presented_at([20.0, 20.0]).unwrap().clone();
        let center = entry.presented.projection.project([0.0; 3]).unwrap();
        let point = [center[0], center[1]];
        assert!(a.begin_drag(point));
        let drag = a.views[&entry.group_key].drag.unwrap();
        assert_eq!(drag.presented.projection.viewport, [400, 300]);
        assert_eq!(drag.candidate, Some(0));
        a.end_drag(true);
        assert_eq!(a.views[&entry.group_key].navigation.focus, Some(0));
        assert!(a.leave_closeup_at([700.0, 300.0]));
        assert_eq!(a.views[&entry.group_key].navigation.focus, None);
        assert_eq!(a.sample_seconds, 125.0);
    }

    #[test]
    fn named_solar_group_spans_its_screens_and_keeps_dead_space_unpickable() {
        let mut a = multiple_outputs("independent", "solar-system", "observatory");
        assert_eq!(a.views.len(), 1);
        let left = a.presented_at([20.0, 20.0]).unwrap();
        let right = a.presented_at([500.0, 120.0]).unwrap();
        assert_eq!(left.group_key, right.group_key);
        assert_eq!(left.draw.origin, right.draw.origin);
        assert_eq!(left.draw.size, [1050.0, 500.0]);
        assert!(!a.begin_drag([425.0, 200.0]));
        assert!(a.zoom([500.0, 120.0], 2.0));
        let changed = a.views.values().next().unwrap().navigation;
        assert_ne!(changed.panorama, PanoramaView::default());
        assert!(a.leave_closeup_at([20.0, 20.0]));
        assert_eq!(
            a.views.values().next().unwrap().navigation,
            SolarNavigation::default()
        );
    }

    #[test]
    fn dated_closeup_keeps_the_selected_body_visible_and_tracks_its_date() {
        let now = SolarEphemeris::at_unix(1_792_195_200.0).unwrap();
        let later = SolarEphemeris::at_unix(1_795_219_200.0).unwrap();
        let mut a = visible_scene();
        a.presented.as_mut().unwrap().projection.centers = Some(now.centers);
        assert!(a.focus_body(3, &scene_phases(0.0).0));
        let viewed =
            a.navigation
                .projection_with_geometry([800, 600], scene_phases(0.0).0, Some(&now));
        assert_eq!(viewed.pick(399.5, 299.5), Some(3));
        assert_eq!(viewed.focus, Some(3));
        let next =
            a.navigation
                .projection_with_geometry([800, 600], scene_phases(20.0).0, Some(&later));
        assert_eq!(next.pick(399.5, 299.5), Some(3));
        assert_ne!(viewed.eye, next.eye);
        assert_eq!(viewed.basis, next.basis);
        // A foreground enlarged neighbour must not steal the selected closeup.
        let mut obstructed = viewed;
        obstructed.centers.as_mut().unwrap()[2] =
            std::array::from_fn(|i| (viewed.eye[i] + now.centers[3][i]) * 0.5);
        assert_eq!(obstructed.pick(399.5, 299.5), Some(3));
        obstructed.focus = None;
        assert_eq!(obstructed.pick(399.5, 299.5), Some(2));
    }

    #[test]
    fn planet_click_enters_its_closeup_and_back_restores_the_overview() {
        let mut a = visible_scene();
        let initial = a.navigation;
        let displayed = a.presented.unwrap().projection;
        let earth = displayed
            .project(body_center(3, &displayed.phases).unwrap())
            .unwrap();
        assert!(a.begin_drag(earth));
        assert_eq!(a.drag.unwrap().candidate, Some(3));
        assert_eq!(a.navigation, initial);
        a.end_drag(true);
        assert_eq!(a.navigation.focus, Some(3));
        assert!(a.navigation.camera.distance < initial.camera.distance);
        assert_eq!(a.overview, Some(initial));
        assert!(a.interaction_pending);
        let now = Instant::now();
        show_navigation(&mut a, now, 20.0);
        let projected = a.presented.unwrap().projection;
        assert_eq!(projected.pick(399.5, 299.5), Some(3));
        assert!(a.focus_body(6, &projected.phases));
        assert_eq!(a.overview, Some(initial));
        assert!(a.leave_closeup());
        assert_eq!(a.navigation, initial);
        assert!(!a.leave_closeup());
    }

    #[test]
    fn cancel_or_drag_release_never_activates_a_planet_candidate() {
        let mut a = visible_scene();
        assert!(a.begin_drag([399.5, 299.5]));
        assert_eq!(a.drag.unwrap().candidate, Some(0));
        assert!(a.drag_motion([470.0, 230.0]));
        let dragged = a.navigation;
        assert!(dragged.orbit.is_some());
        a.end_drag(false);
        assert_eq!(a.navigation, dragged);
        assert_eq!(a.navigation.focus, None);
        assert!(a.overview.is_none());
        assert!(a.drag.is_none());
    }

    #[test]
    fn sky_gesture_rotates_all_rays_retraces_and_ignores_coalescing() {
        let mut a = visible_scene();
        assert!(a.begin_drag([10.0, 10.0]));
        assert!(matches!(a.drag.unwrap().gesture, SolarGesture::Panorama(_)));
        let initial = a.navigation;
        assert!(a.drag_motion([300.0, 170.0]));
        let direct = a.navigation;
        assert_ne!(direct.panorama.matrix(), initial.panorama.matrix());
        assert_eq!(direct.camera, initial.camera);
        assert_eq!(direct.orbit, initial.orbit);
        let old = initial.projection([800, 600], scene_phases(0.0).0);
        let new = direct.projection([800, 600], scene_phases(0.0).0);
        assert_eq!(old.eye, new.eye);
        assert_ne!(old.basis, new.basis);
        for point in [
            [-500.0, 900.0],
            [3000.0, -1400.0],
            [100.0, 50.0],
            [300.0, 170.0],
        ] {
            assert!(a.drag_motion(point));
        }
        assert_eq!(a.navigation, direct);
        assert!(a.drag_motion([10.0, 10.0]));
        assert_eq!(a.navigation, initial);
        a.end_drag(true);
        assert_eq!(a.navigation.focus, None);
    }

    #[test]
    fn focus_orbit_passes_poles_while_its_target_tracks_the_decorative_clock() {
        let now = Instant::now();
        let mut a = visible_scene();
        assert!(a.focus_body(3, &scene_phases(0.0).0));
        show_navigation(&mut a, now, 0.0);
        assert!(a.begin_drag([399.5, 299.5]));
        let distance = a.navigation.camera.distance;
        for point in [
            [399.5, -300.0],
            [-700.0, 300.0],
            [399.5, 1200.0],
            [1800.0, -1200.0],
        ] {
            assert!(a.drag_motion(point));
            let projection = a.navigation.projection([800, 600], scene_phases(30.0).0);
            assert_eq!(projection.pick(399.5, 299.5), Some(3));
            assert!(projection.basis.iter().all(|v| v.is_finite()));
            assert_eq!(a.navigation.camera.distance, distance);
        }
        let before = a.navigation.projection([800, 600], scene_phases(30.0).0);
        let after = a.navigation.projection([800, 600], scene_phases(31.0).0);
        assert_ne!(before.eye, after.eye);
        assert_eq!(before.basis, after.basis);
        a.end_drag(false);
        assert_eq!(a.navigation.focus, Some(3));
    }

    #[test]
    fn pointer_frames_are_coalesced_without_mutating_partial_repair_navigation() {
        let now = Instant::now();
        let mut a = visible_scene();
        assert!(a.begin_drag([10.0, 10.0]));
        let original = a.frame_navigation;
        for x in 20..120 {
            assert!(a.drag_motion([f64::from(x), 50.0]));
        }
        assert_eq!(a.frame_navigation, original);
        assert_eq!(a.interaction_deadline(now), Some(Duration::ZERO));
        assert!(a.advance_interaction(now));
        assert_eq!(a.frame_navigation, a.navigation);
        let displayed = a.frame_navigation;
        assert!(a.drag_motion([300.0, 150.0]));
        assert!(!a.advance_interaction(now + Duration::from_millis(1)));
        assert_eq!(a.frame_navigation, displayed);
        assert_eq!(a.interaction_deadline(now), Some(INTERACTION_INTERVAL));
        assert!(a.advance_interaction(now + INTERACTION_INTERVAL));
        assert_eq!(a.frame_navigation, a.navigation);
        a.end_drag(false);
        assert!(!a.interaction_pending);
        assert_eq!(a.interaction_deadline(now), None);
    }

    #[test]
    fn rejected_grab_and_stationary_release_preserve_an_earlier_pending_pose() {
        let mut a = visible_scene();
        assert!(a.begin_drag([10.0, 10.0]));
        assert!(a.drag_motion([110.0, 90.0]));
        a.end_drag(false);
        let pending = a.navigation;
        let presented = a.presented.unwrap();
        assert!(a.begin_drag([10.0, 10.0]));
        a.abort_drag();
        assert_eq!(a.navigation, pending);
        assert!(a.interaction_pending);
        assert_eq!(a.presented.unwrap().projection, presented.projection);
        assert!(a.begin_drag([10.0, 10.0]));
        assert!(a.drag_motion([10.0, 10.0]));
        a.end_drag(true);
        assert_eq!(a.navigation, pending);
        assert_eq!(a.navigation.focus, None);
    }

    #[test]
    fn pending_focus_never_appears_behind_a_stationary_older_view_grab() {
        let now = Instant::now();
        let mut a = visible_scene();
        assert!(a.focus_body(3, &scene_phases(0.0).0));
        let pending = a.navigation;
        let displayed = a.frame_navigation;
        assert!(a.begin_drag([10.0, 10.0]));
        assert!(!a.advance_interaction(now));
        assert_eq!(a.frame_navigation, displayed);
        assert_eq!(a.navigation, pending);
        assert_eq!(a.interaction_deadline(now), None);
        a.abort_drag();
        assert_eq!(a.navigation, pending);
        assert!(a.advance_interaction(now));

        // A deliberate movement takes over what was actually visible and
        // discards the unpresented focus plus its stale return destination.
        let mut a = visible_scene();
        assert!(a.focus_body(3, &scene_phases(0.0).0));
        assert!(a.begin_drag([10.0, 10.0]));
        assert!(a.drag_motion([100.0, 80.0]));
        assert_eq!(a.navigation.focus, None);
        assert!(a.overview.is_none());
        assert!(a.advance_interaction(now));
        assert_eq!(a.frame_navigation, a.navigation);
        assert!(!a.leave_closeup());
    }

    #[test]
    fn zoom_targets_the_active_closeup_and_suspension_retires_pointer_wakes() {
        let mut a = visible_scene();
        let original = a.navigation;
        assert!(a.zoom([10.0, 10.0], 4.0));
        assert!(a.navigation.panorama.fov_degrees() < original.panorama.fov_degrees());
        assert_eq!(a.navigation.camera, original.camera);
        assert!(a.focus_body(5, &scene_phases(0.0).0));
        show_navigation(&mut a, Instant::now(), 0.0);
        let focused = a.navigation;
        assert!(a.zoom([399.5, 299.5], 4.0));
        assert!(a.navigation.camera.distance < focused.camera.distance);
        assert_eq!(a.navigation.panorama, focused.panorama);
        let distance = a.navigation.camera.distance;
        assert!(a.zoom([0.0, 0.0], 3.0));
        assert_eq!(a.navigation.camera.distance, distance);
        assert!(a.navigation.panorama.fov_degrees() < focused.panorama.fov_degrees());
        let valid = a.navigation;
        assert!(!a.zoom([10.0, 10.0], f64::NAN));
        assert_eq!(a.navigation, valid);
        a.suspend_interaction();
        assert!(!a.interaction_pending);
        assert!(a.interaction_after.is_none());
        assert!(a.drag.is_none());
        assert_eq!(a.frame_navigation, a.navigation);
    }
    #[test]
    fn optional_material_polling_is_frozen_during_partial_damage() {
        let mut polls = 0;
        assert_eq!(
            SolarSystemWallpaper::poll_material(false, || {
                polls += 1;
                42
            }),
            None
        );
        assert_eq!(polls, 0);
        assert_eq!(
            SolarSystemWallpaper::poll_material(true, || {
                polls += 1;
                42
            }),
            Some(42)
        );
        assert_eq!(polls, 1);
    }
    #[test]
    fn optional_handles_invalidate_bar_identity_and_release_once() {
        let mut a = SolarSystemWallpaper::default();
        let earth = glow::NativeTexture(std::num::NonZeroU32::new(1).unwrap());
        let cloud = glow::NativeTexture(std::num::NonZeroU32::new(2).unwrap());
        let sky = glow::NativeTexture(std::num::NonZeroU32::new(3).unwrap());
        a.earth_texture = Some(earth);
        let old = a.bar_metric_source().0;
        a.cloud_texture = Some(cloud);
        assert_ne!(a.bar_metric_source().0, old);
        let old = a.bar_metric_source().0;
        a.sky_texture = Some(sky);
        assert_ne!(a.bar_metric_source().0, old);
        a.cloud_pending = Some(wallpaper::DeferredWallpaperLoad::new(|_| None));
        a.sky_pending = Some(wallpaper::DeferredWallpaperLoad::new(|_| None));
        assert_eq!(a.take_materials(), vec![earth, cloud, sky]);
        assert!(a.take_materials().is_empty());
        assert!(a.earth_pending.is_none() && a.cloud_pending.is_none() && a.sky_pending.is_none());
    }
    #[test]
    fn partial_damage_reuses_frozen_spin_uniforms() {
        let mut a = SolarSystemWallpaper::default();
        a.spin_clock.sample(0.0, 718.035);
        a.spin_clock.sample(1.0, 718.035);
        a.frame_changed = true;
        a.sample_spin_for_frame();
        let held = scene_spin_phases(a.sample_spin_seconds);
        a.spin_clock.sample(1.01, 718.035);
        a.frame_changed = false;
        a.sample_spin_for_frame();
        assert_eq!(scene_spin_phases(a.sample_spin_seconds), held);
        a.frame_changed = true;
        a.sample_spin_for_frame();
        assert_ne!(scene_spin_phases(a.sample_spin_seconds), held);
    }
    #[test]
    fn disabled_and_failed_states_stop_wakes() {
        let mut a = SolarSystemWallpaper::default();
        assert_eq!(a.deadline(Instant::now()), None);
        a.enabled = true;
        assert_eq!(a.deadline(Instant::now()), Some(Duration::ZERO));
        a.failed = true;
        assert_eq!(a.deadline(Instant::now()), None);
    }
    #[test]
    fn sleeping_outputs_only_poll_power_and_active_frames_are_capped() {
        let now = Instant::now();
        let mut a = SolarSystemWallpaper {
            enabled: true,
            last_power_query: Some(now),
            ..Default::default()
        };
        assert_eq!(a.deadline(now), Some(POWER_QUERY_INTERVAL));
        a.clock.set_paused(false);
        assert!(a.clock.tick(now));
        assert_eq!(
            a.deadline(now),
            Some(crate::backend::compositor_common::animated_wallpaper::FRAME_INTERVAL)
        );
        assert!(!a.clock.tick(now + Duration::from_millis(1)));
        a.clock.set_paused(true);
        assert_eq!(a.deadline(now), Some(POWER_QUERY_INTERVAL));
    }
    #[test]
    fn camera_identity_changes_without_resetting_animation_time() {
        let mut a = SolarSystemWallpaper {
            enabled: true,
            powered: true,
            sample_seconds: 125.0,
            ..Default::default()
        };
        let old = a.bar_metric_source();
        assert!(old.1);
        let view = a.views.entry("test".into()).or_default();
        assert!(view.navigation.camera.delta(0.3, 0.1, 1.0));
        view.frame_navigation = view.navigation;
        assert_ne!(a.bar_metric_source().0, old.0);
        assert_eq!(a.sample_seconds, 125.0);
    }
}
