use super::Compositor;
use super::CompositorConnection;
use super::OverviewEntry;
use super::math::{mat4_mul, rotate_y_matrix, scale_matrix, translate_matrix};
use super::prism::{
    MAX_PRISM_SIDES, MIN_PRISM_SIDES, PrismCamera, PrismFace, PrismKind, PrismPass,
    build_prism_pieces, mirror_matrix,
};
use super::{SnapshotDrawCoordinates, SnapshotTextureStorage, snapshot_texture_uv_rect};
use crate::backend::compositor_common::ui_theme::{self, UiPalette};
use crate::backend::compositor_font;
use glow::HasContext;

/// Share of the monitor height the front face covers.
const FACE_FILL: f32 = 0.56;
/// Where the prism's front bottom edge lands, leaving room for the reflection.
const BASE_LINE: f32 = 0.84;
/// Horizontal breathing room between a title's text and the edge of the
/// card-toned pill drawn behind it.
const TITLE_PAD_X: f32 = 10.0;
/// Vertical equivalent of [`TITLE_PAD_X`].
const TITLE_PAD_Y: f32 = 5.0;

impl<C: CompositorConnection> Compositor<C> {
    pub(super) fn clear_overview_snapshots(&mut self) {
        unsafe {
            for entry in &mut self.overview_windows {
                if let Some(texture) = entry.snapshot_texture.take() {
                    self.gl.delete_texture(texture);
                }
            }
        }
    }

    pub(super) fn clear_overview_title_textures(&mut self) {
        unsafe {
            for entry in &mut self.overview_windows {
                if let Some((texture, _, _)) = entry.title_texture.take() {
                    self.gl.delete_texture(texture);
                }
            }
        }
    }

    /// Drop every compositor-owned overview resource without playing the exit
    /// animation. Config hot-disable uses this path because no later frame may
    /// assume the disabled feature still owns snapshots or modal scene state.
    pub(super) fn clear_overview_mode_immediate(&mut self) {
        self.clear_overview_snapshots();
        self.clear_overview_title_textures();
        self.overview_windows.clear();
        self.overview_active = false;
        self.overview_closing = false;
        self.overview_opacity = 0.0;
        self.overview_entry_progress = 1.0;
        self.overview_exit_progress = 1.0;
        self.overview_prism_target_angle = 0.0;
        self.overview_prism_current_angle = 0.0;
        self.overview_prism_last_tick = None;
        self.overview_prism_sides = 4;
        self.overview_prism_spin = 0.0;
        self.overview_slide_offset = 0;
        self.overview_total_clients = 0;
        self.needs_render = true;
    }

