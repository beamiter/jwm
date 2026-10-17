//! Fixed-cost solar system; optional bundled surface/cloud/sky maps load off-thread.
use super::*;
use crate::backend::compositor_common::{
    animated_wallpaper::{
        CelestialArcball, CelestialRotation, POWER_QUERY_INTERVAL, WallpaperClock,
    },
    panorama::{PanoramaDrag, PanoramaView},
    solar_system::{
        BODY_RADII, SolarCamera, SolarProjection, SolarSpinClock, body_center, scene_phases,
        scene_spin_phases,
    },
};
use glow::HasContext;
use std::time::{Duration, Instant};
const VERTEX: &str = "#version 330 core\nvoid main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);gl_Position=vec4(p*2.0-1.0,0.0,1.0);}";
const INTERACTION_INTERVAL: Duration = Duration::from_nanos(16_666_667);

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
        let target = self
            .focus
            .and_then(|body| body_center(body, &phases))
            .unwrap_or([0.0; 3]);
        let default_lens = (PanoramaView::default().fov_degrees().to_radians() * 0.5).tan();
        let lens = (self.panorama.fov_degrees().to_radians() * 0.5).tan();
        self.camera.projection(
            target,
            self.orbit
                .map_or_else(|| self.camera.orbit_matrix(), CelestialRotation::matrix),
            self.panorama.matrix(),
            viewport,
            phases,
            default_lens / lens,
        )
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
}
#[derive(Default)]
pub(super) struct SolarSystemWallpaper {
    cache: procedural_wallpaper_cache::ProceduralFrameCache,
    enabled: bool,
    powered: bool,
    failed: bool,
    rendered: bool,
    program: Option<SolarProgram>,
    navigation: SolarNavigation,
    frame_navigation: SolarNavigation,
    overview: Option<SolarNavigation>,
    presented: Option<SolarPresented>,
    drag: Option<SolarDrag>,
    interaction_pending: bool,
    interaction_after: Option<Instant>,
    camera_dirty: bool,
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
    texture_max_edge: u32,
    frame_changed: bool,
}
impl SolarSystemWallpaper {
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
                .and_then(|body| body_center(body, &presented.projection.phases))
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
        if !navigation.camera.focus(body, phases) {
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
        let interaction = (self.powered && self.interaction_ready()).then(|| {
            self.interaction_after.map_or(Duration::ZERO, |deadline| {
                deadline.saturating_duration_since(now)
            })
        });
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
            self.frame_navigation.camera.azimuth.to_bits(),
            self.frame_navigation.camera.elevation.to_bits(),
            self.frame_navigation.camera.distance.to_bits(),
            self.frame_navigation.focus,
        )
            .hash(&mut h);
        self.frame_navigation
            .orbit()
            .matrix()
            .map(f32::to_bits)
            .hash(&mut h);
        self.frame_navigation
            .panorama
            .matrix()
            .map(f32::to_bits)
            .hash(&mut h);
        self.frame_navigation
            .panorama
            .fov_degrees()
            .to_bits()
            .hash(&mut h);
        (h.finish(), self.enabled && self.powered && !self.failed)
    }
    fn take_materials(&mut self) -> [Option<glow::Texture>; 3] {
        self.earth_pending = None;
        self.earth_detail_requested = false;
        self.cloud_pending = None;
        self.sky_pending = None;
        [
            self.earth_texture.take(),
            self.cloud_texture.take(),
            self.sky_texture.take(),
        ]
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
        for texture in self.take_materials().into_iter().flatten() {
            unsafe {
                gl.delete_texture(texture);
            }
        }
        self.rendered = false;
        self.presented = None;
        self.suspend_interaction();
    }
}
impl<C: CompositorConnection> Compositor<C> {
    pub(crate) fn solar_system_interactive(&self) -> bool {
        let a = &self.solar_system_wallpaper;
        crate::config::CONFIG.load().behavior().wallpaper_effect == "solar-system"
            && a.enabled
            && a.powered
            && !a.failed
            && a.rendered
            && a.presented.is_some_and(|presented| {
                presented.projection.viewport == [self.screen_w, self.screen_h]
            })
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
        self.solar_system_interactive() && self.solar_system_wallpaper.drag.is_some()
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
        self.solar_system_interactive()
            && self
                .solar_system_wallpaper
                .presented
                .is_some_and(|presented| presented.projection.ray_at(x, y).is_some())
            && self.solar_system_wallpaper.leave_closeup()
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
        if self
            .solar_system_wallpaper
            .navigation
            .camera
            .delta(azimuth, elevation, zoom_steps)
        {
            self.solar_system_wallpaper.navigation.orbit = None;
            self.solar_system_wallpaper.camera_dirty = true;
            self.needs_render = true;
        }
        true
    }
    pub(super) fn prepare_solar_system_wallpaper(&mut self, occluded: bool) -> bool {
        let now = Instant::now();
        let enabled = crate::config::CONFIG.load().behavior().wallpaper_effect == "solar-system";
        let a = &mut self.solar_system_wallpaper;
        let mut changed = a.enabled != enabled;
        if changed {
            a.cache.invalidate();
            a.enabled = enabled;
            a.failed = false;
            a.rendered = false;
            a.presented = None;
            a.suspend_interaction();
            a.last_power_query = None;
            a.clock = WallpaperClock::default();
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
        if std::mem::take(&mut a.camera_dirty) {
            a.frame_navigation = a.navigation;
            changed = true;
        }
        if a.powered && !occluded {
            changed |= a.advance_interaction(now);
        }
        a.frame_changed = changed;
        a.sample_spin_for_frame();
        if a.frame_changed {
            a.cache.invalidate();
        }
        a.frame_changed
    }
    pub(super) fn draw_solar_system_wallpaper(&mut self) {
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
            if a.frame_navigation.focus == Some(3)
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
                        }
                    });
                }
                Err(error) => {
                    for texture in a.take_materials().into_iter().flatten() {
                        unsafe {
                            self.gl.delete_texture(texture);
                        }
                    }
                    a.failed = true;
                    a.rendered = false;
                    a.presented = None;
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
        let clip = (0, 0, self.screen_w as i32, self.screen_h as i32);
        if cached && a.cache.valid() {
            unsafe {
                a.cache.present(&self.gl, [clip]);
            }
            a.rendered = true;
            return;
        }
        let cache_state = cached.then(|| unsafe { a.cache.begin(&self.gl) });
        let p = a.program.as_ref().unwrap();
        let (phases, surface) = scene_phases(a.sample_seconds);
        let projection = a
            .frame_navigation
            .projection([self.screen_w, self.screen_h], phases);
        unsafe {
            let gl = &self.gl;
            gl.use_program(Some(p.program));
            gl.uniform_2_f32(
                p.resolution.as_ref(),
                self.screen_w as f32,
                self.screen_h as f32,
            );
            gl.uniform_3_f32(
                p.eye.as_ref(),
                projection.eye[0],
                projection.eye[1],
                projection.eye[2],
            );
            gl.uniform_matrix_3_f32_slice(p.basis.as_ref(), false, &projection.basis);
            gl.uniform_1_i32(p.basis_enabled.as_ref(), 1);
            gl.uniform_1_f32(p.focal.as_ref(), projection.focal);
            gl.uniform_1_f32_slice(p.phases.as_ref(), &phases);
            gl.uniform_1_f32(p.surface.as_ref(), surface);
            gl.uniform_1_f32_slice(p.spins.as_ref(), &scene_spin_phases(a.sample_spin_seconds));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, a.earth_texture);
            gl.uniform_1_i32(p.earth_map.as_ref(), 0);
            gl.uniform_1_i32(
                p.earth_enabled.as_ref(),
                i32::from(a.earth_texture.is_some()),
            );
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, a.cloud_texture);
            gl.uniform_1_i32(p.cloud_map.as_ref(), 1);
            gl.uniform_1_i32(
                p.cloud_enabled.as_ref(),
                i32::from(a.cloud_texture.is_some()),
            );
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(glow::TEXTURE_2D, a.sky_texture);
            gl.uniform_1_i32(p.sky_map.as_ref(), 2);
            gl.uniform_1_i32(p.sky_enabled.as_ref(), i32::from(a.sky_texture.is_some()));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
        }
        drop(cache_state);
        if cached {
            a.cache.finish();
            unsafe {
                a.cache.present(&self.gl, [clip]);
            }
        }
        a.rendered = true;
        a.presented = Some(SolarPresented {
            navigation: a.frame_navigation,
            projection,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible_scene() -> SolarSystemWallpaper {
        let navigation = SolarNavigation::default();
        SolarSystemWallpaper {
            enabled: true,
            powered: true,
            rendered: true,
            presented: Some(SolarPresented {
                navigation,
                projection: navigation.projection([800, 600], scene_phases(0.0).0),
            }),
            ..Default::default()
        }
    }

    fn show_navigation(a: &mut SolarSystemWallpaper, now: Instant, seconds: f64) {
        a.advance_interaction(now);
        a.presented = Some(SolarPresented {
            navigation: a.frame_navigation,
            projection: a
                .frame_navigation
                .projection([800, 600], scene_phases(seconds).0),
        });
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
        a.last_power_query = Some(now);
        assert!(a.begin_drag([10.0, 10.0]));
        let original = a.frame_navigation;
        for x in 20..120 {
            assert!(a.drag_motion([f64::from(x), 50.0]));
        }
        assert_eq!(a.frame_navigation, original);
        assert_eq!(a.deadline(now), Some(Duration::ZERO));
        assert!(a.advance_interaction(now));
        assert_eq!(a.frame_navigation, a.navigation);
        let displayed = a.frame_navigation;
        assert!(a.drag_motion([300.0, 150.0]));
        assert!(!a.advance_interaction(now + Duration::from_millis(1)));
        assert_eq!(a.frame_navigation, displayed);
        assert_eq!(a.deadline(now), Some(INTERACTION_INTERVAL));
        assert!(a.advance_interaction(now + INTERACTION_INTERVAL));
        assert_eq!(a.frame_navigation, a.navigation);
        a.end_drag(false);
        assert!(!a.interaction_pending);
        assert_eq!(a.deadline(now), Some(POWER_QUERY_INTERVAL));
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
        a.last_power_query = Some(now);
        assert!(a.focus_body(3, &scene_phases(0.0).0));
        let pending = a.navigation;
        let displayed = a.frame_navigation;
        assert!(a.begin_drag([10.0, 10.0]));
        assert!(!a.advance_interaction(now));
        assert_eq!(a.frame_navigation, displayed);
        assert_eq!(a.navigation, pending);
        assert_eq!(a.deadline(now), Some(POWER_QUERY_INTERVAL));
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
        assert_eq!(a.take_materials(), [Some(earth), Some(cloud), Some(sky)]);
        assert_eq!(a.take_materials(), [None, None, None]);
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
        assert!(a.navigation.camera.delta(0.3, 0.1, 1.0));
        a.frame_navigation = a.navigation;
        assert_ne!(a.bar_metric_source().0, old.0);
        assert_eq!(a.sample_seconds, 125.0);
    }
}
