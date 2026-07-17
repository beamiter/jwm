// Compositor::new() constructor
#[allow(unused_imports)]
use super::math::ortho;
#[allow(unused_imports)]
use super::*;
use crate::backend::x11::compositor_common::BootstrapState;
#[allow(unused_imports)]
use glow::HasContext;
#[allow(unused_imports)]
use std::collections::HashMap;
#[allow(unused_imports)]
use std::ffi::CString;
#[allow(unused_imports)]
use std::sync::Arc;
#[allow(unused_imports)]
use std::sync::mpsc;

impl<C: CompositorConnection> Compositor<C> {
    pub(crate) fn new(
        conn: Arc<C>,
        root: u32,
        screen_w: u32,
        screen_h: u32,
        primary_refresh_hz: u32,
    ) -> Result<Self, String> {
        // 1. Check composite extension
        conn.query_composite_version()?;

        // 2. Redirect subwindows
        conn.redirect_subwindows_manual(root)?;

        // RAII guard: if we return Err after the redirect, undo it so the screen
        // doesn't go permanently black.
        struct RedirectGuard<C: CompositorConnection> {
            conn: Arc<C>,
            root: u32,
            overlay: Option<u32>,
            active: bool,
        }
        impl<C: CompositorConnection> Drop for RedirectGuard<C> {
            fn drop(&mut self) {
                if self.active {
                    let _ = self.conn.unredirect_subwindows_manual(self.root);
                    if let Some(ow) = self.overlay {
                        let _ = self.conn.release_overlay_window(ow);
                    }
                    let _ = self.conn.flush_x11();
                }
            }
        }
        let mut guard = RedirectGuard::<C> {
            conn: conn.clone(),
            root,
            overlay: None,
            active: true,
        };

        // 3-5. Bootstrap X11 compositor state shared across X11 backends.
        let BootstrapState {
            damage_event_base,
            overlay_window,
        } = conn.bootstrap_state(root)?;
        guard.overlay = Some(overlay_window);

        // Select the shared X11 graphics platform. JWM_COMPOSITOR_API
        // overrides config for diagnostics and recovery without editing files.
        let (hdr_enabled, configured_api) = {
            let cfg = crate::config::CONFIG.load();
            let behavior = cfg.behavior();
            (behavior.hdr_enabled, behavior.compositor_api.clone())
        };
        let requested_api = std::env::var("JWM_COMPOSITOR_API").unwrap_or(configured_api);
        let graphics_preference = GraphicsApiPreference::parse(&requested_api)?;
        let graphics = GraphicsPlatform::new(
            overlay_window,
            conn.get_window_visual(overlay_window)?,
            hdr_enabled,
            graphics_preference,
        )?;
        let cm_selection_owner =
            conn.claim_compositor_selection_owner(root, graphics.screen_num())?;
        graphics.make_current()?;

        log::info!(
            "compositor: creating glow context through {}",
            graphics.api_name()
        );
        let gl =
            unsafe { glow::Context::from_loader_function(|name| graphics.get_proc_address(name)) };
        let oml_loaded = graphics.load_oml();

        // P5D: Create shader cache
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join("jwm")
            .join("shaders");
        let shader_cache = ShaderCache::new(cache_dir);

        log::info!("compositor: glow GL context created, compiling shaders...");
        // 14. Compile shaders with caching
        let program = shader_cache.get_or_compile(
            &gl,
            "main",
            shaders::VERTEX_SHADER,
            shaders::FRAGMENT_SHADER,
        )?;
        let shadow_program = shader_cache.get_or_compile(
            &gl,
            "shadow",
            shaders::VERTEX_SHADER,
            shaders::SHADOW_FRAGMENT_SHADER,
        )?;

        // Cache uniform locations (avoids per-frame string lookups)
        let win_uniforms = unsafe {
            WindowUniforms {
                projection: gl.get_uniform_location(program, "u_projection"),
                rect: gl.get_uniform_location(program, "u_rect"),
                texture: gl.get_uniform_location(program, "u_texture"),
                opacity: gl.get_uniform_location(program, "u_opacity"),
                radius: gl.get_uniform_location(program, "u_radius"),
                size: gl.get_uniform_location(program, "u_size"),
                dim: gl.get_uniform_location(program, "u_dim"),
                uv_rect: gl.get_uniform_location(program, "u_uv_rect"),
                ripple_progress: gl.get_uniform_location(program, "u_ripple_progress"),
                ripple_amplitude: gl.get_uniform_location(program, "u_ripple_amplitude"),
            }
        };
        let shadow_uniforms = unsafe {
            ShadowUniforms {
                projection: gl.get_uniform_location(shadow_program, "u_projection"),
                rect: gl.get_uniform_location(shadow_program, "u_rect"),
                shadow_color: gl.get_uniform_location(shadow_program, "u_shadow_color"),
                size: gl.get_uniform_location(shadow_program, "u_size"),
                radius: gl.get_uniform_location(shadow_program, "u_radius"),
                spread: gl.get_uniform_location(shadow_program, "u_spread"),
            }
        };

        // Compile blur shaders
        let blur_down_program = shader_cache.get_or_compile(
            &gl,
            "blur_down",
            shaders::BLUR_DOWN_VERTEX,
            shaders::BLUR_DOWN_FRAGMENT,
        )?;
        let blur_up_program = shader_cache.get_or_compile(
            &gl,
            "blur_up",
            shaders::BLUR_DOWN_VERTEX,
            shaders::BLUR_UP_FRAGMENT,
        )?;
        let blur_down_uniforms = unsafe {
            BlurUniforms {
                projection: gl.get_uniform_location(blur_down_program, "u_projection"),
                rect: gl.get_uniform_location(blur_down_program, "u_rect"),
                texture: gl.get_uniform_location(blur_down_program, "u_texture"),
                halfpixel: gl.get_uniform_location(blur_down_program, "u_halfpixel"),
            }
        };
        let blur_up_uniforms = unsafe {
            BlurUniforms {
                projection: gl.get_uniform_location(blur_up_program, "u_projection"),
                rect: gl.get_uniform_location(blur_up_program, "u_rect"),
                texture: gl.get_uniform_location(blur_up_program, "u_texture"),
                halfpixel: gl.get_uniform_location(blur_up_program, "u_halfpixel"),
            }
        };

        // P4: Compile temporal blur mix shader
        let temporal_blur_mix_program = shader_cache.get_or_compile(
            &gl,
            "temporal_mix",
            shaders::TEMPORAL_BLUR_MIX_VERTEX,
            shaders::TEMPORAL_BLUR_MIX_FRAGMENT,
        )?;
        let temporal_blur_mix_uniforms = unsafe {
            BlurUniforms {
                projection: gl.get_uniform_location(temporal_blur_mix_program, "u_projection"),
                rect: gl.get_uniform_location(temporal_blur_mix_program, "u_rect"),
                texture: gl.get_uniform_location(temporal_blur_mix_program, "u_texture"),
                halfpixel: gl.get_uniform_location(temporal_blur_mix_program, "u_halfpixel"),
            }
        };

        // Compile border shader (feature 1)
        let border_program = shader_cache.get_or_compile(
            &gl,
            "border",
            shaders::VERTEX_SHADER,
            shaders::BORDER_FRAGMENT_SHADER,
        )?;
        let border_uniforms = unsafe {
            BorderUniforms {
                projection: gl.get_uniform_location(border_program, "u_projection"),
                rect: gl.get_uniform_location(border_program, "u_rect"),
                border_color: gl.get_uniform_location(border_program, "u_border_color"),
                size: gl.get_uniform_location(border_program, "u_size"),
                radius: gl.get_uniform_location(border_program, "u_radius"),
                border_width: gl.get_uniform_location(border_program, "u_border_width"),
            }
        };

        // Compile post-process shader (features 8/9/10 + magnifier)
        let postprocess_program = shader_cache.get_or_compile(
            &gl,
            "postprocess",
            shaders::BLUR_DOWN_VERTEX,
            shaders::MAGNIFIER_POSTPROCESS_FRAGMENT_SHADER,
        )?;
        let postprocess_uniforms = unsafe {
            PostprocessUniforms {
                projection: gl.get_uniform_location(postprocess_program, "u_projection"), // P5F.1
                rect: gl.get_uniform_location(postprocess_program, "u_rect"),             // P5F.1
                texture: gl.get_uniform_location(postprocess_program, "u_texture"),
                color_temp: gl.get_uniform_location(postprocess_program, "u_color_temp"),
                saturation: gl.get_uniform_location(postprocess_program, "u_saturation"),
                brightness: gl.get_uniform_location(postprocess_program, "u_brightness"),
                contrast: gl.get_uniform_location(postprocess_program, "u_contrast"),
                invert: gl.get_uniform_location(postprocess_program, "u_invert"),
                grayscale: gl.get_uniform_location(postprocess_program, "u_grayscale"),
                hdr_enabled: gl.get_uniform_location(postprocess_program, "u_hdr_enabled"),
                hdr_peak_nits: gl.get_uniform_location(postprocess_program, "u_hdr_peak_nits"),
                tone_mapping_method: gl
                    .get_uniform_location(postprocess_program, "u_tone_mapping_method"),
                eotf_mode: gl.get_uniform_location(postprocess_program, "u_eotf_mode"),
                output_colorspace: gl
                    .get_uniform_location(postprocess_program, "u_output_colorspace"),
            }
        };

        let slime_wave_program = shader_cache.get_or_compile(
            &gl,
            "slime_wave_sim",
            shaders::BLUR_DOWN_VERTEX,
            shaders::SLIME_WAVE_SIM_FRAGMENT_SHADER,
        )?;
        let slime_wave_uniforms = unsafe {
            SlimeWaveUniforms {
                projection: gl.get_uniform_location(slime_wave_program, "u_projection"),
                rect: gl.get_uniform_location(slime_wave_program, "u_rect"),
                state: gl.get_uniform_location(slime_wave_program, "u_state"),
                texel: gl.get_uniform_location(slime_wave_program, "u_texel"),
                aspect: gl.get_uniform_location(slime_wave_program, "u_aspect"),
                turbulence: gl.get_uniform_location(slime_wave_program, "u_turbulence"),
                foam: gl.get_uniform_location(slime_wave_program, "u_foam"),
                injection_count: gl.get_uniform_location(slime_wave_program, "u_injection_count"),
                injections: gl.get_uniform_location(slime_wave_program, "u_injections[0]"),
                injection_params: gl
                    .get_uniform_location(slime_wave_program, "u_injection_params[0]"),
                time_step: gl.get_uniform_location(slime_wave_program, "u_time_step"),
            }
        };

        let slime_pressure_program = shader_cache.get_or_compile(
            &gl,
            "slime_pressure",
            shaders::BLUR_DOWN_VERTEX,
            shaders::SLIME_PRESSURE_FRAGMENT_SHADER,
        )?;
        let slime_pressure_uniforms = unsafe {
            SlimePressureUniforms {
                projection: gl.get_uniform_location(slime_pressure_program, "u_projection"),
                rect: gl.get_uniform_location(slime_pressure_program, "u_rect"),
                state: gl.get_uniform_location(slime_pressure_program, "u_state"),
                pressure: gl.get_uniform_location(slime_pressure_program, "u_pressure"),
                texel: gl.get_uniform_location(slime_pressure_program, "u_texel"),
                mode: gl.get_uniform_location(slime_pressure_program, "u_mode"),
                projection_amount: gl
                    .get_uniform_location(slime_pressure_program, "u_projection_amount"),
            }
        };

        let magnifier_uniforms = unsafe {
            MagnifierUniforms {
                magnifier_enabled: gl
                    .get_uniform_location(postprocess_program, "u_magnifier_enabled"),
                magnifier_center: gl
                    .get_uniform_location(postprocess_program, "u_magnifier_center"),
                magnifier_radius: gl
                    .get_uniform_location(postprocess_program, "u_magnifier_radius"),
                magnifier_zoom: gl.get_uniform_location(postprocess_program, "u_magnifier_zoom"),
                slime_enabled: gl.get_uniform_location(postprocess_program, "u_slime_enabled"),
                slime_points: gl.get_uniform_location(postprocess_program, "u_slime_points[0]"),
                slime_depths: gl.get_uniform_location(postprocess_program, "u_slime_depths[0]"),
                slime_bbox: gl.get_uniform_location(postprocess_program, "u_slime_bbox"),
                slime_surface_rect: gl
                    .get_uniform_location(postprocess_program, "u_slime_surface_rect"),
                slime_screen_size: gl
                    .get_uniform_location(postprocess_program, "u_slime_screen_size"),
                slime_scale: gl.get_uniform_location(postprocess_program, "u_slime_scale"),
                slime_strength: gl.get_uniform_location(postprocess_program, "u_slime_strength"),
                slime_ocean_strength: gl
                    .get_uniform_location(postprocess_program, "u_slime_ocean_strength"),
                slime_turbulence_strength: gl
                    .get_uniform_location(postprocess_program, "u_slime_turbulence_strength"),
                slime_foam_strength: gl
                    .get_uniform_location(postprocess_program, "u_slime_foam_strength"),
                slime_opacity: gl.get_uniform_location(postprocess_program, "u_slime_opacity"),
                slime_time: gl.get_uniform_location(postprocess_program, "u_slime_time"),
                slime_wave: gl.get_uniform_location(postprocess_program, "u_slime_wave"),
                slime_wave_texel: gl
                    .get_uniform_location(postprocess_program, "u_slime_wave_texel"),
                colorblind_mode: gl.get_uniform_location(postprocess_program, "u_colorblind_mode"),
            }
        };

        // Compile HUD shader (feature 11)
        let hud_program = shader_cache.get_or_compile(
            &gl,
            "hud",
            shaders::VERTEX_SHADER,
            shaders::HUD_FRAGMENT_SHADER,
        )?;
        let hud_uniforms = unsafe {
            HudUniforms {
                projection: gl.get_uniform_location(hud_program, "u_projection"),
                rect: gl.get_uniform_location(hud_program, "u_rect"),
                bg_color: gl.get_uniform_location(hud_program, "u_bg_color"),
                fg_color: gl.get_uniform_location(hud_program, "u_fg_color"),
                size: gl.get_uniform_location(hud_program, "u_size"),
            }
        };

        // Compile HUD text shader (feature 11b)
        let hud_text_program = shader_cache.get_or_compile(
            &gl,
            "hud_text",
            shaders::VERTEX_SHADER,
            shaders::HUD_TEXT_FRAGMENT_SHADER,
        )?;
        let hud_text_uniforms = unsafe {
            HudTextUniforms {
                projection: gl.get_uniform_location(hud_text_program, "u_projection"),
                rect: gl.get_uniform_location(hud_text_program, "u_rect"),
                texture: gl.get_uniform_location(hud_text_program, "u_texture"),
            }
        };

        let annotation_line_program = shader_cache.get_or_compile(
            &gl,
            "annotation_line",
            shaders::LINE_VERTEX_SHADER,
            shaders::LINE_FRAGMENT_SHADER,
        )?;
        let annotation_line_uniforms = unsafe {
            LineUniforms {
                projection: gl.get_uniform_location(annotation_line_program, "u_projection"),
                color: gl.get_uniform_location(annotation_line_program, "u_color"),
            }
        };

        // Compile tag-switch transition shader
        let transition_program = shader_cache.get_or_compile(
            &gl,
            "transition",
            shaders::BLUR_DOWN_VERTEX,
            shaders::TRANSITION_FRAGMENT_SHADER,
        )?;
        let transition_uniforms = unsafe {
            TransitionUniforms {
                projection: gl.get_uniform_location(transition_program, "u_projection"),
                rect: gl.get_uniform_location(transition_program, "u_rect"),
                texture: gl.get_uniform_location(transition_program, "u_texture"),
                opacity: gl.get_uniform_location(transition_program, "u_opacity"),
                uv_rect: gl.get_uniform_location(transition_program, "u_uv_rect"),
            }
        };

        // Compile cube transition shader
        let cube_program = shader_cache.get_or_compile(
            &gl,
            "cube",
            shaders::CUBE_VERTEX_SHADER,
            shaders::CUBE_FRAGMENT_SHADER,
        )?;
        let cube_uniforms = unsafe {
            CubeUniforms {
                mvp: gl.get_uniform_location(cube_program, "u_mvp"),
                aspect: gl.get_uniform_location(cube_program, "u_aspect"),
                texture: gl.get_uniform_location(cube_program, "u_texture"),
                brightness: gl.get_uniform_location(cube_program, "u_brightness"),
                uv_rect: gl.get_uniform_location(cube_program, "u_uv_rect"),
            }
        };

        // Compile portal transition shader
        let portal_program = shader_cache.get_or_compile(
            &gl,
            "portal",
            shaders::BLUR_DOWN_VERTEX,
            shaders::PORTAL_FRAGMENT_SHADER,
        )?;
        let portal_uniforms = unsafe {
            PortalUniforms {
                projection: gl.get_uniform_location(portal_program, "u_projection"),
                rect: gl.get_uniform_location(portal_program, "u_rect"),
                texture: gl.get_uniform_location(portal_program, "u_texture"),
                progress: gl.get_uniform_location(portal_program, "u_progress"),
                glow: gl.get_uniform_location(portal_program, "u_glow"),
                center: gl.get_uniform_location(portal_program, "u_center"),
                uv_rect: gl.get_uniform_location(portal_program, "u_uv_rect"),
            }
        };

        // Compile edge glow shader
        let edge_glow_program = shader_cache.get_or_compile(
            &gl,
            "edge_glow",
            shaders::VERTEX_SHADER,
            shaders::EDGE_GLOW_FRAGMENT_SHADER,
        )?;
        let edge_glow_uniforms = unsafe {
            EdgeGlowUniforms {
                projection: gl.get_uniform_location(edge_glow_program, "u_projection"),
                rect: gl.get_uniform_location(edge_glow_program, "u_rect"),
                glow_color: gl.get_uniform_location(edge_glow_program, "u_glow_color"),
                glow_width: gl.get_uniform_location(edge_glow_program, "u_glow_width"),
                mouse: gl.get_uniform_location(edge_glow_program, "u_mouse"),
                screen_size: gl.get_uniform_location(edge_glow_program, "u_screen_size"),
                time: gl.get_uniform_location(edge_glow_program, "u_time"),
            }
        };

        // Compile tilt shader (uses tilt vertex + tilt fragment)
        let tilt_program = shader_cache.get_or_compile(
            &gl,
            "tilt",
            shaders::TILT_VERTEX_SHADER,
            shaders::TILT_FRAGMENT_SHADER,
        )?;
        let tilt_uniforms = unsafe {
            TiltUniforms {
                projection: gl.get_uniform_location(tilt_program, "u_projection"),
                rect: gl.get_uniform_location(tilt_program, "u_rect"),
                texture: gl.get_uniform_location(tilt_program, "u_texture"),
                opacity: gl.get_uniform_location(tilt_program, "u_opacity"),
                radius: gl.get_uniform_location(tilt_program, "u_radius"),
                size: gl.get_uniform_location(tilt_program, "u_size"),
                dim: gl.get_uniform_location(tilt_program, "u_dim"),
                uv_rect: gl.get_uniform_location(tilt_program, "u_uv_rect"),
                tilt: gl.get_uniform_location(tilt_program, "u_tilt"),
                perspective: gl.get_uniform_location(tilt_program, "u_perspective"),
                grid_size: gl.get_uniform_location(tilt_program, "u_grid_size"),
                light_dir: gl.get_uniform_location(tilt_program, "u_light_dir"),
            }
        };

        // Compile wobbly shader (uses wobbly vertex + standard fragment)
        let wobbly_program = shader_cache.get_or_compile(
            &gl,
            "wobbly",
            shaders::WOBBLY_VERTEX_SHADER,
            shaders::FRAGMENT_SHADER,
        )?;
        let wobbly_uniforms = unsafe {
            WobblyUniforms {
                projection: gl.get_uniform_location(wobbly_program, "u_projection"),
                rect: gl.get_uniform_location(wobbly_program, "u_rect"),
                texture: gl.get_uniform_location(wobbly_program, "u_texture"),
                opacity: gl.get_uniform_location(wobbly_program, "u_opacity"),
                radius: gl.get_uniform_location(wobbly_program, "u_radius"),
                size: gl.get_uniform_location(wobbly_program, "u_size"),
                dim: gl.get_uniform_location(wobbly_program, "u_dim"),
                uv_rect: gl.get_uniform_location(wobbly_program, "u_uv_rect"),
                grid_offsets: gl.get_uniform_location(wobbly_program, "u_grid_offsets"),
                grid_n: gl.get_uniform_location(wobbly_program, "u_grid_n"),
            }
        };

        // Compile overview background shader
        let overview_bg_program = shader_cache.get_or_compile(
            &gl,
            "overview_bg",
            shaders::VERTEX_SHADER,
            shaders::OVERVIEW_BG_FRAGMENT_SHADER,
        )?;
        let overview_bg_uniforms = unsafe {
            OverviewBgUniforms {
                projection: gl.get_uniform_location(overview_bg_program, "u_projection"),
                rect: gl.get_uniform_location(overview_bg_program, "u_rect"),
                opacity: gl.get_uniform_location(overview_bg_program, "u_opacity"),
            }
        };

        // Compile particle shader
        let particle_program = shader_cache.get_or_compile(
            &gl,
            "particle",
            shaders::PARTICLE_VERTEX_SHADER,
            shaders::PARTICLE_FRAGMENT_SHADER,
        )?;
        let particle_uniforms = unsafe {
            ParticleUniforms {
                projection: gl.get_uniform_location(particle_program, "u_projection"),
                point_size: gl.get_uniform_location(particle_program, "u_point_size"),
            }
        };

        // Create particle VAO/VBO
        let (particle_vao, particle_vbo) = unsafe {
            let vao = gl
                .create_vertex_array()
                .map_err(|e| format!("particle vao: {e}"))?;
            let vbo = gl
                .create_buffer()
                .map_err(|e| format!("particle vbo: {e}"))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            // Layout: vec2 position, vec4 color, float life = 7 floats per vertex
            let stride = 7 * 4; // 7 floats * 4 bytes
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(1, 4, glow::FLOAT, false, stride, 2 * 4);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(2, 1, glow::FLOAT, false, stride, 6 * 4);
            gl.enable_vertex_attrib_array(2);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            (vao, vbo)
        };

        // 15. Create VAO (empty — vertex shader generates quad from gl_VertexID)
        let quad_vao = unsafe {
            let vao = gl
                .create_vertex_array()
                .map_err(|e| format!("create vao: {e}"))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_vertex_array(None);
            vao
        };

        // Phase 3.2: Compile genie minimize shader
        let genie_program = shader_cache.get_or_compile(
            &gl,
            "genie",
            shaders::GENIE_VERTEX_SHADER,
            shaders::FRAGMENT_SHADER,
        )?;
        let genie_uniforms = unsafe {
            GenieUniforms {
                projection: gl.get_uniform_location(genie_program, "u_projection"),
                rect: gl.get_uniform_location(genie_program, "u_rect"),
                texture: gl.get_uniform_location(genie_program, "u_texture"),
                opacity: gl.get_uniform_location(genie_program, "u_opacity"),
                radius: gl.get_uniform_location(genie_program, "u_radius"),
                size: gl.get_uniform_location(genie_program, "u_size"),
                dim: gl.get_uniform_location(genie_program, "u_dim"),
                uv_rect: gl.get_uniform_location(genie_program, "u_uv_rect"),
                progress: gl.get_uniform_location(genie_program, "u_progress"),
                dock_pos: gl.get_uniform_location(genie_program, "u_dock_pos"),
                grid_size: gl.get_uniform_location(genie_program, "u_grid_size"),
            }
        };

        // 16. Setup GL state
        unsafe {
            gl.viewport(0, 0, screen_w as i32, screen_h as i32);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
        }

        log::info!(
            "Compositor initialized: {}x{}, overlay=0x{:x}, damage_event_base={}",
            screen_w,
            screen_h,
            overlay_window,
            damage_event_base
        );

        // Success — defuse the guard so it doesn't undo our redirect
        guard.active = false;

        // Read compositor visual settings from config
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        let anim_speed = cfg.animation_speed();

        // Load Present extension (always try to load even if not the selected vsync method,
        // since it may be used for per-window presentation alongside other methods)
        let present_loaded = present::load_present_manager(conn.clone());

        // Determine which VSync method to use based on config and availability
        let (oml, vsync_method, present_mgr) = match behavior.vsync_method.as_str() {
            "oml_sync_control" => {
                if let Some(oml_ctrl) = oml_loaded {
                    log::info!(
                        "compositor: using GLX_OML_sync_control for per-window vblank timing"
                    );
                    (Some(oml_ctrl), VsyncMethod::OmlSyncControl, present_loaded)
                } else {
                    log::warn!(
                        "compositor: GLX_OML_sync_control requested but unavailable, falling back to global vsync"
                    );
                    (None, VsyncMethod::Global, present_loaded)
                }
            }
            "present" => {
                if present_loaded.is_some() {
                    log::info!(
                        "compositor: using Present extension for per-window independent presentation"
                    );
                    (oml_loaded, VsyncMethod::Present, present_loaded)
                } else {
                    // Fallback chain: Present unavailable → try OML → Global
                    if let Some(oml_ctrl) = oml_loaded {
                        log::warn!(
                            "compositor: Present unavailable, falling back to OML_sync_control"
                        );
                        (Some(oml_ctrl), VsyncMethod::OmlSyncControl, None)
                    } else {
                        log::warn!(
                            "compositor: Present and OML both unavailable, using global vsync"
                        );
                        (None, VsyncMethod::Global, None)
                    }
                }
            }
            _ => {
                log::info!("compositor: using graphics-platform global vsync");
                (oml_loaded, VsyncMethod::Global, present_loaded)
            }
        };

        // Parse per-window rules.
        let opacity_rules = parse_opacity_rules(&behavior.opacity_rules);
        let corner_radius_rules = parse_corner_radius_rules(&behavior.corner_radius_rules);
        let scale_rules = parse_scale_rules(&behavior.scale_rules);

        // Create blur FBOs if blur is enabled
        let blur_fbos = if behavior.blur_enabled {
            unsafe { Self::create_blur_fbos(&gl, screen_w, screen_h, behavior.blur_strength) }
        } else {
            Vec::new()
        };

        // Create scene capture FBO for blur source
        let scene_fbo = if behavior.blur_enabled {
            unsafe { Self::create_scene_fbo(&gl, screen_w, screen_h).ok() }
        } else {
            None
        };

        // Load wallpaper asynchronously — decode on background thread so the
        // desktop appears immediately and the wallpaper fades in once ready.
        let wallpaper_mode = parse_wallpaper_mode(&behavior.wallpaper_mode);
        let pending_wallpaper = if !behavior.wallpaper.is_empty() {
            Some(Self::load_wallpaper_async(
                &behavior.wallpaper,
                screen_w,
                screen_h,
                wallpaper_mode,
            ))
        } else {
            None
        };

        // Create post-process FBO (features 8/9/10) — needed if any post-processing is active
        let needs_postprocess = behavior.color_temperature != 0.0
            || behavior.saturation != 1.0
            || behavior.brightness != 1.0
            || behavior.contrast != 1.0
            || behavior.invert_colors
            || behavior.grayscale;
        let postprocess_fbo = if needs_postprocess {
            unsafe { Self::create_scene_fbo(&gl, screen_w, screen_h).ok() }
        } else {
            None
        };

        // The pose data plane is optional: compositor startup must not fail if
        // the runtime directory is read-only or another experimental instance
        // already owns the socket.
        let slime_ipc = match SlimeIpc::bind_default() {
            Ok(ipc) => Some(ipc),
            Err(err) => {
                log::warn!("compositor: slime pose IPC disabled: {err}");
                None
            }
        };

        // Parse P4: blur_strength_by_hz configuration
        let blur_strength_by_hz = parse_blur_strength_by_hz(&behavior.blur_strength_by_hz);

        // Parse P4: blur_quality_by_monitor configuration
        let blur_quality_by_monitor =
            parse_blur_quality_by_monitor(&behavior.blur_quality_by_monitor);

        // P5: Apply dynamic blur strength based on Hz
        // Use actual primary monitor refresh rate (now queried from RandR)
        let mut dynamic_blur_strength = behavior.blur_strength;
        if !blur_strength_by_hz.is_empty() {
            // Use actual primary monitor refresh rate from RandR
            if let Some(hz_strength) =
                blur_strength_for_hz(&blur_strength_by_hz, primary_refresh_hz)
            {
                dynamic_blur_strength = hz_strength;
                log::info!(
                    "compositor: dynamic blur strength at {}Hz: {} (config: {})",
                    primary_refresh_hz,
                    hz_strength,
                    behavior.blur_strength
                );
            }
        }

        // P5B Phase 1: Build monitor rectangles from RandR (must do before consuming conn)
        let monitor_rects = Self::build_monitor_rects(&conn, root);
        // P5B Phase 2: Build monitor refresh rates from RandR.
        let mut monitor_refresh_rates = Self::build_monitor_refresh_rates(&conn, root);
        if let [single_monitor] = monitor_rects.as_slice() {
            let monitor_id = single_monitor.0;
            let queried_hz = monitor_refresh_rates.get(&monitor_id).copied();
            if queried_hz != Some(primary_refresh_hz) {
                log::debug!(
                    "compositor: replacing single-monitor RandR refresh {:?}Hz with primary {}Hz",
                    queried_hz,
                    primary_refresh_hz
                );
                monitor_refresh_rates.insert(monitor_id, primary_refresh_hz);
            }
        }

        // P5B: Log detected monitor configuration
        log::info!("compositor: P5B detected {} monitors", monitor_rects.len());
        for (id, x, y, w, h) in &monitor_rects {
            let hz = monitor_refresh_rates.get(id).copied().unwrap_or(60);
            log::info!(
                "  Monitor {}: rect=({},{} {}x{}) refresh={}Hz",
                id,
                x,
                y,
                w,
                h,
                hz
            );
        }

        let hdr_output_10bit = graphics.output_is_10bit();

        Ok(Self {
            conn,
            graphics,
            overlay_window,
            cm_selection_owner,
            gl,
            shader_cache,
            program,
            shadow_program,
            blur_down_program,
            blur_up_program,
            temporal_blur_mix_program,
            temporal_blur_mix_uniforms,
            win_uniforms,
            shadow_uniforms,
            blur_down_uniforms,
            blur_up_uniforms,
            quad_vao,
            windows: HashMap::new(),
            screen_w,
            screen_h,
            root,
            needs_render: true,
            damage_render_pending: false,
            context_current: true,
            last_scene_hash: 0,
            corner_radius: behavior.corner_radius,
            shadow_enabled: behavior.shadow_enabled,
            shadow_radius: behavior.shadow_radius,
            shadow_offset: behavior.shadow_offset,
            shadow_color: behavior.shadow_color,
            inactive_opacity: behavior.inactive_opacity,
            active_opacity: behavior.active_opacity,
            blur_enabled: behavior.blur_enabled,
            blur_strength: dynamic_blur_strength,
            blur_fbos,
            scene_fbo,
            fading: behavior.fading,
            fade_in_step: anim_speed.apply_fade_step(behavior.fade_in_step),
            fade_out_step: anim_speed.apply_fade_step(behavior.fade_out_step),
            shadow_exclude: behavior.shadow_exclude.clone(),
            opacity_rules,
            blur_exclude: behavior.blur_exclude.clone(),
            rounded_corners_exclude: behavior.rounded_corners_exclude.clone(),
            detect_client_opacity: behavior.detect_client_opacity,
            fullscreen_unredirect: behavior.fullscreen_unredirect,
            unredirected_window: None,
            vsync_method,
            oml,
            audio_sync: audio_sync::AudioSyncManager::new(),
            present_mgr,
            // Feature 1: borders
            border_program,
            border_uniforms,
            border_enabled: behavior.border_enabled,
            border_width: behavior.border_width,
            border_color_focused: behavior.border_color_focused,
            border_color_unfocused: behavior.border_color_unfocused,
            // Feature 3: per-window corner radius
            corner_radius_rules,
            // Feature 4: scale
            scale_rules,
            // Feature 6: damage tracking (tile-based, Phase 2.1)
            partial_damage_enabled: true,
            damage_tracker: DamageTracker::new(screen_w, screen_h),
            // P5C: Rectangle-level dirty tracking
            dirty_region_tracker: DirtyRegionTracker::new(screen_w, screen_h),
            buffer_age_damage_history: BufferAgeDamageHistory::new(),
            // Phase 2.2: Blur quality auto-downgrade
            blur_quality: BlurQuality::Full,
            blur_quality_auto: behavior.blur_quality_auto,
            // Feature 8: color management
            postprocess_program,
            postprocess_uniforms,
            slime_wave_program,
            slime_wave_uniforms,
            slime_pressure_program,
            slime_pressure_uniforms,
            slime_wave_simulation: None,
            postprocess_fbo,
            color_temperature: behavior.color_temperature,
            saturation: behavior.saturation,
            brightness: behavior.brightness,
            contrast: behavior.contrast,
            // Feature 10: invert / accessibility
            invert_colors: behavior.invert_colors,
            grayscale: behavior.grayscale,
            // P3: HDR / 10-bit output
            hdr_enabled: behavior.hdr_enabled,
            hdr_peak_nits: behavior.hdr_peak_nits,
            tone_mapping_method: match behavior.tone_mapping_method.as_str() {
                "reinhard" => 1,
                "aces" => 2,
                _ => 0, // "none" or unknown
            },
            // Feature 11: debug HUD
            hud_program,
            hud_uniforms,
            hud_text_program,
            hud_text_uniforms,
            annotation_line_program,
            annotation_line_uniforms,
            hud_text_texture: None,
            hud_text_width: 0,
            hud_text_height: 0,
            hud_text_cache: String::new(),
            system_ui: None,
            debug_hud: behavior.debug_hud,
            sys_stats: crate::backend::sys_stats::SysStatsSampler::new(),
            frame_stats: FrameStats::new(),
            // Feature 12: screenshot
            screenshot_requests: Default::default(),
            // Feature 13: blur mask
            blur_use_frame_extents: behavior.blur_use_frame_extents,
            // Feature 14: shadow shape
            shadow_bottom_extra: behavior.shadow_bottom_extra,
            // Tag-switch crossfade transition
            transition_program,
            transition_uniforms,
            transition_fbo: None,
            transition_start: None,
            transition_duration: std::time::Duration::from_millis(anim_speed.apply_duration(150)),
            transition_direction: 1.0,
            transition_exclude_top: 0,
            transition_mon_x: 0,
            transition_mon_y: 0,
            transition_mon_w: screen_w,
            transition_mon_h: screen_h,
            transition_mode: TransitionMode::from_name(behavior.transition_mode.as_str()),
            // Cube transition
            cube_program,
            cube_uniforms,
            transition_new_fbo: None,
            // Portal transition
            portal_program,
            portal_uniforms,
            // Window scale animation
            window_animation: behavior.window_animation,
            window_animation_scale: behavior.window_animation_scale,
            // Dim inactive
            inactive_dim: behavior.inactive_dim,
            // Mouse position
            mouse_x: 0.0,
            mouse_y: 0.0,
            // Edge glow
            edge_glow_program,
            edge_glow_uniforms,
            edge_glow: behavior.edge_glow,
            edge_glow_active: false,
            edge_glow_suppressed: false,
            edge_glow_color: behavior.edge_glow_color,
            edge_glow_width: behavior.edge_glow_width,
            // Attention animation
            attention_animation: behavior.attention_animation,
            attention_color: behavior.attention_color,
            compositor_start_time: std::time::Instant::now(),
            // PiP visual
            pip_border_color: behavior.pip_border_color,
            pip_border_width: behavior.pip_border_width,
            // Magnifier
            magnifier_enabled: behavior.magnifier_enabled,
            magnifier_radius: behavior.magnifier_radius,
            magnifier_zoom: behavior.magnifier_zoom,
            magnifier_uniforms,
            // Realtime slime hand refraction
            slime_ipc,
            slime_state: SlimeState::default(),
            slime_effect_enabled: std::env::var("JWM_SLIME_ENABLED")
                .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            // Window tilt
            tilt_program,
            tilt_uniforms,
            window_tilt: behavior.window_tilt,
            tilt_amount: behavior.tilt_amount,
            tilt_perspective: behavior.tilt_perspective,
            tilt_speed: behavior.tilt_speed,
            tilt_grid: behavior.tilt_grid.max(1),
            tilt_current_x: 0.0,
            tilt_current_y: 0.0,
            tilt_target_x: 0.0,
            tilt_target_y: 0.0,
            // Frosted glass
            frosted_glass_rules: behavior.frosted_glass_rules.clone(),
            frosted_glass_strength: behavior.frosted_glass_strength,
            blur_cache_hash: 0,
            // Overview
            overview_active: false,
            overview_windows: Vec::new(),
            overview_opacity: 0.0,
            overview_bg_program,
            overview_bg_uniforms,
            // Overview prism state
            overview_prism_target_angle: 0.0,
            overview_prism_current_angle: 0.0,
            overview_prism_last_tick: None,
            overview_slide_offset: 0,
            overview_total_clients: 0,
            overview_mon_x: 0,
            overview_mon_y: 0,
            overview_mon_w: screen_w,
            overview_mon_h: screen_h,
            overview_entry_progress: 1.0,
            overview_closing: false,
            overview_exit_progress: 1.0,
            // Wobbly windows
            wobbly_program,
            wobbly_uniforms,
            wobbly_windows: behavior.wobbly_windows,
            wobbly_stiffness: behavior.wobbly_stiffness,
            wobbly_damping: behavior.wobbly_damping,
            wobbly_restore_stiffness: behavior.wobbly_restore_stiffness,
            wobbly_grid_size: behavior.wobbly_grid_size,
            // Phase 5: Expose/Mission Control
            expose_active: false,
            expose_enabled: behavior.expose_enabled,
            expose_gap: behavior.expose_gap,
            expose_entries: Vec::new(),
            expose_opacity: 0.0,
            expose_start: None,
            // Phase 5: Smart Snap Preview
            snap_preview_enabled: behavior.snap_preview,
            snap_preview_color: behavior.snap_preview_color,
            snap_animation_duration_ms: behavior.snap_animation_duration_ms,
            snap_target: None,
            // Phase 5: Window Peek
            peek_active: false,
            peek_enabled: behavior.peek_enabled,
            peek_exclude: behavior.peek_exclude.clone(),
            peek_opacity: 1.0,
            peek_start: None,
            // Phase 5: Window Tabs
            window_tabs_enabled: behavior.window_tabs,
            tab_bar_height: behavior.tab_bar_height,
            tab_bar_color: behavior.tab_bar_color,
            tab_active_color: behavior.tab_active_color,
            window_groups: HashMap::new(),
            // Particle effects
            particle_program,
            particle_uniforms,
            particle_effects: behavior.particle_effects,
            particle_count: behavior.particle_count,
            particle_lifetime: behavior.particle_lifetime,
            particle_gravity: behavior.particle_gravity,
            particle_systems: Vec::new(),
            particle_vao,
            particle_vbo,
            // Wallpaper (texture loaded asynchronously)
            wallpaper_texture: None,
            wallpaper_mode,
            wallpaper_path: behavior.wallpaper.clone(),
            wallpaper_img_w: 0,
            wallpaper_img_h: 0,
            monitor_wallpapers: Vec::new(),
            // Phase 6.1: Colorblind correction
            colorblind_mode: match behavior.colorblind_mode.as_str() {
                "deuteranopia" => 1,
                "protanopia" => 2,
                "tritanopia" => 3,
                _ => 0,
            },
            // Phase 6.2: Annotations
            annotation_active: false,
            annotation_strokes: Vec::new(),
            annotation_color: behavior.annotation_color,
            annotation_line_width: behavior.annotation_line_width,
            // Phase 6.3: Zoom to fit
            zoom_to_fit_window: None,
            zoom_to_fit_scale: 1.0,
            zoom_to_fit_target: 1.0,
            // Phase 7.2: Extended debug HUD
            debug_hud_extended: behavior.debug_hud_extended,
            // Phase 7.3: Screen recording
            recording_active: false,
            recording_fps: behavior.recording_fps,
            recording_bitrate: behavior.recording_bitrate.clone(),
            recording_quality: behavior.recording_quality,
            recording_encoder: behavior.recording_encoder.clone(),
            recording_output_dir: behavior.recording_output_dir.clone(),
            recording_process: None,
            recording_last_frame: None,
            recording_pbo: [None, None],
            recording_cursor: [None, None],
            recording_frame_region: [(0, 0, screen_w, screen_h); 2],
            recording_region: (0, 0, screen_w, screen_h),
            recording_output_size: (screen_w, screen_h),
            recording_fbo: None,
            recording_region_overlay: None,
            recording_current_pbo: 0,
            recording_captured_frames: 0,
            // Phase 3.1: Motion trail
            motion_trail_enabled: behavior.motion_trail,
            motion_trail_frames: behavior.motion_trail_frames,
            motion_trail_opacity: behavior.motion_trail_opacity,
            // Phase 3.2: Genie minimize
            genie_program,
            genie_uniforms,
            genie_minimize: behavior.genie_minimize,
            genie_duration_ms: behavior.genie_duration_ms,
            genie_active: Vec::new(),
            dock_position: (0.5 * screen_w as f32, screen_h as f32),
            // Phase 3.3: Ripple on open
            ripple_on_open: behavior.ripple_on_open,
            ripple_duration: behavior.ripple_duration,
            ripple_amplitude: behavior.ripple_amplitude,
            ripple_active: Vec::new(),
            // Phase 3.4: Focus highlight
            focus_highlight: behavior.focus_highlight,
            focus_highlight_color: behavior.focus_highlight_color,
            focus_highlight_duration_ms: behavior.focus_highlight_duration_ms,
            focus_highlight_start: None,
            last_focused_window: None,
            // Phase 3.5: Wallpaper crossfade
            wallpaper_crossfade: behavior.wallpaper_crossfade,
            wallpaper_crossfade_duration_ms: behavior.wallpaper_crossfade_duration_ms,
            old_wallpaper_texture: None,
            wallpaper_transition_start: None,
            // Async wallpaper loading
            pending_wallpaper,
            pending_monitor_wallpapers: Vec::new(),
            // Shader hot-reload
            shader_hot_reload_enabled: false,
            shader_dir: String::new(),
            shader_file_mtimes: std::collections::HashMap::new(),
            is_game_window: HashMap::new(),
            vrr_active: false,
            vrr_last_check: std::time::Instant::now(),
            last_gpu_load: 0,
            last_gpu_load_update: std::time::Instant::now(),
            // P4: Per-monitor and temporal blur
            blur_strength_by_hz,
            blur_quality_by_monitor,
            monitor_rects,         // P5B Phase 1: Real monitor geometry
            monitor_refresh_rates, // P5B Phase 2: Per-monitor refresh rates
            prev_blur_fbo: None,
            prev_window_positions_hash: 0,
            temporal_blur_mix_ratio: behavior.blur_temporal_mix_ratio,
            temporal_blur_enabled: behavior.blur_temporal_enabled,
            temporal_blur_reuse_count: 0,
            temporal_blur_total_count: 0,
            // P6C: PBO uploader (4MB PBOs, pool of 4)
            pbo_uploader: PBOUploader::new(4 * 1024 * 1024, 4),
            // P6A: Async X11 communication
            priority_event_queue: PriorityEventQueue::new(),
            deferred_ops_queue: DeferredOpQueue::new(256),
            // P7A: Predictive rendering
            predictive_render_mgr: PredictiveRenderManager::new(),
            // P7C: Smart cache warmup
            cache_warmup_mgr: CacheWarmupManager::new(),
            // P7D: Power saving mode
            power_saving_mgr: PowerSavingManager::new(PowerSavingConfig::new()),
            subpixel_render_mgr: SubpixelRenderManager::new(),

            // Phase 2 Optimizations
            direct_scanout_mgr: {
                let mut mgr = DirectScanoutManager::new(screen_w, screen_h);
                mgr.set_enabled(behavior.direct_scanout_enabled);
                mgr
            },
            frame_profiler: {
                let mut profiler = FrameProfiler::new();
                profiler.set_enabled(behavior.profiling_enabled);
                profiler
            },
            gl_state_tracker: GLStateTracker::new(),

            // Benchmark harness
            benchmark: benchmark::BenchmarkHarness::new(),

            // HDR output control
            eotf_mode: 0,
            output_colorspace: 0,
            hdr_output_10bit,
            scratch_scene_info: Vec::new(),
            scratch_blur_dirty: Vec::new(),
            scratch_tfp_order: Vec::new(),
            scratch_refresh_wins: Vec::new(),
            scratch_new_pixmaps: Vec::new(),
            scratch_swap_damage: Vec::with_capacity(16 * 4),
        })
    }

