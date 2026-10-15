//! Astronomy is a wallpaper material, rendered below every client window.
use super::*;
use crate::backend::compositor_common::animated_wallpaper::{
    POWER_QUERY_INTERVAL, WallpaperClock, solar_direction, viewing_longitude,
};
use glow::HasContext;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERTEX: &str = "#version 330 core\nvoid main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);gl_Position=vec4(p*2.0-1.0,0.0,1.0);}";
const FRAGMENT: &str = include_str!("shaders/animated_wallpaper.frag");

#[derive(Default)]
pub(super) struct AstronomyWallpaper {
    enabled: bool,
    path: String,
    pending: Option<wallpaper::DeferredWallpaperLoad>,
    texture: Option<glow::Texture>,
    retired: Vec<glow::Texture>,
    program: Option<glow::Program>,
    failed: bool,
    powered: bool,
    last_power_query: Option<Instant>,
    clock: WallpaperClock,
    sample_unix: f64,
    sample_offset: i32,
    frame_changed: bool,
}

impl AstronomyWallpaper {
    fn observe_power(&mut self, now: Instant, reply: Option<bool>) -> bool {
        let powered = reply == Some(true);
        let changed = self.powered != powered;
        self.powered = powered;
        self.last_power_query = Some(now);
        changed
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
    pub(super) unsafe fn cleanup(&mut self, gl: &glow::Context) {
        unsafe {
            for tex in self.retired.drain(..).chain(self.texture.take()) {
                gl.delete_texture(tex);
            }
            if let Some(program) = self.program.take() {
                gl.delete_program(program);
            }
        }
    }
}

impl<C: CompositorConnection> Compositor<C> {
    /// CPU/X11-only maintenance, deliberately before fullscreen/unchanged gates.
    pub(super) fn prepare_astronomy_wallpaper(&mut self, occluded: bool) -> bool {
        let now = Instant::now();
        let cfg = crate::config::CONFIG.load();
        let enabled = cfg.behavior().wallpaper_effect == "astronomy";
        let path = &cfg.behavior().wallpaper_earth_texture;
        let a = &mut self.astronomy_wallpaper;
        let changed = a.enabled != enabled || a.path != *path;
        if changed {
            a.enabled = enabled;
            a.path = path.clone();
            a.failed = false;
            a.pending = None;
            a.retired.extend(a.texture.take());
            a.last_power_query = None;
            a.clock = WallpaperClock::default();
            if enabled && !path.is_empty() {
                a.pending = Some(Self::load_wallpaper_async(
                    path,
                    4096,
                    2048,
                    WallpaperMode::Stretch,
                ));
            } else if enabled {
                a.failed = true;
                log::warn!(
                    "astronomy wallpaper: no Earth texture configured; keeping static wallpaper"
                );
            }
        }
        if !a.enabled || a.failed {
            return changed;
        }
        let power_changed = if a
            .last_power_query
            .is_none_or(|last| now.saturating_duration_since(last) >= POWER_QUERY_INTERVAL)
        {
            a.observe_power(now, self.graphics.wallpaper_power_allows_animation())
        } else {
            false
        };

        a.clock.set_paused(!a.powered || occluded);
        let tick = a.clock.tick(now);
        if tick {
            a.sample_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            a.sample_offset = chrono::Local::now().offset().local_minus_utc();
        }
        a.frame_changed = changed || power_changed || tick;
        a.frame_changed
    }

    /// Called with a current context, after the normal static wallpaper pass.
    pub(super) fn draw_astronomy_wallpaper(&mut self) {
        let a = &mut self.astronomy_wallpaper;
        unsafe {
            for tex in a.retired.drain(..) {
                self.gl.delete_texture(tex);
            }
        }
        if !a.enabled || a.failed || !a.powered {
            return;
        }
        // First adoption changes the entire material, so wait for a frame
        // whose prepare step already requested full repair/blur invalidation.
        // Unrelated client damage between animation ticks must stay static.
        if a.pending.is_some() && !a.frame_changed {
            return;
        }
        if let Some(load) = a.pending.as_ref() {
            match load.try_recv() {
                Ok(data) => {
                    a.pending = None;
                    if data.width != data.height.saturating_mul(2) {
                        log::warn!(
                            "astronomy wallpaper: Earth texture must be equirectangular 2:1; keeping static wallpaper"
                        );
                        a.failed = true;
                        return;
                    }
                    a.texture = Self::upload_wallpaper_texture(&self.gl, &data).map(|v| v.0);
                    if a.texture.is_none() {
                        a.failed = true;
                        return;
                    }
                    unsafe {
                        self.gl.bind_texture(glow::TEXTURE_2D, a.texture);
                        self.gl.tex_parameter_i32(
                            glow::TEXTURE_2D,
                            glow::TEXTURE_WRAP_S,
                            glow::REPEAT as i32,
                        );
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    a.pending = None;
                    a.failed = true;
                    log::warn!("astronomy wallpaper: Earth image failed; keeping static wallpaper");
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
            }
        }
        let Some(texture) = a.texture else {
            return;
        };
        if a.program.is_none() {
            match unsafe { Self::create_program(&self.gl, VERTEX, FRAGMENT) } {
                Ok(program) => a.program = Some(program),
                Err(error) => {
                    a.failed = true;
                    log::warn!(
                        "astronomy wallpaper shader failed; keeping static wallpaper: {error}"
                    );
                    return;
                }
            }
        }
        let Some(program) = a.program else {
            return;
        };
        let sun = solar_direction(a.sample_unix);
        let longitude = viewing_longitude(a.sample_unix, a.sample_offset);
        unsafe {
            let gl = &self.gl;
            gl.use_program(Some(program));
            gl.uniform_2_f32(
                gl.get_uniform_location(program, "resolution").as_ref(),
                self.screen_w as f32,
                self.screen_h as f32,
            );
            gl.uniform_3_f32(
                gl.get_uniform_location(program, "sun_direction").as_ref(),
                sun[0],
                sun[1],
                sun[2],
            );
            gl.uniform_1_f32(
                gl.get_uniform_location(program, "view_longitude").as_ref(),
                longitude,
            );
            gl.uniform_1_i32(gl.get_uniform_location(program, "earth_map").as_ref(), 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_vertex_array(None);
            gl.use_program(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