    pub(super) fn create_overview_title_textures(&mut self) {
        let entries: Vec<(String, f32)> = self
            .overview_windows
            .iter()
            .map(|e| (e.title.clone(), e.target_w))
            .collect();

        // The same face, size and ink the tab strip paints its titles with, so
        // an overview label reads as part of the same UI rather than a
        // separate overlay with its own typography.
        let ui = ui_theme::palette();
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let size = compositor_font::ui_font_pixel_size(font);

        let textures: Vec<Option<(glow::Texture, u32, u32)>> = entries
            .iter()
            .map(|(title, target_w)| {
                let max_w = (*target_w as u32).max(120);
                let text = compositor_font::fit_ui_text(title, font, size, max_w);
                if text.is_empty() {
                    return None;
                }
                let (pixels, w, h) =
                    compositor_font::render_ui_text_to_rgba(&text, font, size, ui.title_ink);
                if w == 0 || h == 0 {
                    return None;
                }
                unsafe {
                    let tex = self.gl.create_texture().ok()?;
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    // P6C: Allocate texture storage first
                    self.gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA8 as i32,
                        w as i32,
                        h as i32,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(None), // Allocate without data
                    );
                    self.gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_MIN_FILTER,
                        glow::LINEAR as i32,
                    );
                    self.gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_MAG_FILTER,
                        glow::LINEAR as i32,
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

                    // P6C: Upload data via PBO (async, reduces CPU stall)
                    self.pbo_uploader
                        .upload_texture(&self.gl, tex, w, h, glow::RGBA, &pixels);

                    Some((tex, w, h))
                }
            })
            .collect();

        for (entry, title_tex) in self.overview_windows.iter_mut().zip(textures.into_iter()) {
            entry.title_texture = title_tex;
        }
    }

    pub(super) fn upload_overview_snapshot_texture(
        &mut self, // P6C: mut needed for pbo_uploader
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Option<glow::Texture> {
        unsafe {
            let texture = self.gl.create_texture().ok()?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            // P6C: Allocate texture storage first
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None), // Allocate without data
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
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

            // P6C: Upload data via PBO (async, reduces CPU stall)
            self.pbo_uploader
                .upload_texture(&self.gl, texture, width, height, glow::RGBA, pixels);

            Some(texture)
        }
    }

    pub(super) fn create_overview_snapshot_texture(
        &mut self, // P6C: mut needed for pbo_uploader
        x11_win: u32,
        max_size: u32,
    ) -> Option<glow::Texture> {
        let (pixels, width, height) = self.capture_window_thumbnail(x11_win, max_size)?;
        // capture_window_thumbnail already returns the shared top-left RGBA8
        // contract. Upload row zero unchanged; the ordinary top-down quad UV
        // convention samples that OpenGL bottom storage row as the visual top.
        self.upload_overview_snapshot_texture(&pixels, width, height)
    }

    pub(super) fn refresh_overview_snapshots(&mut self) {
        self.clear_overview_snapshots();

        let requests: Vec<(u32, u32)> = self
            .overview_windows
            .iter()
            .map(|entry| {
                let desired = (entry.target_w.max(entry.target_h) * 2.0).ceil() as u32;
                let max_size = desired.clamp(256, 1024);
                (entry.x11_win, max_size)
            })
            .collect();

        let snapshots: Vec<Option<glow::Texture>> = requests
            .into_iter()
            .map(|(x11_win, max_size)| self.create_overview_snapshot_texture(x11_win, max_size))
            .collect();

        for (entry, snapshot_texture) in self.overview_windows.iter_mut().zip(snapshots.into_iter())
        {
            entry.snapshot_texture = snapshot_texture;
        }
    }

    /// Tick the overview prism rotation animation (exponential ease-out).
    pub(super) fn tick_overview_prism(&mut self) {
        let now = std::time::Instant::now();
        let dt = if let Some(last) = self.overview_prism_last_tick {
            now.duration_since(last).as_secs_f32().min(0.1)
        } else {
            1.0 / 60.0
        };
        self.overview_prism_last_tick = Some(now);

        // Prism rotation animation
        let diff = self.overview_prism_target_angle - self.overview_prism_current_angle;
        let mut angular_speed = 0.0;
        if diff.abs() < 0.001 {
            self.overview_prism_current_angle = self.overview_prism_target_angle;
        } else {
            let t = 1.0 - (-20.0_f32 * dt).exp();
            let delta = diff * t;
            self.overview_prism_current_angle += delta;
            angular_speed = (delta / dt.max(1.0e-4)).abs();
            self.needs_render = true;
        }

        // Spin energy makes the cube turn see-through, pull back and tilt while
        // it rotates. It rises fast so the effect starts on the first frame of a
        // rotation, and decays slowly so the cube settles instead of snapping
        // back to opaque.
        let target_spin = (angular_speed / 7.0).clamp(0.0, 1.0);
        let rate = if target_spin > self.overview_prism_spin {
            22.0
        } else {
            7.0
        };
        self.overview_prism_spin +=
            (target_spin - self.overview_prism_spin) * (1.0 - (-rate * dt).exp());
        if self.overview_prism_spin < 0.002 {
            self.overview_prism_spin = 0.0;
        }

        // Entry animation (scale + fade in)
        if self.overview_entry_progress < 1.0 {
            let t = 1.0 - (-10.0_f32 * dt).exp();
            self.overview_entry_progress += (1.0 - self.overview_entry_progress) * t;
            if (1.0 - self.overview_entry_progress).abs() < 0.002 {
                self.overview_entry_progress = 1.0;
            }
            self.overview_opacity = self.overview_entry_progress;
            self.needs_render = true;
        }

        // Exit animation (scale + fade out)
        if self.overview_closing {
            let t = 1.0 - (-12.0_f32 * dt).exp();
            self.overview_exit_progress -= self.overview_exit_progress * t;
            self.overview_opacity = self.overview_exit_progress;
            if self.overview_exit_progress < 0.01 {
                // Animation complete: actually deactivate
                self.clear_overview_mode_immediate();
            }
            self.needs_render = true;
        }

        // The skydome twinkles and the caps sheen, so an open overview keeps
        // asking for frames even when the rotation itself has settled.
        if self.overview_active && !self.overview_windows.is_empty() {
            self.needs_render = true;
        }
    }

    /// Project a point in model space through the MVP matrix to screen coordinates.
    fn project_to_screen(
        mvp: &[f32; 16],
        model_pt: [f32; 3],
        vp_w: f32,
        vp_h: f32,
        vp_x: f32,
        vp_y: f32,
    ) -> (f32, f32) {
        let [mx, my, mz] = model_pt;
        let clip_x = mvp[0] * mx + mvp[4] * my + mvp[8] * mz + mvp[12];
        let clip_y = mvp[1] * mx + mvp[5] * my + mvp[9] * mz + mvp[13];
        let clip_w = mvp[3] * mx + mvp[7] * my + mvp[11] * mz + mvp[15];
        let ndc_x = clip_x / clip_w;
        let ndc_y = clip_y / clip_w;
        let sx = (ndc_x * 0.5 + 0.5) * vp_w + vp_x;
        let sy = (1.0 - (ndc_y * 0.5 + 0.5)) * vp_h + vp_y;
        (sx, sy)
    }

    /// Render overview overlay (Alt+Ctrl+Tab) as a Compiz-style rotating prism.
    ///
    /// The scene is built in four layers: a skydome backdrop with a horizon and
    /// a light pool, the prism mirrored into that floor, the prism itself (lit
    /// faces with beveled edges plus polygon caps), and finally the flat title
    /// labels. Rendering is confined to the monitor that owns the overview.
    pub(super) fn render_overview(&self, proj: &[f32; 16], _focused: Option<u32>) {
        if self.overview_windows.is_empty() {
            return;
        }

        let mon_x = self.overview_mon_x;
        let mon_y = self.overview_mon_y;
        let mon_w = self.overview_mon_w;
        let mon_h = self.overview_mon_h;
        if mon_w == 0 || mon_h == 0 {
            return;
        }
        let mw = mon_w as f32;
        let mh = mon_h as f32;

        // Combined scale for the entry/exit animation.
        let anim = (self.overview_entry_progress * self.overview_exit_progress).clamp(0.0, 1.0);
        let spin = self.overview_prism_spin.clamp(0.0, 1.0);
        let time = self.compositor_start_time.elapsed().as_secs_f32();
        let sides = self
            .overview_prism_sides
            .clamp(MIN_PRISM_SIDES, MAX_PRISM_SIDES);

        // === 1. Camera ===
        // Faces keep the monitor aspect. Rotating pulls the camera back and
        // tips it down a little further, which is what sells the spin.
        let camera = PrismCamera::frame(mw / mh, sides, FACE_FILL, 0.27 + 0.06 * spin, 0.18 * spin);

        // Swing the prism in on entry and back out on exit.
        let swing = (1.0 - self.overview_entry_progress.clamp(0.0, 1.0)) * 0.55
            - (1.0 - self.overview_exit_progress.clamp(0.0, 1.0)) * 0.55;
        let angle = self.overview_prism_current_angle + swing;
        let lift = camera.lift_for_base_line(BASE_LINE) * anim;
        let base_model = mat4_mul(
            &translate_matrix(0.0, lift, 0.0),
            &mat4_mul(&rotate_y_matrix(angle), &scale_matrix(anim, anim, anim)),
        );
        // The prism stands on a mirror at its own bottom edge.
        let floor_y = lift - anim;
        let ground = camera
            .project(&translate_matrix(0.0, 0.0, 0.0), [0.0, floor_y, 0.0])
            .1;

        // === 2. Assign windows to face slots ===
        // More windows than faces can only happen transiently while the visible
        // subset is being reshuffled; the selected window always wins its face.
        let mut faces = vec![PrismFace::default(); sides];
        let mut face_entries: Vec<Option<usize>> = vec![None; sides];
        for (idx, entry) in self.overview_windows.iter().enumerate() {
            let slot = entry.face_index.min(sides - 1);
            if face_entries[slot].is_some() && !entry.is_selected {
                continue;
            }
            face_entries[slot] = Some(idx);
            faces[slot] = PrismFace {
                texture: self.entry_texture(entry),
                // Prism `v_uv.y == 0` is the geometric bottom, unlike the
                // compositor's screen-space quads. Both a top-left CPU upload
                // and the live X11 fallback expose the visual top at texture
                // v=0, so traverse them in reverse on the face.
                uv_rect: snapshot_texture_uv_rect(
                    SnapshotTextureStorage::CpuTopLeftUpload,
                    SnapshotDrawCoordinates::BottomUpFace,
                ),
                accent: if entry.is_selected { 1.0 } else { 0.15 },
                desat: if entry.is_selected { 0.0 } else { 0.30 },
                brightness: if entry.is_selected { 1.0 } else { 0.82 },
                edge: 1.0,
                // Overview snapshots are already downscaled to roughly the size
                // they are drawn at, so a mipmap chain would buy nothing.
                mipmapped: false,
            };
        }
        // Empty slots are drawn as glass, which still needs a bound texture.
        let filler = faces.iter().find_map(|face| face.texture);

        let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
        unsafe {
            // === 3. Skydome backdrop ===
            // Clip to the owning monitor explicitly: earlier passes leave their
            // own damage rectangle in the scissor box.
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl
                .scissor(mon_x, scissor_gl_y, mon_w as i32, mon_h as i32);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.draw_prism_skydome(
                proj,
                [mon_x as f32, mon_y as f32, mw, mh],
                &camera,
                ground,
                angle,
                self.overview_opacity,
                time,
            );

            if anim <= 0.01 {
                self.gl.disable(glow::SCISSOR_TEST);
                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
                return;
            }

            // === 4. Viewport for the 3D passes ===
            self.gl
                .viewport(mon_x, scissor_gl_y, mon_w as i32, mon_h as i32);
            self.bind_prism_programs(&camera, time);

            // === 5. Mirrored prism, fading into the floor ===
            let mirrored = mat4_mul(&mirror_matrix(floor_y), &base_model);
            let reflection = build_prism_pieces(&camera, &mirrored);
            self.draw_prism_pass(
                &camera,
                &reflection,
                &faces,
                filler,
                &PrismPass {
                    fade: anim,
                    spin,
                    floor_y,
                    reflect: true,
                },
            );

            // === 6. The prism itself ===
            let solid = build_prism_pieces(&camera, &base_model);
            self.draw_prism_pass(
                &camera,
                &solid,
                &faces,
                filler,
                &PrismPass {
                    fade: anim,
                    spin,
                    floor_y,
                    reflect: false,
                },
            );

            // === 7. Restore viewport for the flat overlays ===
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);

            // === 8. Title labels, front to back ===
            // The selected face is marked in 3D by its accent bevel, so the
            // flat overlay only carries text.
            let vp_x = mon_x as f32;
            let vp_y = mon_y as f32;

            // Resolve the visible labels once, so the pill pass and the text
            // pass each bind their program a single time instead of switching
            // per label.
            let mut labels: Vec<(glow::Texture, f32, f32, f32, f32, f32)> = Vec::new();
            for piece in solid.iter().rev() {
                let PrismKind::Face { slot } = piece.kind else {
                    continue;
                };
                let Some(idx) = face_entries.get(slot).copied().flatten() else {
                    continue;
                };
                let Some((tex, tw, th)) = self.overview_windows[idx].title_texture else {
                    continue;
                };
                // Titles belong to the face they label: they fade out as the
                // face turns away, so a spinning cube is not covered in text.
                let title_alpha = smoothstep(0.62, 0.95, piece.facing) * anim;
                if title_alpha < 0.02 {
                    continue;
                }

                let (bcx, bcy) =
                    Self::project_to_screen(&piece.mvp, [0.0, -1.0, 0.0], mw, mh, vp_x, vp_y);
                labels.push((tex, tw as f32, th as f32, bcx, bcy, title_alpha));
            }

            if !labels.is_empty() {
                let ui = ui_theme::palette();

                // The card-toned pill the tab strip carries its cells on.
                self.gl.use_program(Some(self.border_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.border_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                for &(_, tw, th, bcx, bcy, alpha) in &labels {
                    let chip_w = tw + TITLE_PAD_X * 2.0;
                    let chip_h = th + TITLE_PAD_Y * 2.0;
                    let chip_x = bcx - chip_w * 0.5;
                    let chip_y = bcy + 10.0 - TITLE_PAD_Y;
                    self.sysui_fill_rounded(
                        chip_x,
                        chip_y,
                        chip_w,
                        chip_h,
                        chip_h * 0.5,
                        UiPalette::faded(ui.card, alpha),
                    );
                }

                self.gl.use_program(Some(self.hud_text_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_text_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                self.gl
                    .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
                for &(tex, tw, th, bcx, bcy, alpha) in &labels {
                    let title_x = bcx - tw * 0.5;
                    let title_y = bcy + 10.0;
                    self.gl.uniform_4_f32(
                        self.hud_text_uniforms.rect.as_ref(),
                        title_x,
                        title_y,
                        tw,
                        th,
                    );
                    self.gl
                        .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), alpha);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
            }

            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Texture to sample for an overview entry: its snapshot if one was taken,
    /// otherwise the live window texture.
    fn entry_texture(&self, entry: &OverviewEntry) -> Option<glow::Texture> {
        entry
            .snapshot_texture
            .or_else(|| self.windows.get(&entry.x11_win).map(|wt| wt.gl_texture))
    }
}

/// Hermite interpolation between two edges, matching the GLSL builtin.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge1 <= edge0 {
        return if x < edge0 { 0.0 } else { 1.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