    pub(crate) unsafe fn create_program(
        gl: &glow::Context,
        vs_src: &str,
        fs_src: &str,
    ) -> Result<glow::Program, String> {
        unsafe {
            let vs = gl
                .create_shader(glow::VERTEX_SHADER)
                .map_err(|e| format!("create vs: {e}"))?;
            gl.shader_source(vs, vs_src);
            gl.compile_shader(vs);
            if !gl.get_shader_compile_status(vs) {
                let info = gl.get_shader_info_log(vs);
                gl.delete_shader(vs);
                return Err(format!("vertex shader: {info}"));
            }

            let fs = gl
                .create_shader(glow::FRAGMENT_SHADER)
                .map_err(|e| format!("create fs: {e}"))?;
            gl.shader_source(fs, fs_src);
            gl.compile_shader(fs);
            if !gl.get_shader_compile_status(fs) {
                let info = gl.get_shader_info_log(fs);
                gl.delete_shader(vs);
                gl.delete_shader(fs);
                return Err(format!("fragment shader: {info}"));
            }

            let program = gl
                .create_program()
                .map_err(|e| format!("create program: {e}"))?;
            gl.attach_shader(program, vs);
            gl.attach_shader(program, fs);
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                let info = gl.get_program_info_log(program);
                gl.delete_program(program);
                gl.delete_shader(vs);
                gl.delete_shader(fs);
                return Err(format!("link program: {info}"));
            }
            gl.delete_shader(vs);
            gl.delete_shader(fs);
            Ok(program)
        }
    }
}
