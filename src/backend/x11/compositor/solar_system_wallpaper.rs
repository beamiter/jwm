//! Fixed-cost solar system; optional bundled surface/cloud/sky maps load off-thread.
use super::*;
use crate::backend::compositor_common::{
    animated_wallpaper::{POWER_QUERY_INTERVAL, WallpaperClock},
    solar_system::{SolarCamera, SolarSpinClock, scene_phases, scene_spin_phases},
};
use glow::HasContext;
use std::time::{Duration, Instant};
const VERTEX: &str = "#version 330 core\nvoid main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);gl_Position=vec4(p*2.0-1.0,0.0,1.0);}";
struct SolarProgram {
    program: glow::Program,
    resolution: Option<glow::UniformLocation>,
    eye: Option<glow::UniformLocation>,
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
    enabled: bool,
    powered: bool,
    failed: bool,
    rendered: bool,
    program: Option<SolarProgram>,
    camera: SolarCamera,
    camera_dirty: bool,
    clock: WallpaperClock,
    last_power_query: Option<Instant>,
    epoch: Option<Instant>,
    sample_seconds: f64,
    spin_clock: SolarSpinClock,
    sample_spin_seconds: f64,
    earth_pending: Option<wallpaper::DeferredWallpaperLoad>,
    earth_texture: Option<glow::Texture>,
    cloud_pending: Option<wallpaper::DeferredWallpaperLoad>,
    cloud_texture: Option<glow::Texture>,
    sky_pending: Option<wallpaper::DeferredWallpaperLoad>,
    sky_texture: Option<glow::Texture>,
    frame_changed: bool,
}
impl SolarSystemWallpaper {
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
        Some(
            self.clock
                .deadline(now)
                .map_or(query, |frame| frame.min(query)),
        )
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
            self.camera.azimuth.to_bits(),
            self.camera.elevation.to_bits(),
            self.camera.distance.to_bits(),
        )
            .hash(&mut h);
        (h.finish(), self.enabled && self.powered && !self.failed)
    }
    fn take_materials(&mut self) -> [Option<glow::Texture>; 3] {
        self.earth_pending = None;
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
            .camera
            .delta(azimuth, elevation, zoom_steps)
        {
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
            a.enabled = enabled;
            a.failed = false;
            a.rendered = false;
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
                    a.sky_pending = Some(Self::load_wallpaper_async(
                        &path,
                        2048,
                        1024,
                        WallpaperMode::Stretch,
                    ));
                }
            }
        }
        if !enabled || a.failed {
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
        if a.clock.tick(now) {
            let epoch = *a.epoch.get_or_insert(now);
            a.sample_seconds = now.saturating_duration_since(epoch).as_secs_f64();
            changed = true;
        }
        a.frame_changed = changed | std::mem::take(&mut a.camera_dirty);
        a.sample_spin_for_frame();
        a.frame_changed
    }
    pub(super) fn draw_solar_system_wallpaper(&mut self) {
        let a = &mut self.solar_system_wallpaper;
        if !a.enabled || !a.powered || a.failed {
            return;
        }
        // Adopt only on a full material frame, never on unrelated partial client damage.
        if a.frame_changed {
            if let Some(load) = a.earth_pending.as_ref() {
                match SolarSystemWallpaper::poll_material(a.frame_changed, || load.try_recv()) {
                    Some(Ok(data)) => {
                        a.earth_pending = None;
                        if data.width == data.height.saturating_mul(2) {
                            unsafe {
                                self.gl.active_texture(glow::TEXTURE0);
                            }
                            a.earth_texture =
                                Self::upload_wallpaper_texture(&self.gl, &data).map(|v| v.0);
                            if let Some(texture) = a.earth_texture {
                                unsafe {
                                    self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                                    self.gl.tex_parameter_i32(
                                        glow::TEXTURE_2D,
                                        glow::TEXTURE_WRAP_S,
                                        glow::REPEAT as i32,
                                    );
                                    self.gl.bind_texture(glow::TEXTURE_2D, None);
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
            if let Some(load) = a.cloud_pending.as_ref() {
                match SolarSystemWallpaper::poll_material(a.frame_changed, || load.try_recv()) {
                    Some(Ok(data)) => {
                        a.cloud_pending = None;
                        if data.width == data.height.saturating_mul(2) {
                            unsafe {
                                self.gl.active_texture(glow::TEXTURE0);
                            }
                            a.cloud_texture =
                                Self::upload_wallpaper_texture(&self.gl, &data).map(|v| v.0);
                            if let Some(texture) = a.cloud_texture {
                                unsafe {
                                    self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                                    self.gl.tex_parameter_i32(
                                        glow::TEXTURE_2D,
                                        glow::TEXTURE_WRAP_S,
                                        glow::REPEAT as i32,
                                    );
                                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                                }
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
                        a.sky_pending = None;
                        if data.width == data.height.saturating_mul(2) {
                            unsafe {
                                self.gl.active_texture(glow::TEXTURE0);
                            }
                            a.sky_texture =
                                Self::upload_wallpaper_texture(&self.gl, &data).map(|v| v.0);
                            if let Some(texture) = a.sky_texture {
                                unsafe {
                                    self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                                    self.gl.tex_parameter_i32(
                                        glow::TEXTURE_2D,
                                        glow::TEXTURE_WRAP_S,
                                        glow::REPEAT as i32,
                                    );
                                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                                }
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
                Self::create_program(&self.gl, VERTEX, include_str!("shaders/solar_system.frag"))
            } {
                Ok(program) => {
                    a.program = Some(unsafe {
                        SolarProgram {
                            program,
                            resolution: self.gl.get_uniform_location(program, "resolution"),
                            eye: self.gl.get_uniform_location(program, "camera_eye"),
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
                    log::warn!("solar-system shader failed; retaining static wallpaper: {error}");
                    return;
                }
            }
        }
        let p = a.program.as_ref().unwrap();
        let eye = a.camera.eye();
        unsafe {
            let gl = &self.gl;
            gl.use_program(Some(p.program));
            gl.uniform_2_f32(
                p.resolution.as_ref(),
                self.screen_w as f32,
                self.screen_h as f32,
            );
            gl.uniform_3_f32(p.eye.as_ref(), eye[0], eye[1], eye[2]);
            let (phases, surface) = scene_phases(a.sample_seconds);
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
        a.rendered = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(a.camera.delta(0.3, 0.1, 1.0));
        assert_ne!(a.bar_metric_source().0, old.0);
        assert_eq!(a.sample_seconds, 125.0);
    }
}
