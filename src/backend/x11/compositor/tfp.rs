use glow::HasContext;
use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
use super::Compositor;
use super::{WindowTexture, RippleState, GLX_TEXTURE_TARGET_EXT, GLX_TEXTURE_2D_EXT, GLX_TEXTURE_FORMAT_EXT, GLX_TEXTURE_FORMAT_RGBA_EXT, GLX_TEXTURE_FORMAT_RGB_EXT, GLX_FRONT_LEFT_EXT};

impl Compositor {
    // =====================================================================
    // Feature 13: Set frame extents for blur mask
    // =====================================================================
    pub(in crate::backend::x11) fn set_frame_extents(&mut self, x11_win: u32, left: u32, right: u32, top: u32, bottom: u32) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.frame_extents = [left, right, top, bottom];
        }
    }

    // =====================================================================
    // Feature 14: Set shaped window
    // =====================================================================
    pub(in crate::backend::x11) fn set_window_shaped(&mut self, x11_win: u32, shaped: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_shaped = shaped;
        }
    }

    // ----- Window management -----

    pub(in crate::backend::x11) fn add_window(&mut self, x11_win: u32, x: i32, y: i32, w: u32, h: u32) {
        if self.windows.contains_key(&x11_win) {
            return;
        }
        if w == 0 || h == 0 {
            return;
        }
        log::info!(
            "compositor: add_window START 0x{:x} {}x{} at ({},{})",
            x11_win, w, h, x, y
        );

        // Create damage
        let damage_id = match self.conn.generate_id() {
            Ok(id) => id,
            Err(e) => {
                log::warn!("compositor: generate_id for damage failed: {e}");
                return;
            }
        };
        if let Err(e) = self
            .conn
            .damage_create(damage_id, x11_win, damage::ReportLevel::NON_EMPTY)
        {
            log::warn!("compositor: damage_create failed for 0x{x11_win:x}: {e}");
            return;
        }

        // NameWindowPixmap
        let pixmap = match self.conn.generate_id() {
            Ok(id) => id,
            Err(e) => {
                log::warn!("compositor: generate_id for pixmap failed: {e}");
                let _ = self.conn.damage_destroy(damage_id);
                return;
            }
        };
        if let Err(e) = self.conn.composite_name_window_pixmap(x11_win, pixmap) {
            log::warn!("compositor: name_window_pixmap failed for 0x{x11_win:x}: {e}");
            let _ = self.conn.damage_destroy(damage_id);
            return;
        }
        // Flush x11rb AND sync Xlib so the pixmap XID is visible to GLX.
        let _ = self.conn.flush();

        // Select the TFP FBConfig for this window.  First try an exact match
        // by visual ID (required on older Mesa, e.g. Ubuntu 20); fall back to
        // the generic depth-based selection.
        let win_visual = self
            .conn
            .get_window_attributes(x11_win)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|a| a.visual)
            .unwrap_or(0);
        let win_depth = self
            .conn
            .get_geometry(x11_win)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|g| g.depth)
            .unwrap_or(24);

        let (fbconfig, use_rgba) = if let Some(&(cfg, is_rgba)) =
            self.tfp_visual_configs.get(&win_visual)
        {
            log::debug!(
                "compositor: win 0x{:x} visual 0x{:x} -> per-visual FBConfig (rgba={})",
                x11_win, win_visual, is_rgba
            );
            (cfg, is_rgba)
        } else {
            // Fallback: depth-based selection
            let rgba = win_depth == 32 && !self.fbconfig_rgba.is_null();
            let cfg = if rgba {
                self.fbconfig_rgba
            } else {
                self.fbconfig_rgb
            };
            log::debug!(
                "compositor: win 0x{:x} visual 0x{:x} depth={} -> depth-based FBConfig (rgba={})",
                x11_win, win_visual, win_depth, rgba
            );
            (cfg, rgba)
        };
        if fbconfig.is_null() {
            log::warn!(
                "compositor: no fbconfig for visual=0x{:x} depth={} win=0x{:x}",
                win_visual, win_depth, x11_win
            );
            let _ = self.conn.free_pixmap(pixmap);
            let _ = self.conn.damage_destroy(damage_id);
            return;
        }
        let tex_fmt = if use_rgba {
            GLX_TEXTURE_FORMAT_RGBA_EXT
        } else {
            GLX_TEXTURE_FORMAT_RGB_EXT
        };

        // Create GLX pixmap for TFP
        let pixmap_attrs: Vec<i32> = vec![
            GLX_TEXTURE_TARGET_EXT,
            GLX_TEXTURE_2D_EXT,
            GLX_TEXTURE_FORMAT_EXT,
            tex_fmt,
            0,
        ];

        log::info!(
            "compositor: add_window 0x{:x} depth={} rgba={} pixmap=0x{:x}, calling glXCreatePixmap...",
            x11_win, win_depth, use_rgba, pixmap
        );
        let glx_pixmap = unsafe {
            // Sync both connections so the Xlib display can see the pixmap
            // created by x11rb.
            x11::xlib::XSync(self.xlib_display, 0);

            x11::glx::glXCreatePixmap(
                self.xlib_display,
                fbconfig,
                pixmap as _,
                pixmap_attrs.as_ptr(),
            )
        };
        log::info!("compositor: glXCreatePixmap returned 0x{:x}", glx_pixmap);
        if glx_pixmap == 0 {
            log::warn!("compositor: glXCreatePixmap failed for 0x{x11_win:x}");
            let _ = self.conn.free_pixmap(pixmap);
            let _ = self.conn.damage_destroy(damage_id);
            return;
        }

        // Create GL texture
        let gl_texture = unsafe {
            match self.gl.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("compositor: create_texture failed: {e}");
                    x11::glx::glXDestroyPixmap(self.xlib_display, glx_pixmap);
                    let _ = self.conn.free_pixmap(pixmap);
                    let _ = self.conn.damage_destroy(damage_id);
                    return;
                }
            }
        };

        // Bind texture
        log::info!("compositor: add_window 0x{:x} binding TFP texture...", x11_win);
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(gl_texture));
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            (self.tfp.bind)(
                self.xlib_display,
                glx_pixmap,
                GLX_FRONT_LEFT_EXT,
                std::ptr::null(),
            );
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }
        log::info!("compositor: add_window 0x{:x} COMPLETE", x11_win);

        // Start with fade opacity = 0 if fading is enabled (will fade in)
        let initial_fade = if self.fading { 0.0 } else { 1.0 };

        self.windows.insert(
            x11_win,
            WindowTexture {
                x,
                y,
                w,
                h,
                damage: damage_id,
                pixmap,
                glx_pixmap,
                gl_texture,
                dirty: true,
                has_rgba: use_rgba,
                fbconfig,
                needs_pixmap_refresh: false,
                x11_win,
                fade_opacity: initial_fade,
                fading_out: false,
                class_name: String::new(),
                opacity_override: None,
                is_fullscreen: false,
                corner_radius_override: None,
                scale: 1.0,
                frame_extents: [0; 4],
                is_shaped: false,
                anim_scale: if self.window_animation { self.window_animation_scale } else { 1.0 },
                anim_scale_target: 1.0,
                is_urgent: false,
                is_pip: false,
                is_frosted: false,
                wobbly: None,
                pending_fence: None,
                motion_trail: std::collections::VecDeque::new(),
            },
        );

        // Phase 3.3: Trigger ripple effect on window open
        if self.ripple_on_open {
            self.ripple_active.push(RippleState {
                start: std::time::Instant::now(),
            });
        }

        self.needs_render = true;

        log::debug!(
            "compositor: add_window 0x{:x} {}x{} at ({},{})",
            x11_win,
            w,
            h,
            x,
            y
        );
    }

    /// Update the compositor's screen dimensions (e.g. after a RandR hotplug).
    /// The overlay window is resized automatically by the X server, but we need
    /// to update our GL viewport and projection matrix dimensions.
    pub(in crate::backend::x11) fn resize(&mut self, new_w: u32, new_h: u32) {
        if new_w == self.screen_w && new_h == self.screen_h {
            return;
        }
        log::info!(
            "compositor: resize {}x{} -> {}x{}",
            self.screen_w, self.screen_h, new_w, new_h
        );
        self.screen_w = new_w;
        self.screen_h = new_h;
        self.needs_render = true;

        // Resize damage tracker for new screen dimensions
        self.damage_tracker.resize(new_w, new_h);

        // Recreate blur FBOs for new screen size
        if self.blur_enabled {
            unsafe {
                for level in self.blur_fbos.drain(..) {
                    self.gl.delete_framebuffer(level.fbo);
                    self.gl.delete_texture(level.texture);
                }
                self.blur_fbos = Self::create_blur_fbos(&self.gl, new_w, new_h, self.blur_strength);
                if let Some((fbo, tex)) = self.scene_fbo.take() {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
                self.scene_fbo = Self::create_scene_fbo(&self.gl, new_w, new_h).ok();
            }
        }
        // Recreate postprocess FBO
        if self.postprocess_fbo.is_some() {
            unsafe {
                if let Some((fbo, tex)) = self.postprocess_fbo.take() {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
                self.postprocess_fbo = Self::create_scene_fbo(&self.gl, new_w, new_h).ok();
            }
        }
        // Cancel in-progress transition on resize (screen geometry changed)
        if let Some((fbo, tex)) = self.transition_fbo.take() {
            unsafe {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(tex);
            }
            self.transition_start = None;
        }
    }

    pub(in crate::backend::x11) fn remove_window(&mut self, x11_win: u32) {
        // Spawn particles for closing window
        if self.particle_effects {
            if let Some(wt) = self.windows.get(&x11_win) {
                self.spawn_particles_for_window(wt.x, wt.y, wt.w, wt.h);
            }
        }

        // Phase 3.2: Start genie minimize animation
        if self.genie_minimize {
            if let Some(wt) = self.windows.get(&x11_win) {
                self.start_genie_animation(
                    x11_win,
                    wt.x as f32, wt.y as f32,
                    wt.w as f32, wt.h as f32,
                );
            }
        }

        // If fading is enabled and the window exists, start fade-out instead of immediate remove
        if self.fading {
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                if !wt.fading_out && wt.fade_opacity > 0.0 {
                    wt.fading_out = true;
                    wt.anim_scale_target = self.window_animation_scale;
                    self.needs_render = true;
                    return;
                }
            }
        }

        self.remove_window_immediate(x11_win);
    }

    /// Actually remove a window (no fade). Used internally.
    pub(super) fn remove_window_immediate(&mut self, x11_win: u32) {
        let Some(wt) = self.windows.remove(&x11_win) else {
            return;
        };
        self.needs_render = true;
        // Undo fullscreen unredirect if this was the unredirected window
        if self.unredirected_window == Some(x11_win) {
            self.unredirected_window = None;
        }

        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
            (self.tfp.release)(self.xlib_display, wt.glx_pixmap, GLX_FRONT_LEFT_EXT);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.delete_texture(wt.gl_texture);
            x11::glx::glXDestroyPixmap(self.xlib_display, wt.glx_pixmap);
        }
        let _ = self.conn.free_pixmap(wt.pixmap);
        let _ = self.conn.damage_destroy(wt.damage);

        log::debug!("compositor: remove_window 0x{:x}", x11_win);
    }

    pub(in crate::backend::x11) fn update_geometry(&mut self, x11_win: u32, x: i32, y: i32, w: u32, h: u32) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            let size_changed = wt.w != w || wt.h != h;
            let moved = wt.x != x || wt.y != y;
            wt.x = x;
            wt.y = y;
            self.needs_render = true;

            if moved {
                // Window move exposes old screen area and occupies new area.
                // Damage events are not always sufficient for both regions,
                // so request a full-frame redraw to prevent trails/ghosting.
                self.damage_tracker.mark_all_dirty();
            }

            if size_changed && w > 0 && h > 0 {
                wt.w = w;
                wt.h = h;
                // Defer the heavy pixmap recreation to the next render_frame()
                // call, so multiple resize events within a single frame are batched.
                wt.needs_pixmap_refresh = true;
            }
        }
    }

    /// Recreate GLX pixmaps for windows that had their size changed.
    /// Called once per frame in render_frame() to batch all pending recreations.
    pub(super) fn refresh_pixmaps(&mut self) {
        // Collect window IDs that need refresh to avoid borrowing issues
        let refresh_wins: Vec<u32> = self
            .windows
            .iter()
            .filter(|(_, wt)| wt.needs_pixmap_refresh)
            .map(|(&id, _)| id)
            .collect();

        if refresh_wins.is_empty() {
            return;
        }

        // Release old pixmaps for all windows that need refresh
        for &win in &refresh_wins {
            let wt = self.windows.get(&win).unwrap();
            unsafe {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                (self.tfp.release)(self.xlib_display, wt.glx_pixmap, GLX_FRONT_LEFT_EXT);
                self.gl.bind_texture(glow::TEXTURE_2D, None);
                x11::glx::glXDestroyPixmap(self.xlib_display, wt.glx_pixmap);
            }
            let _ = self.conn.free_pixmap(wt.pixmap);
        }

        // Create new named pixmaps for all windows via x11rb
        let mut new_pixmaps: Vec<(u32, u32)> = Vec::new(); // (win, pixmap)
        for &win in &refresh_wins {
            let wt = self.windows.get_mut(&win).unwrap();
            let pixmap = match self.conn.generate_id() {
                Ok(id) => id,
                Err(_) => {
                    wt.glx_pixmap = 0;
                    wt.pixmap = 0;
                    wt.needs_pixmap_refresh = false;
                    continue;
                }
            };
            if self
                .conn
                .composite_name_window_pixmap(wt.x11_win, pixmap)
                .is_err()
            {
                wt.glx_pixmap = 0;
                wt.pixmap = 0;
                wt.needs_pixmap_refresh = false;
                continue;
            }
            wt.pixmap = pixmap;
            new_pixmaps.push((win, pixmap));
        }

        // Single flush + sync for the entire batch
        let _ = self.conn.flush();
        unsafe {
            x11::xlib::XSync(self.xlib_display, 0);
        }

        // Create GLX pixmaps and rebind textures
        for (win, pixmap) in new_pixmaps {
            let wt = self.windows.get_mut(&win).unwrap();
            let fbconfig = wt.fbconfig;
            let tex_fmt = if wt.has_rgba {
                GLX_TEXTURE_FORMAT_RGBA_EXT
            } else {
                GLX_TEXTURE_FORMAT_RGB_EXT
            };
            let pixmap_attrs: Vec<i32> = vec![
                GLX_TEXTURE_TARGET_EXT,
                GLX_TEXTURE_2D_EXT,
                GLX_TEXTURE_FORMAT_EXT,
                tex_fmt,
                0,
            ];
            let glx_pixmap = unsafe {
                x11::glx::glXCreatePixmap(
                    self.xlib_display,
                    fbconfig,
                    pixmap as _,
                    pixmap_attrs.as_ptr(),
                )
            };
            if glx_pixmap == 0 {
                let _ = self.conn.free_pixmap(pixmap);
                wt.pixmap = 0;
                wt.glx_pixmap = 0;
                wt.needs_pixmap_refresh = false;
                continue;
            }

            unsafe {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                (self.tfp.bind)(
                    self.xlib_display,
                    glx_pixmap,
                    GLX_FRONT_LEFT_EXT,
                    std::ptr::null(),
                );
                self.gl.bind_texture(glow::TEXTURE_2D, None);

                // Phase 2.3: Insert fence after rebind to avoid GPU stall
                if let Some(old_fence) = wt.pending_fence.take() {
                    self.gl.delete_sync(old_fence);
                }
                wt.pending_fence = self.gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0).ok();
            }

            wt.glx_pixmap = glx_pixmap;
            wt.dirty = true;
            wt.needs_pixmap_refresh = false;
        }

        // Clear flag for any remaining windows (error paths above)
        for &win in &refresh_wins {
            if let Some(wt) = self.windows.get_mut(&win) {
                wt.needs_pixmap_refresh = false;
            }
        }
    }

    pub(in crate::backend::x11) fn mark_damaged(&mut self, x11_win: u32) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.dirty = true;
            self.needs_render = true;
            // Subtract damage so we get future notifications
            let _ = self.conn.damage_subtract(wt.damage, 0u32, 0u32);
        }
    }

    /// Set the window class name (for per-window rules).
    pub(in crate::backend::x11) fn set_window_class(&mut self, x11_win: u32, class_name: &str) {
        // Look up per-window rules before borrowing windows mutably
        let opacity_override = self.lookup_opacity_rule(class_name);
        let corner_radius_override = self.lookup_corner_radius_rule(class_name);
        let scale = self.lookup_scale_rule(class_name);
        let is_frosted = self.lookup_frosted_glass_rule(class_name);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if wt.class_name != class_name {
                wt.class_name = class_name.to_string();
                wt.opacity_override = opacity_override;
                wt.corner_radius_override = corner_radius_override;
                wt.is_frosted = is_frosted;
                if let Some(s) = scale {
                    wt.scale = s;
                }
                self.needs_render = true;
            }
        }
    }

    /// Set/unset fullscreen state for a window (for fullscreen unredirect).
    pub(in crate::backend::x11) fn set_window_fullscreen(&mut self, x11_win: u32, fullscreen: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if wt.is_fullscreen != fullscreen {
                wt.is_fullscreen = fullscreen;
                self.needs_render = true;
            }
        }
    }
}
