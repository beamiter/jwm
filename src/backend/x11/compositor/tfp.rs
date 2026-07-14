use super::Compositor;
use super::{PixmapBinding, RippleState, WindowTexture};
use glow::HasContext;

use super::CompositorConnection;

impl<C: CompositorConnection> Compositor<C> {
    // =====================================================================
    // Feature 13: Set frame extents for blur mask
    // =====================================================================
    pub(crate) fn set_frame_extents(
        &mut self,
        x11_win: u32,
        left: u32,
        right: u32,
        top: u32,
        bottom: u32,
    ) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.frame_extents = [left, right, top, bottom];
        }
    }

    // =====================================================================
    // Feature 14: Set shaped window
    // =====================================================================
    pub(crate) fn set_window_shaped(&mut self, x11_win: u32, shaped: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_shaped = shaped;
        }
    }

    // =====================================================================
    // Mark window as override-redirect (unmanaged overlay)
    // =====================================================================
    pub(crate) fn set_window_override_redirect(&mut self, x11_win: u32, is_or: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_override_redirect = is_or;
        }
    }

    // ----- Window management -----

    pub(crate) fn add_window(&mut self, x11_win: u32, x: i32, y: i32, w: u32, h: u32) {
        if self.windows.contains_key(&x11_win) || w == 0 || h == 0 {
            return;
        }
        log::info!(
            "compositor: add_window START 0x{:x} {}x{} at ({},{})",
            x11_win,
            w,
            h,
            x,
            y
        );

        let damage_id = match self.conn.generate_xid() {
            Ok(id) => id,
            Err(error) => {
                log::warn!("compositor: generate_id for damage failed: {error}");
                return;
            }
        };
        if let Err(error) = self.conn.create_window_damage(damage_id, x11_win) {
            log::warn!("compositor: damage_create failed for 0x{x11_win:x}: {error}");
            return;
        }

        let pixmap = match self.conn.generate_xid() {
            Ok(id) => id,
            Err(error) => {
                log::warn!("compositor: generate_id for pixmap failed: {error}");
                let _ = self.conn.destroy_window_damage(damage_id);
                return;
            }
        };
        if let Err(error) = self.conn.name_window_pixmap(x11_win, pixmap) {
            log::warn!("compositor: name_window_pixmap failed for 0x{x11_win:x}: {error}");
            let _ = self.conn.destroy_window_damage(damage_id);
            return;
        }
        let _ = self.conn.flush_x11();

        let visual = self.conn.get_window_visual(x11_win).unwrap_or(0);
        let depth = self.conn.get_window_depth(x11_win).unwrap_or(24);
        let gl_texture = unsafe {
            match self.gl.create_texture() {
                Ok(texture) => texture,
                Err(error) => {
                    log::warn!("compositor: create_texture failed: {error}");
                    let _ = self.conn.free_window_pixmap(pixmap);
                    let _ = self.conn.destroy_window_damage(damage_id);
                    return;
                }
            }
        };
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
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }

        if let Err(error) = self.graphics.sync_x11() {
            log::warn!("compositor: native pixmap synchronization failed: {error}");
        }
        let (binding, use_rgba) = match self.graphics.import_pixmap(
            &self.gl,
            gl_texture,
            pixmap,
            visual,
            depth,
            self.hdr_enabled,
        ) {
            Ok(import) => import,
            Err(error) => {
                log::warn!(
                    "compositor: {} pixmap import failed for 0x{x11_win:x}: {error}",
                    self.graphics.api_name()
                );
                unsafe { self.gl.delete_texture(gl_texture) };
                let _ = self.conn.free_window_pixmap(pixmap);
                let _ = self.conn.destroy_window_damage(damage_id);
                return;
            }
        };

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
                binding: Some(binding),
                gl_texture,
                dirty: true,
                has_rgba: use_rgba,
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
                anim_scale: if self.window_animation {
                    self.window_animation_scale
                } else {
                    1.0
                },
                anim_scale_target: 1.0,
                is_urgent: false,
                is_pip: false,
                is_frosted: false,
                is_override_redirect: false,
                wobbly: None,
                pending_fence: None,
                motion_trail: std::collections::VecDeque::new(),
                audio_sync_target: None,
            },
        );

        if self.ripple_on_open {
            self.ripple_active.push(RippleState {
                x11_win,
                start: std::time::Instant::now(),
            });
        }
        self.needs_render = true;
        log::debug!(
            "compositor: add_window 0x{:x} {}x{} at ({},{}) via {}",
            x11_win,
            w,
            h,
            x,
            y,
            self.graphics.api_name()
        );
    }

    /// Update the compositor's screen dimensions (e.g. after a RandR hotplug).
    /// The overlay window is resized automatically by the X server, but we need
    /// to update our GL viewport and projection matrix dimensions.
    pub(crate) fn resize(&mut self, new_w: u32, new_h: u32) {
        if new_w == self.screen_w && new_h == self.screen_h {
            return;
        }
        log::info!(
            "compositor: resize {}x{} -> {}x{}",
            self.screen_w,
            self.screen_h,
            new_w,
            new_h
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
        if let Some(simulation) = self.slime_wave_simulation.take() {
            unsafe {
                for fbo in simulation.fbos {
                    self.gl.delete_framebuffer(fbo);
                }
                for texture in simulation.textures {
                    self.gl.delete_texture(texture);
                }
                for fbo in simulation.pressure_fbos {
                    self.gl.delete_framebuffer(fbo);
                }
                for texture in simulation.pressure_textures {
                    self.gl.delete_texture(texture);
                }
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

    pub(crate) fn remove_window(&mut self, x11_win: u32) {
        // Spawn particles for closing window
        if self.particle_effects {
            if let Some(wt) = self.windows.get(&x11_win) {
                self.spawn_particles_for_window(wt.x, wt.y, wt.w, wt.h);
            }
        }

        // Phase 3.2: Start genie minimize animation. This takes ownership of the
        // window's GPU/X resources and frees them when the animation completes,
        // so we must NOT fall through to remove_window_immediate (which would
        // delete the texture the genie pass is still sampling — a UAF).
        if self.genie_minimize {
            if let Some(wt) = self.windows.get(&x11_win) {
                let (gx, gy, gw, gh) = (wt.x as f32, wt.y as f32, wt.w as f32, wt.h as f32);
                self.start_genie_animation(x11_win, gx, gy, gw, gh);
                return;
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

    /// Release the GL texture, imported native pixmap, X pixmap, and Damage.
    /// Shared by immediate removal and genie-animation cleanup.
    pub(super) fn free_texture_resources(
        &mut self,
        gl_texture: glow::Texture,
        binding: Option<PixmapBinding>,
        pixmap: u32,
        damage: u32,
    ) {
        if let Some(binding) = binding {
            self.graphics
                .release_pixmap_binding(&self.gl, gl_texture, binding);
        }
        unsafe {
            self.gl.delete_texture(gl_texture);
        }
        if pixmap != 0 {
            let _ = self.conn.free_window_pixmap(pixmap);
        }
        if damage != 0 {
            let _ = self.conn.destroy_window_damage(damage);
        }
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

        self.free_texture_resources(wt.gl_texture, wt.binding, wt.pixmap, wt.damage);

        log::debug!("compositor: remove_window 0x{:x}", x11_win);
    }

    pub(crate) fn update_geometry(&mut self, x11_win: u32, x: i32, y: i32, w: u32, h: u32) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            let size_changed = wt.w != w || wt.h != h;
            let moved = wt.x != x || wt.y != y;
            let (old_x, old_y, old_w, old_h) = (wt.x, wt.y, wt.w, wt.h);
            wt.x = x;
            wt.y = y;
            self.needs_render = true;

            if moved {
                // Mark old and new positions as dirty instead of full screen.
                // Expand by shadow radius to cover shadow artifacts.
                let expand = self.shadow_radius as i32 + self.shadow_offset[0].abs() as i32 + 4;
                self.damage_tracker.mark_region_dirty(
                    old_x - expand,
                    old_y - expand,
                    old_w + expand as u32 * 2,
                    old_h + expand as u32 * 2,
                );
                self.damage_tracker.mark_region_dirty(
                    x - expand,
                    y - expand,
                    w.max(old_w) + expand as u32 * 2,
                    h.max(old_h) + expand as u32 * 2,
                );
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

    /// Recreate native pixmap imports for windows whose backing pixmap changed.
    /// Called once per frame so resize bursts are coalesced.
    pub(super) fn refresh_pixmaps(&mut self) {
        let mut refresh_wins = std::mem::take(&mut self.scratch_refresh_wins);
        refresh_wins.clear();
        refresh_wins.extend(
            self.windows
                .iter()
                .filter_map(|(&id, wt)| wt.needs_pixmap_refresh.then_some(id)),
        );
        if refresh_wins.is_empty() {
            self.scratch_refresh_wins = refresh_wins;
            return;
        }

        for &win in &refresh_wins {
            let (texture, binding, pixmap) = {
                let wt = self
                    .windows
                    .get_mut(&win)
                    .expect("tracked window disappeared");
                (wt.gl_texture, wt.binding.take(), wt.pixmap)
            };
            if let Some(binding) = binding {
                self.graphics
                    .release_pixmap_binding(&self.gl, texture, binding);
            }
            if pixmap != 0 {
                let _ = self.conn.free_window_pixmap(pixmap);
            }
        }

        let mut new_pixmaps = std::mem::take(&mut self.scratch_new_pixmaps);
        new_pixmaps.clear();
        new_pixmaps.reserve(refresh_wins.len());
        for &win in &refresh_wins {
            let x11_win = self.windows[&win].x11_win;
            let pixmap = match self.conn.generate_xid() {
                Ok(id) => id,
                Err(error) => {
                    log::warn!("compositor: resized pixmap XID allocation failed: {error}");
                    if let Some(wt) = self.windows.get_mut(&win) {
                        wt.pixmap = 0;
                        wt.needs_pixmap_refresh = false;
                    }
                    continue;
                }
            };
            if let Err(error) = self.conn.name_window_pixmap(x11_win, pixmap) {
                log::warn!("compositor: resized NameWindowPixmap failed: {error}");
                if let Some(wt) = self.windows.get_mut(&win) {
                    wt.pixmap = 0;
                    wt.needs_pixmap_refresh = false;
                }
                continue;
            }
            if let Some(wt) = self.windows.get_mut(&win) {
                wt.pixmap = pixmap;
            }
            new_pixmaps.push((win, pixmap));
        }

        let _ = self.conn.flush_x11();
        if let Err(error) = self.graphics.sync_x11() {
            log::warn!("compositor: resized pixmap synchronization failed: {error}");
        }

        for (win, pixmap) in new_pixmaps.drain(..) {
            let (texture, x11_win) = {
                let wt = &self.windows[&win];
                (wt.gl_texture, wt.x11_win)
            };
            let visual = self.conn.get_window_visual(x11_win).unwrap_or(0);
            let depth = self.conn.get_window_depth(x11_win).unwrap_or(24);
            match self.graphics.import_pixmap(
                &self.gl,
                texture,
                pixmap,
                visual,
                depth,
                self.hdr_enabled,
            ) {
                Ok((binding, rgba)) => {
                    let wt = self
                        .windows
                        .get_mut(&win)
                        .expect("tracked window disappeared");
                    wt.binding = Some(binding);
                    wt.has_rgba = rgba;
                    wt.dirty = true;
                    wt.needs_pixmap_refresh = false;
                    unsafe {
                        if let Some(old_fence) = wt.pending_fence.take() {
                            self.gl.delete_sync(old_fence);
                        }
                        wt.pending_fence =
                            self.gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0).ok();
                    }
                }
                Err(error) => {
                    log::warn!(
                        "compositor: resized {} pixmap import failed for 0x{x11_win:x}: {error}",
                        self.graphics.api_name()
                    );
                    let _ = self.conn.free_window_pixmap(pixmap);
                    if let Some(wt) = self.windows.get_mut(&win) {
                        wt.pixmap = 0;
                        wt.binding = None;
                        wt.needs_pixmap_refresh = false;
                    }
                }
            }
        }

        for &win in &refresh_wins {
            if let Some(wt) = self.windows.get_mut(&win) {
                wt.needs_pixmap_refresh = false;
            }
        }
        self.scratch_refresh_wins = refresh_wins;
        self.scratch_new_pixmaps = new_pixmaps;
    }

    pub(crate) fn mark_damaged(&mut self, x11_win: u32) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.dirty = true;
            self.needs_render = true;
            // Mark the window's region as dirty in the damage tracker
            // Expand by shadow radius to cover shadow
            let expand = self.shadow_radius as i32 + self.shadow_offset[0].abs() as i32 + 4;
            self.damage_tracker.mark_region_dirty(
                wt.x - expand,
                wt.y - expand,
                wt.w + expand as u32 * 2,
                wt.h + expand as u32 * 2,
            );
            // Subtract damage so we get future notifications
            let _ = self.conn.clear_window_damage(wt.damage);
        }
    }

    /// Set the window class name (for per-window rules).
    pub(crate) fn set_window_class(&mut self, x11_win: u32, class_name: &str) {
        // Look up per-window rules before borrowing windows mutably
        let opacity_override = self.lookup_opacity_rule(class_name);
        let corner_radius_override = self.lookup_corner_radius_rule(class_name);
        let scale = self.lookup_scale_rule(class_name);
        let is_frosted = self.lookup_frosted_glass_rule(class_name);

        // Auto-detect known video players for audio sync
        let is_video_player = self.is_known_video_player(class_name);
        // Detect games for VRR
        let is_game = self.detect_game_window(class_name);

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

                // Auto-enable audio sync for known video players
                if is_video_player && wt.audio_sync_target.is_none() {
                    log::info!(
                        "compositor: detected video player {} (0x{:x}), enabling audio sync",
                        class_name,
                        x11_win
                    );
                    // Default audio sync at 60fps; will be overridden by app notification
                    wt.audio_sync_target = Some(60.0);
                }

                // Track game windows for VRR
                if is_game {
                    self.is_game_window.insert(x11_win, true);
                    log::debug!(
                        "compositor: detected game window: {} (0x{:x})",
                        class_name,
                        x11_win
                    );
                } else {
                    self.is_game_window.remove(&x11_win);
                }
            }
        }
    }

    /// Check if a window class is a known video player
    fn is_known_video_player(&self, class_name: &str) -> bool {
        let class_lower = class_name.to_lowercase();
        matches!(
            class_lower.as_str(),
            "mpv"
                | "vlc"
                | "ffplay"
                | "kodi"
                | "mplayer"
                | "mplayer2"
                | "smplayer"
                | "totem"
                | "gstreamer"
                | "rhythmbox"
                | "audacious"
                | "clementine"
        )
    }

    /// Set/unset fullscreen state for a window (for fullscreen unredirect).
    pub(crate) fn set_window_fullscreen(&mut self, x11_win: u32, fullscreen: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if wt.is_fullscreen != fullscreen {
                wt.is_fullscreen = fullscreen;
                self.needs_render = true;
            }
        }
    }
}
