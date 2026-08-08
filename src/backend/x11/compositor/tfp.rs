use super::Compositor;
use super::{
    MinimizedWindowIntent, PixmapBinding, RippleState, WindowTexture, xcomposite_backing_changed,
};
use crate::backend::compositor_common::window_glow::WindowGlowSettings;
use glow::HasContext;

use super::CompositorConnection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowRetirement {
    Closed,
    ExplicitlyMinimized,
}

fn retirement_uses_genie(reason: WindowRetirement, genie_enabled: bool) -> bool {
    genie_enabled && reason == WindowRetirement::ExplicitlyMinimized
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddWindowMinimizeDisposition {
    TrackNormally,
    SettlePendingMinimize,
    StartExplicitRestore,
}

const fn refreshed_window_settles_pending_minimize(
    intent: Option<MinimizedWindowIntent>,
    import_succeeded: bool,
) -> bool {
    import_succeeded && matches!(intent, Some(MinimizedWindowIntent::PendingMinimize))
}

fn restore_cancels_uncaptured_direct_minimize(
    is_manually_unredirected: bool,
    intent: Option<MinimizedWindowIntent>,
    has_live_texture: bool,
    has_detached_pixels: bool,
) -> bool {
    is_manually_unredirected
        && matches!(intent, Some(MinimizedWindowIntent::PendingMinimize))
        && has_live_texture
        && !has_detached_pixels
}

const fn requested_minimized_window_intent(
    minimized: bool,
    lifecycle_active: bool,
) -> Option<MinimizedWindowIntent> {
    if minimized {
        Some(MinimizedWindowIntent::PendingMinimize)
    } else if lifecycle_active {
        Some(MinimizedWindowIntent::ExplicitRestore)
    } else {
        None
    }
}

fn prepare_window_restore_collections(
    pending_captures: &mut std::collections::HashSet<u32>,
    pending_uploads: &mut std::collections::HashSet<u32>,
    intents: &mut std::collections::HashMap<u32, MinimizedWindowIntent>,
    x11_win: u32,
    lifecycle_active: bool,
) {
    pending_captures.remove(&x11_win);
    pending_uploads.remove(&x11_win);
    match requested_minimized_window_intent(false, lifecycle_active) {
        Some(intent) => {
            intents.insert(x11_win, intent);
        }
        None => {
            // Idempotent restore after completion must not poison a future
            // ordinary AddWindow for a reused XID.
            intents.remove(&x11_win);
        }
    }
}

const fn add_window_minimize_disposition(
    intent: Option<MinimizedWindowIntent>,
) -> AddWindowMinimizeDisposition {
    match intent {
        Some(MinimizedWindowIntent::PendingMinimize) => {
            AddWindowMinimizeDisposition::SettlePendingMinimize
        }
        Some(MinimizedWindowIntent::ExplicitRestore) => {
            AddWindowMinimizeDisposition::StartExplicitRestore
        }
        None => AddWindowMinimizeDisposition::TrackNormally,
    }
}

impl<C: CompositorConnection> Compositor<C> {
    fn decoration_damage_margin(&self) -> i32 {
        let shadow_margin = if self.shadow_enabled && self.shadow_radius > 0.0 {
            (self.shadow_radius
                + self.shadow_offset[0].abs().max(self.shadow_offset[1].abs())
                + self.shadow_bottom_extra.max(0.0)
                + 4.0)
                .ceil() as i32
        } else {
            0
        };
        let config = crate::config::CONFIG.load();
        let glow_margin = WindowGlowSettings::from_behavior(config.behavior()).damage_margin();
        shadow_margin.max(glow_margin)
    }

    fn create_window_gl_texture(&self) -> Result<glow::Texture, String> {
        let texture = unsafe { self.gl.create_texture()? };
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
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
        Ok(texture)
    }

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
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&x11_win) {
            metadata.frame_extents = [left, right, top, bottom];
        }
    }

    // =====================================================================
    // Feature 14: Set shaped window
    // =====================================================================
    pub(crate) fn set_window_shaped(&mut self, x11_win: u32, shaped: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_shaped = shaped;
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&x11_win) {
            metadata.is_shaped = shaped;
        }
    }

    // =====================================================================
    // Mark window as override-redirect (unmanaged overlay)
    // =====================================================================
    pub(crate) fn set_window_override_redirect(&mut self, x11_win: u32, is_or: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_override_redirect = is_or;
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&x11_win) {
            metadata.is_override_redirect = is_or;
        }
    }

    // ----- Window management -----

    pub(crate) fn add_window(&mut self, x11_win: u32, x: i32, y: i32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let confirm_pixmap_on_damage = self.graphics.is_gles();
        let disposition =
            add_window_minimize_disposition(self.minimized_window_intents.get(&x11_win).copied());
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if wt.fading_out {
                // A client can remap the same XID before its unmap fade has
                // completed. Reuse the tracked resources, cancel retirement,
                // and refresh the named pixmap instead of letting the closing
                // tick delete a live window.
                wt.fading_out = false;
                wt.x = x;
                wt.y = y;
                wt.w = w;
                wt.h = h;
                wt.anim_scale_target = 1.0;
                wt.pixmap_refresh.backing_changed(confirm_pixmap_on_damage);
                wt.dirty = true;
                self.effect_tick_clock.reset();
                self.ripple_active.retain(|r| r.x11_win != x11_win);
                if self.ripple_on_open {
                    self.ripple_active.push(RippleState {
                        x11_win,
                        start: std::time::Instant::now(),
                    });
                }
                self.needs_render = true;
            }
            self.finish_add_window_minimize_lifecycle(x11_win, disposition, x, y, w, h);
            return;
        }

        // An explicit restore may still have a detached minimize texture. Keep
        // it until the reverse mesh finishes; the newly imported live entry is
        // filtered out of ordinary passes meanwhile. A pending minimize takes
        // the opposite path after import: it is converted straight into a
        // bounded retained visual without animating from hidden coordinates.
        log::debug!(
            "compositor: add_window START 0x{:x} {}x{} at ({},{})",
            x11_win,
            w,
            h,
            x,
            y
        );

        let format = if self.graphics.is_gles() {
            self.conn.get_window_depth(x11_win).map(|depth| (0, depth))
        } else {
            self.conn.get_window_visual_and_depth(x11_win)
        };
        let (visual, depth) = match format {
            Ok(format) => format,
            Err(error) => {
                log::debug!(
                    "compositor: skipping stale window 0x{x11_win:x}; format unavailable: {error}"
                );
                return;
            }
        };
        if depth == 0 {
            log::debug!("compositor: skipping input-only window 0x{x11_win:x}");
            return;
        }

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
            if error.contains("Match") || error.contains("BadMatch") {
                log::debug!(
                    "compositor: window 0x{x11_win:x} is not redirectable yet; skipping pixmap: {error}"
                );
            } else {
                log::warn!("compositor: name_window_pixmap failed for 0x{x11_win:x}: {error}");
            }
            let _ = self.conn.destroy_window_damage(damage_id);
            return;
        }
        let _ = self.conn.flush_x11();

        let gl_texture = match self.create_window_gl_texture() {
            Ok(texture) => texture,
            Err(error) => {
                log::warn!("compositor: create_texture failed: {error}");
                let _ = self.conn.free_window_pixmap(pixmap);
                let _ = self.conn.destroy_window_damage(damage_id);
                return;
            }
        };

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

        let ordinary_add = disposition == AddWindowMinimizeDisposition::TrackNormally;
        let initial_fade = if ordinary_add && self.fading {
            0.0
        } else {
            1.0
        };
        if ordinary_add && (self.fading || self.window_animation) {
            self.effect_tick_clock.reset();
        }
        self.windows.insert(
            x11_win,
            WindowTexture {
                x,
                y,
                w,
                h,
                x_border_width: None,
                damage: damage_id,
                pixmap,
                visual,
                depth,
                binding: Some(binding),
                gl_texture,
                dirty: true,
                has_rgba: use_rgba,
                pixmap_refresh: Default::default(),
                x11_win,
                fade_opacity: initial_fade,
                fading_out: false,
                class_name: String::new(),
                opacity_override: None,
                is_fullscreen: false,
                bypass_compositor: 0,
                corner_radius_override: None,
                scale: 1.0,
                frame_extents: [0; 4],
                is_shaped: false,
                anim_scale: if ordinary_add && self.window_animation {
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
                motion_trail: Default::default(),
                audio_sync_target: None,
            },
        );
        if let (Some(metadata), Some(window)) = (
            self.minimized_window_metadata.get(&x11_win),
            self.windows.get_mut(&x11_win),
        ) {
            metadata.apply_to(window);
        }

        if ordinary_add && self.ripple_on_open {
            self.ripple_active.push(RippleState {
                x11_win,
                start: std::time::Instant::now(),
            });
        }
        self.needs_render = true;
        self.finish_add_window_minimize_lifecycle(x11_win, disposition, x, y, w, h);
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

    fn finish_add_window_minimize_lifecycle(
        &mut self,
        x11_win: u32,
        disposition: AddWindowMinimizeDisposition,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) {
        match disposition {
            AddWindowMinimizeDisposition::TrackNormally => {}
            AddWindowMinimizeDisposition::SettlePendingMinimize => {
                self.settle_late_minimized_window(x11_win);
            }
            AddWindowMinimizeDisposition::StartExplicitRestore => {
                // Consume only after add_window has a usable live texture. If
                // pixmap import failed, add_window returned earlier and the
                // explicit intent remains available for the next retry.
                self.minimized_window_intents.remove(&x11_win);
                self.start_genie_restore(x11_win, x as f32, y as f32, w as f32, h as f32);
            }
        }
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
        self.dirty_region_tracker.resize(new_w, new_h);
        self.buffer_age_damage_history.clear();
        // The persistent last-presented texture is full-output sized. It is
        // recreated lazily from the first complete frame at the new geometry;
        // tag switches before then intentionally fall back to no animation.
        self.retire_presented_scene_snapshot();

        // Recreate blur FBOs for new screen size. The chain also backs the
        // frosted-glass UI theme, so its presence — not `blur_enabled` — is
        // what decides whether there is anything to resize.
        if !self.blur_fbos.is_empty() || self.scene_fbo.is_some() {
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
        // Per-window blur caches and the temporal scratch target follow the
        // largest blur level, so a RandR resize must recreate them lazily.
        self.clear_window_blur_caches();

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
        // Recreate the private WaterLily backdrop snapshot at the new output
        // size. It stays independent from the regular client blur cache.
        if self.waterlily_scene_fbo.is_some() {
            unsafe {
                if let Some((fbo, tex)) = self.waterlily_scene_fbo.take() {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
                self.waterlily_scene_fbo = Self::create_scene_fbo(&self.gl, new_w, new_h).ok();
            }
        }
        // Cancel in-progress transition on resize (screen geometry changed).
        // Some transition modes keep both the old- and new-scene targets alive;
        // retire the pair together so the second target cannot retain stale
        // dimensions (or leak VRAM) until the next transition.
        if let Some((fbo, tex)) = self.transition_fbo.take() {
            unsafe {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(tex);
            }
        }
        if let Some((fbo, tex)) = self.transition_new_fbo.take() {
            unsafe {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(tex);
            }
        }
        self.transition_start = None;
    }

    /// Retire a window after an ordinary UnmapNotify/DestroyNotify.
    ///
    /// Client retirement is a close, not a minimize request. It may use the
    /// close fade (and close particles), but must never target the Dock with a
    /// genie animation merely because that effect is configured.
    pub(crate) fn remove_window(&mut self, x11_win: u32) {
        self.minimized_window_metadata.remove(&x11_win);
        self.discard_minimized_visual(x11_win);
        self.retire_window(x11_win, WindowRetirement::Closed);
    }

    /// Release a live texture for a client that remains under WM ownership.
    ///
    /// Swallowing is neither a close nor a minimize: it must not create close
    /// particles/fades, and it has no retained Dock visual. Restore Composite
    /// redirection first when fullscreen direct presentation was active so a
    /// later unswallow can import the remapped window normally.
    pub(crate) fn discard_window_silently(&mut self, x11_win: u32) {
        let retry_redirect = if self.unredirected_window.take() == Some(x11_win) {
            !self.restore_unredirected_window(x11_win, "managed window was silently unmapped")
        } else {
            false
        };
        self.remove_window_immediate(x11_win);
        if retry_redirect {
            // `remove_window_immediate` clears the ordinary live marker. Keep
            // this one solely so the render loop can retry the failed protocol
            // transition before the window is mapped again.
            self.unredirected_window = Some(x11_win);
        }
    }

    /// Retire a window after an explicit WM minimize request.
    ///
    /// This is the only X11 path allowed to transfer the live texture and
    /// native pixmap resources into a genie animation. Restoring the same XID
    /// through `add_window` cancels that detached animation safely.
    pub(crate) fn minimize_window(&mut self, x11_win: u32) {
        self.ensure_minimized_snapshot_generation(x11_win);
        self.request_iconic_snapshot_recapture(x11_win);
        self.minimized_windows.insert(x11_win);
        self.pending_static_minimized_captures.remove(&x11_win);
        self.minimized_window_intents.insert(
            x11_win,
            requested_minimized_window_intent(true, true)
                .expect("a minimize request always has an intent"),
        );
        self.retire_window(x11_win, WindowRetirement::ExplicitlyMinimized);
    }

    /// Mark a restore before the backend performs any fallible geometry or
    /// metadata queries. A later MapNotify/lazy add can then finish the same
    /// request instead of being mistaken for a late minimize import.
    pub(crate) fn prepare_window_restore(&mut self, x11_win: u32) {
        // A restore supersedes any hover/adoption recapture that has not yet
        // imported pixels. The live AddWindow path owns the next texture, but
        // a pinned CPU snapshot remains the only durable source until that
        // fallible import succeeds.
        self.clear_iconic_snapshot_recapture(x11_win);
        let lifecycle_active = self.minimized_windows.contains(&x11_win);
        let previous_intent = self.minimized_window_intents.get(&x11_win).copied();
        prepare_window_restore_collections(
            &mut self.pending_static_minimized_captures,
            &mut self.pending_minimized_gpu_uploads,
            &mut self.minimized_window_intents,
            x11_win,
            lifecycle_active,
        );

        // A directly-presented fullscreen client can be restored before the
        // first compositor frame has re-redirected and captured it. In that
        // state the existing WindowTexture belongs to the old Composite
        // backing and must never be borrowed by a reverse Genie: the refresh
        // would free that GL texture underneath the animation. No minimize
        // pixels ever became visible, so cancel the pending lifecycle and keep
        // the X server's direct-presentation owner intact. The restored client
        // simply remains live/fullscreen without a synthetic animation.
        let has_detached_pixels = self.minimized_visuals.contains_key(&x11_win)
            || self
                .genie_active
                .iter()
                .any(|animation| animation.x11_win == x11_win);
        if restore_cancels_uncaptured_direct_minimize(
            self.unredirected_window == Some(x11_win),
            previous_intent,
            self.windows.contains_key(&x11_win),
            has_detached_pixels,
        ) {
            self.minimized_windows.remove(&x11_win);
            self.minimized_window_intents.remove(&x11_win);
            self.pending_static_minimized_captures.remove(&x11_win);
            self.genie_targets.remove(&x11_win);
            self.minimized_window_metadata.remove(&x11_win);
            if self
                .dock_preview
                .is_some_and(|preview| preview.x11_win == x11_win)
            {
                self.set_minimized_window_preview(None);
            }
            self.needs_render = true;
            return;
        }
        if lifecycle_active {
            // Protect a retained source while the backend queries geometry
            // and imports the remapped live pixmap. A cache insertion can
            // otherwise evict the old-but-current restore between these two
            // asynchronous lifecycle steps.
            self.touch_minimized_visual(x11_win, std::time::Instant::now());
        }
    }

    fn retire_window(&mut self, x11_win: u32, reason: WindowRetirement) {
        // X11 commonly reports UnmapNotify followed by DestroyNotify. The
        // first event already owns the closing animation; treating the second
        // as a fresh removal would spawn duplicate particles and immediately
        // tear down the texture underneath the fade.
        if self.windows.get(&x11_win).is_some_and(|wt| wt.fading_out) {
            return;
        }

        // Particles describe a close/destruction. Explicit minimization has
        // its own visual language and must not look like the client exploded.
        if reason == WindowRetirement::Closed && self.particle_effects {
            if let Some(wt) = self.windows.get(&x11_win) {
                self.spawn_particles_for_window(wt.x, wt.y, wt.w, wt.h);
            }
        }

        // Every explicit minimize retains one bounded compositor-owned visual
        // for the Dock.  The preference only controls whether that visual
        // travels through the genie mesh or is cached immediately.
        if reason == WindowRetirement::ExplicitlyMinimized {
            if let Some(wt) = self.windows.get(&x11_win) {
                let (gx, gy, gw, gh) = (wt.x as f32, wt.y as f32, wt.w as f32, wt.h as f32);
                self.start_genie_animation(
                    x11_win,
                    gx,
                    gy,
                    gw,
                    gh,
                    retirement_uses_genie(reason, self.genie_minimize),
                );
                return;
            }
            // Keep the explicit pending-minimize intent. Startup adoption and
            // compositor bootstrap can announce IconicState before the X
            // pixmap is importable; a later add_window will retain those pixels
            // statically instead of interpreting their arrival as a restore.
            return;
        }

        // If fading is enabled and the window exists, start fade-out instead of immediate remove
        if self.fading {
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                if !wt.fading_out && wt.fade_opacity > 0.0 {
                    wt.fading_out = true;
                    wt.anim_scale_target = self.window_animation_scale;
                    self.effect_tick_clock.reset();
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
        self.minimized_window_intents.remove(&x11_win);
        self.minimized_window_metadata.remove(&x11_win);
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

    pub(crate) fn update_geometry(
        &mut self,
        x11_win: u32,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        border_width: u32,
    ) {
        let expand = self.decoration_damage_margin();
        let confirm_pixmap_on_damage = self.graphics.is_gles();
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            let backing_changed =
                xcomposite_backing_changed((wt.w, wt.h), wt.x_border_width, (w, h), border_width);
            let moved = wt.x != x || wt.y != y;
            let (old_x, old_y, old_w, old_h) = (wt.x, wt.y, wt.w, wt.h);
            wt.x = x;
            wt.y = y;
            wt.x_border_width = Some(border_width);
            self.needs_render = true;

            if moved {
                // Mark old and new positions as dirty instead of full screen.
                // Expand by every compositor-owned decoration footprint.
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

            if backing_changed && w > 0 && h > 0 {
                wt.w = w;
                wt.h = h;
                // Defer the heavy pixmap recreation to the next render_frame()
                // call, so geometry bursts within a single frame are batched.
                wt.pixmap_refresh.backing_changed(confirm_pixmap_on_damage);
            }
        }
    }

    /// Recreate native pixmap imports for windows whose backing pixmap changed.
    /// Called once per frame so resize bursts are coalesced.
    /// Returns whether the batch's native synchronization completed.
    pub(super) fn refresh_pixmaps(&mut self) -> bool {
        let now = std::time::Instant::now();
        let mut refresh_wins = std::mem::take(&mut self.scratch_refresh_wins);
        refresh_wins.clear();
        refresh_wins.extend(
            self.windows
                .iter()
                .filter_map(|(&id, wt)| wt.pixmap_refresh.needs_refresh_at(now).then_some(id)),
        );
        if refresh_wins.is_empty() {
            self.scratch_refresh_wins = refresh_wins;
            return false;
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
                        wt.pixmap_refresh.refresh_failed();
                    }
                    continue;
                }
            };
            if let Err(error) = self.conn.name_window_pixmap(x11_win, pixmap) {
                // Name the window: on this path the retry backs off and the
                // window keeps presenting its previous texture, so a bare
                // "it failed" line leaves a visibly frozen window
                // unattributable. BadMatch here means the window is not
                // viewable/redirected right now (a client that unmapped
                // between the geometry change and this refresh).
                let (w, h) = self.windows.get(&win).map_or((0, 0), |wt| (wt.w, wt.h));
                log::warn!(
                    "compositor: resized NameWindowPixmap failed for 0x{x11_win:x} ({w}x{h}); \
                     keeping the previous texture until the retry succeeds: {error}"
                );
                if let Some(wt) = self.windows.get_mut(&win) {
                    wt.pixmap_refresh.refresh_failed();
                }
                continue;
            }
            new_pixmaps.push((win, pixmap));
        }

        let _ = self.conn.flush_x11();
        let native_sync_succeeded = match self.graphics.sync_x11() {
            Ok(()) => true,
            Err(error) => {
                log::warn!("compositor: resized pixmap synchronization failed: {error}");
                false
            }
        };
        if !native_sync_succeeded {
            for (win, pixmap) in new_pixmaps.drain(..) {
                let _ = self.conn.free_window_pixmap(pixmap);
                if let Some(wt) = self.windows.get_mut(&win) {
                    wt.pixmap_refresh.refresh_failed();
                }
            }
            self.scratch_refresh_wins = refresh_wins;
            self.scratch_new_pixmaps = new_pixmaps;
            return false;
        }

        // Reuse the candidate scratch vector to remember only successfully
        // refreshed PendingMinimize clients.  A manually-unredirected
        // fullscreen minimize remains live until this exact point; settling
        // before the replacement import succeeds would retain its stale TFP
        // binding, while retrying every frame would defeat the existing
        // backoff policy.
        refresh_wins.clear();
        for (win, pixmap) in new_pixmaps.drain(..) {
            let (texture, x11_win, visual, depth) = {
                let wt = &self.windows[&win];
                (
                    self.create_window_gl_texture(),
                    wt.x11_win,
                    wt.visual,
                    wt.depth,
                )
            };
            let texture = match texture {
                Ok(texture) => texture,
                Err(error) => {
                    log::warn!(
                        "compositor: resized GL texture allocation failed for 0x{x11_win:x}: {error}"
                    );
                    let _ = self.conn.free_window_pixmap(pixmap);
                    if let Some(wt) = self.windows.get_mut(&win) {
                        wt.pixmap_refresh.refresh_failed();
                    }
                    continue;
                }
            };
            match self.graphics.import_pixmap(
                &self.gl,
                texture,
                pixmap,
                visual,
                depth,
                self.hdr_enabled,
            ) {
                Ok((binding, rgba)) => {
                    // Build the replacement completely before touching the
                    // currently displayed resources. XComposite keeps an old
                    // named pixmap alive until FreePixmap, so a transient
                    // import failure can safely retain the last good frame.
                    let (old_texture, old_binding, old_pixmap) = {
                        let wt = self
                            .windows
                            .get_mut(&win)
                            .expect("tracked window disappeared");
                        let old_texture = std::mem::replace(&mut wt.gl_texture, texture);
                        let old_binding = wt.binding.replace(binding);
                        let old_pixmap = std::mem::replace(&mut wt.pixmap, pixmap);
                        wt.has_rgba = rgba;
                        wt.dirty = true;
                        wt.pixmap_refresh.refresh_succeeded();
                        (old_texture, old_binding, old_pixmap)
                    };
                    if let Some(old_binding) = old_binding {
                        self.graphics
                            .release_pixmap_binding(&self.gl, old_texture, old_binding);
                    }
                    unsafe {
                        self.gl.delete_texture(old_texture);
                    }
                    if old_pixmap != 0 {
                        let _ = self.conn.free_window_pixmap(old_pixmap);
                    }
                    if refreshed_window_settles_pending_minimize(
                        self.minimized_window_intents.get(&win).copied(),
                        true,
                    ) {
                        refresh_wins.push(win);
                    }
                }
                Err(error) => {
                    log::warn!(
                        "compositor: resized {} pixmap import failed for 0x{x11_win:x}: {error}",
                        self.graphics.api_name()
                    );
                    unsafe {
                        self.gl.delete_texture(texture);
                    }
                    let _ = self.conn.free_window_pixmap(pixmap);
                    if let Some(wt) = self.windows.get_mut(&win) {
                        wt.pixmap_refresh.refresh_failed();
                    }
                }
            }
        }

        for win in refresh_wins.iter().copied() {
            self.settle_late_minimized_window(win);
        }

        self.scratch_refresh_wins = refresh_wins;
        self.scratch_new_pixmaps = new_pixmaps;
        native_sync_succeeded
    }

    pub(crate) fn mark_damaged(&mut self, x11_win: u32) {
        let expand = self.decoration_damage_margin();
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            crate::backend::damage_diag::MARKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            wt.dirty = true;
            wt.pixmap_refresh.damaged();
            self.damage_render_pending = true;
            // Mark the window and every compositor-owned decoration dirty.
            self.damage_tracker.mark_region_dirty(
                wt.x - expand,
                wt.y - expand,
                wt.w + expand as u32 * 2,
                wt.h + expand as u32 * 2,
            );
            // Subtract damage so we get future notifications
            let subtract = if wt.damage == 0 {
                Ok(())
            } else {
                self.conn.clear_window_damage(wt.damage)
            };
            if let Err(error) = subtract {
                // A failed subtract permanently silences a NonEmpty damage
                // object: with no Subtract the server never reports the next
                // DamageNotify, so the window freezes on screen at whatever it
                // last drew while the client keeps painting. Logging is not
                // enough — resubscribe, and leave the id at 0 if even that
                // fails so the stale handle is never subtracted or freed.
                log::warn!(
                    "compositor: damage subtract failed for 0x{x11_win:x} (damage 0x{:x}); \
                     resubscribing: {error}",
                    wt.damage
                );
                let stale = std::mem::replace(&mut wt.damage, 0);
                let _ = self.conn.destroy_window_damage(stale);
                match self.conn.generate_xid() {
                    Ok(id) => match self.conn.create_window_damage(id, x11_win) {
                        Ok(()) => wt.damage = id,
                        Err(error) => log::warn!(
                            "compositor: damage resubscribe failed for 0x{x11_win:x}: {error}"
                        ),
                    },
                    // Expected once the window is already gone; remove_window
                    // then sees damage == 0 and skips the free.
                    Err(error) => log::warn!(
                        "compositor: damage XID allocation failed for 0x{x11_win:x}: {error}"
                    ),
                }
            }
        } else {
            crate::backend::damage_diag::UNTRACKED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            log::warn!("compositor: damage event for untracked window 0x{x11_win:x}");
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

        let mut changed = false;
        if let Some(wt) = self.windows.get_mut(&x11_win)
            && wt.class_name != class_name
        {
            wt.class_name = class_name.to_string();
            wt.opacity_override = opacity_override;
            wt.corner_radius_override = corner_radius_override;
            wt.is_frosted = is_frosted;
            if let Some(s) = scale {
                wt.scale = s;
            }
            if is_video_player && wt.audio_sync_target.is_none() {
                // Default audio sync at 60fps; an app notification can
                // override it later.
                wt.audio_sync_target = Some(60.0);
            }
            changed = true;
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&x11_win)
            && metadata.class_name != class_name
        {
            metadata.class_name = class_name.to_string();
            metadata.opacity_override = opacity_override;
            metadata.corner_radius_override = corner_radius_override;
            metadata.is_frosted = is_frosted;
            if let Some(s) = scale {
                metadata.scale = s;
            }
            if is_video_player && metadata.audio_sync_target.is_none() {
                metadata.audio_sync_target = Some(60.0);
            }
            changed = true;
        }
        if changed {
            self.needs_render = true;
            if is_video_player {
                log::info!(
                    "compositor: detected video player {} (0x{:x}), enabling audio sync",
                    class_name,
                    x11_win
                );
            }
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
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&x11_win) {
            metadata.is_fullscreen = fullscreen;
        }
    }

    /// Apply the EWMH compositor bypass preference.
    ///
    /// Reserved values are neutral per the specification. Marking the scene
    /// dirty makes both directions immediate: request `1` can enter direct
    /// presentation, while `2` or deletion (`0`) restores redirection.
    pub(crate) fn set_window_bypass_compositor(&mut self, x11_win: u32, value: u32) {
        let value = match value {
            1 | 2 => value as u8,
            _ => 0,
        };
        if let Some(wt) = self.windows.get_mut(&x11_win)
            && wt.bypass_compositor != value
        {
            wt.bypass_compositor = value;
            self.needs_render = true;
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&x11_win) {
            metadata.bypass_compositor = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AddWindowMinimizeDisposition, WindowRetirement, add_window_minimize_disposition,
        prepare_window_restore_collections, refreshed_window_settles_pending_minimize,
        requested_minimized_window_intent, restore_cancels_uncaptured_direct_minimize,
        retirement_uses_genie,
    };
    use crate::backend::compositor_common::minimized_thumbnail::{
        AdmissionOutcome, MinimizedSnapshot, MinimizedSnapshotCache, SnapshotGeneration,
        SnapshotRetention,
    };
    use crate::backend::x11::compositor::MinimizedWindowIntent;
    use crate::backend::x11::compositor::effects::discard_minimized_cpu_snapshot_state;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn genie_is_reserved_for_explicit_minimize() {
        assert!(!retirement_uses_genie(WindowRetirement::Closed, true));
        assert!(!retirement_uses_genie(
            WindowRetirement::ExplicitlyMinimized,
            false,
        ));
        assert!(retirement_uses_genie(
            WindowRetirement::ExplicitlyMinimized,
            true,
        ));
    }

    #[test]
    fn late_add_distinguishes_pending_minimize_from_explicit_restore() {
        let pending = requested_minimized_window_intent(true, false);
        assert_eq!(pending, Some(MinimizedWindowIntent::PendingMinimize));
        assert_eq!(
            add_window_minimize_disposition(pending),
            AddWindowMinimizeDisposition::SettlePendingMinimize
        );

        let restore = requested_minimized_window_intent(false, true);
        assert_eq!(restore, Some(MinimizedWindowIntent::ExplicitRestore));
        assert_eq!(
            add_window_minimize_disposition(restore),
            AddWindowMinimizeDisposition::StartExplicitRestore
        );
    }

    #[test]
    fn idempotent_restore_does_not_mark_a_future_add() {
        assert_eq!(requested_minimized_window_intent(false, false), None);
        assert_eq!(
            add_window_minimize_disposition(None),
            AddWindowMinimizeDisposition::TrackNormally
        );
    }

    #[test]
    fn only_a_successful_refresh_settles_a_pending_minimize() {
        let pending = Some(MinimizedWindowIntent::PendingMinimize);
        assert!(!refreshed_window_settles_pending_minimize(pending, false));
        assert!(refreshed_window_settles_pending_minimize(pending, true));
        assert!(!refreshed_window_settles_pending_minimize(
            Some(MinimizedWindowIntent::ExplicitRestore),
            true,
        ));
        assert!(!refreshed_window_settles_pending_minimize(None, true));
    }

    #[test]
    fn rapid_restore_cancels_only_an_uncaptured_direct_minimize() {
        let pending = Some(MinimizedWindowIntent::PendingMinimize);
        assert!(restore_cancels_uncaptured_direct_minimize(
            true, pending, true, false,
        ));
        assert!(!restore_cancels_uncaptured_direct_minimize(
            false, pending, true, false,
        ));
        assert!(!restore_cancels_uncaptured_direct_minimize(
            true, pending, false, false,
        ));
        assert!(!restore_cancels_uncaptured_direct_minimize(
            true, pending, true, true,
        ));
        assert!(!restore_cancels_uncaptured_direct_minimize(
            true,
            Some(MinimizedWindowIntent::ExplicitRestore),
            true,
            false,
        ));
    }

    #[test]
    fn pinned_snapshot_survives_prepare_and_failed_import_until_live_restore_starts() {
        let window = 42_u32;
        let generation = SnapshotGeneration::new(7).unwrap();
        let mut generations = HashMap::from([(window, generation)]);
        let mut snapshots = MinimizedSnapshotCache::new();
        let snapshot = MinimizedSnapshot::try_new(1, 1, generation.get(), true, vec![7; 4])
            .expect("valid CPU snapshot");
        assert!(matches!(
            snapshots.admit(window, snapshot, SnapshotRetention::RecapturableMapped),
            AdmissionOutcome::Admitted { evicted } if evicted.is_empty()
        ));
        snapshots
            .reserve_iconic_snapshot(&window, generation)
            .expect("current CPU snapshot can be pinned");

        let mut pending_captures = HashSet::from([window]);
        let mut pending_uploads = HashSet::from([window]);
        let mut intents = HashMap::from([(window, MinimizedWindowIntent::PendingMinimize)]);
        prepare_window_restore_collections(
            &mut pending_captures,
            &mut pending_uploads,
            &mut intents,
            window,
            true,
        );

        assert!(!pending_captures.contains(&window));
        assert!(!pending_uploads.contains(&window));
        assert_eq!(
            intents.get(&window),
            Some(&MinimizedWindowIntent::ExplicitRestore)
        );
        assert!(snapshots.has_iconic_snapshot_reservation(&window, generation));

        // Every fallible add/import branch returns before start_genie_restore,
        // so a simulated failure performs no snapshot retirement.
        assert!(snapshots.has_iconic_snapshot_reservation(&window, generation));
        assert!(generations.contains_key(&window));

        // start_genie_restore reaches this transaction only after add_window
        // has installed a usable live WindowTexture.
        assert!(discard_minimized_cpu_snapshot_state(
            &mut pending_uploads,
            &mut snapshots,
            &mut generations,
            window,
        ));
        assert!(!pending_uploads.contains(&window));
        assert!(snapshots.peek(&window).is_none());
        assert!(!generations.contains_key(&window));
    }
}
