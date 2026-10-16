//! Fixed-cost solar-system wallpaper; optional bundled Earth map loads off-thread.
use super::*;
use crate::backend::compositor_common::{
    animated_wallpaper::{POWER_QUERY_INTERVAL, WallpaperClock},
    solar_system::{SolarCamera, scene_phases},
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
    earth_map: Option<glow::UniformLocation>,
    earth_enabled: Option<glow::UniformLocation>,
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
    earth_pending: Option<wallpaper::DeferredWallpaperLoad>,
    earth_texture: Option<glow::Texture>,
    frame_changed: bool,
}
impl SolarSystemWallpaper {
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
            self.camera.azimuth.to_bits(),
            self.camera.elevation.to_bits(),
            self.camera.distance.to_bits(),
        )
            .hash(&mut h);
        (h.finish(), self.enabled && self.powered && !self.failed)
    }
    pub(super) unsafe fn cleanup(&mut self, gl: &glow::Context) {
        if let Some(p) = self.program.take() {
            unsafe {
                gl.delete_program(p.program);
            }
        }
        self.earth_pending = None;
        if let Some(texture) = self.earth_texture.take() {
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
        }
        if !enabled || a.failed {
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
        a.clock.set_paused(!a.powered || occluded);
        if a.clock.tick(now) {
            let epoch = *a.epoch.get_or_insert(now);
            a.sample_seconds = now.saturating_duration_since(epoch).as_secs_f64();
            changed = true;
        }
        a.frame_changed = changed | std::mem::take(&mut a.camera_dirty);
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
                match load.try_recv() {
                    Ok(data) => {
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
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => a.earth_pending = None,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
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
                            earth_map: self.gl.get_uniform_location(program, "earth_map"),
                            earth_enabled: self.gl.get_uniform_location(program, "earth_enabled"),
                        }
                    });
                }
                Err(error) => {
                    a.earth_pending = None;
                    if let Some(texture) = a.earth_texture.take() {
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
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, a.earth_texture);
            gl.uniform_1_i32(p.earth_map.as_ref(), 0);
            gl.uniform_1_i32(
                p.earth_enabled.as_ref(),
                i32::from(a.earth_texture.is_some()),
            );
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
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
