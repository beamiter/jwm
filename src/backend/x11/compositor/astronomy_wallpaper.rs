//! Astronomy is a wallpaper material, rendered below every client window.
use super::*;
use crate::backend::compositor_common::animated_wallpaper::{
    AstronomyDraw, POWER_QUERY_INTERVAL, WallpaperClock, astronomy_draw_plan,
    resolve_earth_texture, solar_direction, viewing_longitude,
};
use glow::HasContext;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERTEX: &str = "#version 330 core\nvoid main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);gl_Position=vec4(p*2.0-1.0,0.0,1.0);}";
const FRAGMENT: &str = include_str!("shaders/animated_wallpaper.frag");

/// Uniform locations belong to one successfully linked program. Keeping them
/// with its handle prevents stale locations from surviving program recreation.
struct AstronomyProgram {
    program: glow::Program,
    sun_direction: Option<glow::UniformLocation>,
    view_longitude: Option<glow::UniformLocation>,
    earth_map: Option<glow::UniformLocation>,
    canvas_origin: Option<glow::UniformLocation>,
    resolution: Option<glow::UniformLocation>,
    sphere_radius: Option<glow::UniformLocation>,
}

impl AstronomyProgram {
    unsafe fn new(gl: &glow::Context, program: glow::Program) -> Self {
        unsafe {
            Self {
                program,
                sun_direction: gl.get_uniform_location(program, "sun_direction"),
                view_longitude: gl.get_uniform_location(program, "view_longitude"),
                earth_map: gl.get_uniform_location(program, "earth_map"),
                canvas_origin: gl.get_uniform_location(program, "canvas_origin"),
                resolution: gl.get_uniform_location(program, "resolution"),
                sphere_radius: gl.get_uniform_location(program, "sphere_radius"),
            }
        }
    }
}

#[derive(Default)]
pub(super) struct AstronomyWallpaper {
    enabled: bool,
    path: String,
    configured_path: String,
    pending: Option<wallpaper::DeferredWallpaperLoad>,
    texture: Option<glow::Texture>,
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
    /// Rebuild only on material layout/topology changes, never on ordinary damage.
    fn update_layout(
        &mut self,
        enabled: bool,
        layout: &str,
        screen: (u32, u32),
        outputs: impl Iterator<Item = (i32, i32, u32, u32)> + Clone,
    ) -> bool {
        let a = self;
        let per_output = layout == "per-output";
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
                gl.delete_program(program.program);
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
        let configured = &cfg.behavior().wallpaper_earth_texture;
        let offset_seconds = cfg.behavior().wallpaper_timezone_offset_hours * 3600;
        let layout = &cfg.behavior().wallpaper_astronomy_layout;
        let a = &mut self.astronomy_wallpaper;
        let layout_changed = a.update_layout(
            enabled,
            layout,
            (self.screen_w, self.screen_h),
            self.monitor_wallpapers
                .iter()
                .map(|m| (m.mon_x, m.mon_y, m.mon_w, m.mon_h)),
        );
        let changed = a.enabled != enabled || a.configured_path != *configured;
        if changed {
            a.enabled = enabled;
            a.configured_path = configured.clone();
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
        }
        if !a.enabled || a.failed {
            return changed || layout_changed;
        }
        let power_changed = if a
            .last_power_query
            .is_none_or(|last| now.saturating_duration_since(last) >= POWER_QUERY_INTERVAL)
        {
            a.observe_power(now, self.graphics.wallpaper_power_allows_animation())
        } else {
            false
        };

        let timezone_changed = a.sample_offset != offset_seconds;
        if timezone_changed {
            a.sample_offset = offset_seconds;
            a.clock = WallpaperClock::default();
        }
        a.clock.set_paused(!a.powered || occluded);
        let tick = a.clock.tick(now);
        if tick {
            a.sample_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
        }
        a.frame_changed = changed || layout_changed || power_changed || timezone_changed || tick;
        a.frame_changed
    }

    /// Called with a current context, after the normal static wallpaper pass.
    pub(super) fn draw_astronomy_wallpaper(&mut self, repair: Option<(i32, i32, i32, i32)>) {
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
                Ok(program) => {
                    a.program = Some(unsafe { AstronomyProgram::new(&self.gl, program) });
                }
                Err(error) => {
                    // Failure stops future animation wakes. Release the uploaded
                    // image now, while this draw owns a current GL context,
                    // rather than waiting indefinitely for another frame.
                    if let Some(texture) = a.texture.take() {
                        unsafe {
                            self.gl.delete_texture(texture);
                        }
                    }
                    a.failed = true;
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
        let sun = solar_direction(a.sample_unix);
        let longitude = viewing_longitude(a.sample_unix, a.sample_offset);
        unsafe {
            let gl = &self.gl;
            gl.use_program(Some(program.program));
            gl.uniform_3_f32(program.sun_direction.as_ref(), sun[0], sun[1], sun[2]);
            gl.uniform_1_f32(program.view_longitude.as_ref(), longitude);
            gl.uniform_1_i32(program.earth_map.as_ref(), 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.enable(glow::SCISSOR_TEST);
            for canvas in &a.draw_plan {
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
                gl.uniform_1_f32(program.sphere_radius.as_ref(), canvas.radius);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            if let Some(clip) = repair {
                gl.scissor(clip.0, clip.1, clip.2, clip.3);
            } else {
                gl.disable(glow::SCISSOR_TEST);
            }
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
