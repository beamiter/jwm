use crate::backend::api::CompositorRect;
use crate::backend::compositor_common::effects::MotionTrail;
use crate::backend::compositor_common::genie::{GenieDirection, PreviewDirection};
use crate::backend::compositor_common::minimized_thumbnail::{
    MinimizedSnapshot, SnapshotGeneration, ThumbnailPurpose, ThumbnailSource,
    preferred_thumbnail_source,
};
use crate::backend::x11::compositor::{WallpaperMode, WobblyState};
pub(super) use crate::backend::x11::compositor_common::effects::{
    Particle, ParticleSystem, RippleState,
};
pub(super) use crate::backend::x11::compositor_common::expose::{ExposeEntry, SnapPreview};
use std::cell::Cell;

/// A backdrop-blur result owned by one X11 window.
///
/// Blur windows can appear more than once in the same scene. Sharing one cache
/// (or one temporal history texture) between them makes each window consume the
/// result produced for a different below-scene. Keep the result and its cache
/// keys tied to the consumer window instead.
pub(super) struct WindowBlurCache {
    pub(super) fbo: glow::Framebuffer,
    pub(super) texture: glow::Texture,
    pub(super) below_hash: Cell<u64>,
    pub(super) blur_levels: Cell<usize>,
    pub(super) valid: Cell<bool>,
}

pub(super) enum PixmapBinding {
    Glx { drawable: x11::glx::GLXPixmap },
    Egl { image: *mut std::ffi::c_void },
}

pub(super) fn xcomposite_backing_changed(
    old_size: (u32, u32),
    old_border_width: Option<u32>,
    new_size: (u32, u32),
    new_border_width: u32,
) -> bool {
    old_size != new_size || old_border_width != Some(new_border_width)
}

/// Resize-driven native pixmap replacement state.
///
/// XComposite allocates a new backing pixmap whenever a window is resized.
/// The first Damage can race the ConfigureNotify-driven import, so remember
/// whether that Damage has been observed. Failed imports retry when either
/// enough producer Damage arrives or a wall-clock deadline expires. The
/// deadline is required for clients that stop drawing after their resize;
/// exponential backoff avoids rebuilding an EGLImage every compositor frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PixmapRefreshState {
    pending: bool,
    confirm_on_damage: bool,
    retry_after_damages: Option<u8>,
    retry_deadline: Option<std::time::Instant>,
    damage_backoff: u8,
    time_backoff: std::time::Duration,
}

impl PixmapRefreshState {
    const INITIAL_DAMAGE_BACKOFF: u8 = 1;
    const MAX_DAMAGE_BACKOFF: u8 = 16;
    const INITIAL_TIME_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);
    const MAX_TIME_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1600);

    pub(super) fn backing_changed(&mut self, confirm_on_damage: bool) {
        self.pending = true;
        self.confirm_on_damage = confirm_on_damage;
        self.retry_after_damages = None;
        self.retry_deadline = None;
        self.damage_backoff = Self::INITIAL_DAMAGE_BACKOFF;
        self.time_backoff = Self::INITIAL_TIME_BACKOFF;
    }

    pub(super) fn needs_refresh_at(self, now: std::time::Instant) -> bool {
        self.pending || self.retry_deadline.is_some_and(|deadline| deadline <= now)
    }

    /// Fold a Damage event into an already-pending refresh when it arrived
    /// before rendering, or schedule one confirmation import when it arrived
    /// after the ConfigureNotify-driven import. During failure backoff, Damage
    /// can bring the retry forward without being required for recovery.
    pub(super) fn damaged(&mut self) {
        if self.confirm_on_damage {
            self.pending = true;
            self.confirm_on_damage = false;
            self.retry_after_damages = None;
            self.retry_deadline = None;
        } else if let Some(remaining) = self.retry_after_damages.as_mut() {
            *remaining = remaining.saturating_sub(1);
            if *remaining == 0 {
                self.pending = true;
                self.retry_after_damages = None;
                self.retry_deadline = None;
            }
        }
    }

    pub(super) fn refresh_succeeded(&mut self) {
        self.pending = false;
        self.retry_after_damages = None;
        self.retry_deadline = None;
        self.damage_backoff = Self::INITIAL_DAMAGE_BACKOFF;
        self.time_backoff = Self::INITIAL_TIME_BACKOFF;
    }

    /// Preserve the current texture and schedule another candidate. Producer
    /// Damage may accelerate the retry, while the deadline guarantees progress
    /// for a client that remains static after resizing.
    pub(super) fn refresh_failed(&mut self) {
        self.refresh_failed_at(std::time::Instant::now());
    }

    pub(super) fn refresh_failed_at(&mut self, now: std::time::Instant) {
        self.pending = false;
        self.retry_after_damages = Some(self.damage_backoff.max(1));
        self.retry_deadline = Some(now + self.time_backoff);
        self.damage_backoff = self
            .damage_backoff
            .saturating_mul(2)
            .clamp(Self::INITIAL_DAMAGE_BACKOFF, Self::MAX_DAMAGE_BACKOFF);
        self.time_backoff = self
            .time_backoff
            .saturating_mul(2)
            .clamp(Self::INITIAL_TIME_BACKOFF, Self::MAX_TIME_BACKOFF);
    }
}

impl Default for PixmapRefreshState {
    fn default() -> Self {
        Self {
            pending: false,
            confirm_on_damage: false,
            retry_after_damages: None,
            retry_deadline: None,
            damage_backoff: Self::INITIAL_DAMAGE_BACKOFF,
            time_backoff: Self::INITIAL_TIME_BACKOFF,
        }
    }
}

pub(super) struct WindowTexture {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) w: u32,
    pub(super) h: u32,
    /// Last server-reported X border width. The initial value is unknown
    /// because add_window deliberately avoids another geometry round trip.
    pub(super) x_border_width: Option<u32>,
    pub(super) damage: u32,
    pub(super) pixmap: u32,
    /// X11 window format is immutable for the lifetime of a window. Cache it
    /// so resize-driven pixmap recreation needs no GetAttributes/GetGeometry.
    pub(super) visual: u32,
    pub(super) depth: u8,
    pub(super) binding: Option<PixmapBinding>,
    pub(super) gl_texture: glow::Texture,
    pub(super) dirty: bool,
    pub(super) has_rgba: bool,
    pub(super) pixmap_refresh: PixmapRefreshState,
    pub(super) x11_win: u32,
    pub(super) fade_opacity: f32,
    pub(super) fading_out: bool,
    pub(super) class_name: String,
    pub(super) opacity_override: Option<f32>,
    pub(super) is_fullscreen: bool,
    /// Normalized `_NET_WM_BYPASS_COMPOSITOR`: 0 = neutral, 1 = prefer
    /// direct presentation, 2 = require composition.
    pub(super) bypass_compositor: u8,
    pub(super) corner_radius_override: Option<f32>,
    pub(super) scale: f32,
    pub(super) frame_extents: [u32; 4],
    pub(super) is_shaped: bool,
    pub(super) anim_scale: f32,
    pub(super) anim_scale_target: f32,
    pub(super) is_urgent: bool,
    pub(super) is_pip: bool,
    pub(super) is_frosted: bool,
    pub(super) is_override_redirect: bool,
    pub(super) wobbly: Option<WobblyState>,
    pub(super) motion_trail: MotionTrail,
    pub(super) audio_sync_target: Option<f32>,
}

/// Resource-free visual state retained while an X11 client's pixmap owners
/// are detached from the compositor.
///
/// This deliberately excludes geometry, animation progress, Damage/Pixmap
/// handles, and GL objects. It is small enough to keep for a hidden client
/// after Dock eligibility is withdrawn, then reapply to the next static import
/// or real restore.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct WindowVisualMetadata {
    pub(super) class_name: String,
    pub(super) opacity_override: Option<f32>,
    pub(super) is_fullscreen: bool,
    pub(super) bypass_compositor: u8,
    pub(super) corner_radius_override: Option<f32>,
    pub(super) scale: f32,
    pub(super) frame_extents: [u32; 4],
    pub(super) is_shaped: bool,
    pub(super) is_urgent: bool,
    pub(super) is_pip: bool,
    pub(super) is_frosted: bool,
    pub(super) is_override_redirect: bool,
    pub(super) audio_sync_target: Option<f32>,
}

impl Default for WindowVisualMetadata {
    fn default() -> Self {
        Self {
            class_name: String::new(),
            opacity_override: None,
            is_fullscreen: false,
            bypass_compositor: 0,
            corner_radius_override: None,
            scale: 1.0,
            frame_extents: [0; 4],
            is_shaped: false,
            is_urgent: false,
            is_pip: false,
            is_frosted: false,
            is_override_redirect: false,
            audio_sync_target: None,
        }
    }
}

impl From<&WindowTexture> for WindowVisualMetadata {
    fn from(window: &WindowTexture) -> Self {
        Self {
            class_name: window.class_name.clone(),
            opacity_override: window.opacity_override,
            is_fullscreen: window.is_fullscreen,
            bypass_compositor: window.bypass_compositor,
            corner_radius_override: window.corner_radius_override,
            scale: window.scale,
            frame_extents: window.frame_extents,
            is_shaped: window.is_shaped,
            is_urgent: window.is_urgent,
            is_pip: window.is_pip,
            is_frosted: window.is_frosted,
            is_override_redirect: window.is_override_redirect,
            audio_sync_target: window.audio_sync_target,
        }
    }
}

impl WindowVisualMetadata {
    pub(super) fn apply_to(&self, window: &mut WindowTexture) {
        window.class_name.clone_from(&self.class_name);
        window.opacity_override = self.opacity_override;
        window.is_fullscreen = self.is_fullscreen;
        window.bypass_compositor = self.bypass_compositor;
        window.corner_radius_override = self.corner_radius_override;
        window.scale = self.scale;
        window.frame_extents = self.frame_extents;
        window.is_shaped = self.is_shaped;
        window.is_urgent = self.is_urgent;
        window.is_pip = self.is_pip;
        window.is_frosted = self.is_frosted;
        window.is_override_redirect = self.is_override_redirect;
        window.audio_sync_target = self.audio_sync_target;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MinimizedThumbnailAvailability, PixmapRefreshState, SnapshotDrawCoordinates,
        SnapshotTextureStorage, next_snapshot_generation, select_minimized_thumbnail_source,
        snapshot_texture_uv_rect, xcomposite_backing_changed,
    };
    use crate::backend::compositor_common::minimized_thumbnail::{
        ThumbnailPurpose, ThumbnailSource,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn xcomposite_backing_tracks_size_and_x_border_changes() {
        assert!(xcomposite_backing_changed(
            (800, 600),
            Some(0),
            (801, 600),
            0
        ));
        assert!(xcomposite_backing_changed(
            (800, 600),
            Some(0),
            (800, 600),
            1
        ));
        assert!(xcomposite_backing_changed((800, 600), None, (800, 600), 0));
        assert!(!xcomposite_backing_changed(
            (800, 600),
            Some(1),
            (800, 600),
            1
        ));
    }

    #[test]
    fn damage_before_the_first_refresh_is_folded_into_that_attempt() {
        let now = Instant::now();
        let mut state = PixmapRefreshState::default();
        state.backing_changed(true);
        state.damaged();
        assert!(state.needs_refresh_at(now));

        state.refresh_succeeded();
        assert!(!state.needs_refresh_at(now));
        state.damaged();
        assert!(
            !state.needs_refresh_at(now),
            "pre-import Damage must not cause a redundant second import"
        );
    }

    #[test]
    fn damage_after_the_first_refresh_confirms_the_new_backing_once() {
        let now = Instant::now();
        let mut state = PixmapRefreshState::default();
        state.backing_changed(true);
        state.refresh_succeeded();
        assert!(!state.needs_refresh_at(now));

        state.damaged();
        assert!(state.needs_refresh_at(now));
        state.refresh_succeeded();
        state.damaged();
        assert!(
            !state.needs_refresh_at(now),
            "ordinary steady-state Damage must not rename the pixmap"
        );
    }

    #[test]
    fn failed_refreshes_retry_with_capped_damage_or_time_backoff() {
        let start = Instant::now();
        let mut state = PixmapRefreshState::default();
        state.backing_changed(true);
        state.damaged();

        for (expected_damages, expected_millis) in [
            (1, 100),
            (2, 200),
            (4, 400),
            (8, 800),
            (16, 1600),
            (16, 1600),
        ] {
            state.refresh_failed_at(start);
            assert!(!state.needs_refresh_at(start + Duration::from_millis(expected_millis - 1)));
            assert!(state.needs_refresh_at(start + Duration::from_millis(expected_millis)));

            for damage in 1..=expected_damages {
                state.damaged();
                assert_eq!(
                    state.needs_refresh_at(start),
                    damage == expected_damages,
                    "retry interval diverged at backoff {expected_damages}"
                );
            }
        }
    }

    #[test]
    fn a_new_resize_resets_both_retry_backoffs() {
        let start = Instant::now();
        let mut state = PixmapRefreshState::default();
        state.backing_changed(true);
        state.damaged();
        for _ in 0..5 {
            state.refresh_failed_at(start);
            while !state.needs_refresh_at(start) {
                state.damaged();
            }
        }

        state.backing_changed(true);
        state.refresh_failed_at(start);
        assert!(!state.needs_refresh_at(start + Duration::from_millis(99)));
        assert!(state.needs_refresh_at(start + Duration::from_millis(100)));

        state.backing_changed(true);
        state.refresh_failed_at(start);
        state.damaged();
        assert!(state.needs_refresh_at(start));
    }

    #[test]
    fn a_static_client_retries_after_the_deadline() {
        let start = Instant::now();
        let mut state = PixmapRefreshState::default();
        state.backing_changed(true);
        state.damaged();
        state.refresh_failed_at(start);

        assert!(!state.needs_refresh_at(start + Duration::from_millis(99)));
        assert!(state.needs_refresh_at(start + Duration::from_millis(100)));
    }

    #[test]
    fn success_clears_a_scheduled_retry() {
        let start = Instant::now();
        let mut state = PixmapRefreshState::default();
        state.backing_changed(true);
        state.damaged();
        state.refresh_failed_at(start);
        state.refresh_succeeded();

        assert!(!state.needs_refresh_at(start + Duration::from_secs(10)));
    }

    #[test]
    fn confirmation_can_be_disabled_for_the_glx_path() {
        let now = Instant::now();
        let mut state = PixmapRefreshState::default();
        state.backing_changed(false);
        state.refresh_succeeded();
        state.damaged();
        assert!(
            !state.needs_refresh_at(now),
            "GLX must not pay for the EGL resize confirmation workaround"
        );
    }

    #[test]
    fn local_snapshot_generation_never_emits_zero_even_after_wrap() {
        let mut clock = u64::MAX - 1;
        assert_eq!(next_snapshot_generation(&mut clock).get(), u64::MAX);
        assert_eq!(next_snapshot_generation(&mut clock).get(), 1);
    }

    #[test]
    fn texture_orientation_matches_cpu_upload_and_fbo_storage_contracts() {
        fn rendered_top_to_bottom(
            storage_bottom_to_top: [&'static str; 2],
            uv_rect: [f32; 4],
            coordinates: SnapshotDrawCoordinates,
        ) -> [&'static str; 2] {
            let vertex_zero_to_one = if uv_rect[3] < 0.0 {
                [storage_bottom_to_top[1], storage_bottom_to_top[0]]
            } else {
                storage_bottom_to_top
            };
            match coordinates {
                SnapshotDrawCoordinates::TopDownQuad => vertex_zero_to_one,
                SnapshotDrawCoordinates::BottomUpFace => {
                    [vertex_zero_to_one[1], vertex_zero_to_one[0]]
                }
            }
        }

        for coordinates in [
            SnapshotDrawCoordinates::TopDownQuad,
            SnapshotDrawCoordinates::BottomUpFace,
        ] {
            // Top-left CPU row zero is uploaded into OpenGL's bottom storage
            // row, while an FBO attachment keeps the visual bottom there.
            for (storage, rows) in [
                (SnapshotTextureStorage::CpuTopLeftUpload, ["top", "bottom"]),
                (
                    SnapshotTextureStorage::FramebufferAttachment,
                    ["bottom", "top"],
                ),
            ] {
                let uv_rect = snapshot_texture_uv_rect(storage, coordinates);
                assert_eq!(
                    rendered_top_to_bottom(rows, uv_rect, coordinates),
                    ["top", "bottom"],
                    "orientation diverged for {storage:?} on {coordinates:?}"
                );
            }
        }
    }

    #[test]
    fn x11_consumers_keep_static_hover_and_restore_quality_contracts_distinct() {
        let all = MinimizedThumbnailAvailability {
            live: true,
            retained: true,
            gpu: true,
            cpu: true,
        };
        assert_eq!(
            select_minimized_thumbnail_source(ThumbnailPurpose::StaticDockCard, all),
            Some(ThumbnailSource::GpuSnapshot)
        );
        assert_eq!(
            select_minimized_thumbnail_source(ThumbnailPurpose::HoverPreview, all),
            Some(ThumbnailSource::RetainedVisual)
        );
        assert_eq!(
            select_minimized_thumbnail_source(
                ThumbnailPurpose::RestoreAnimation,
                MinimizedThumbnailAvailability {
                    gpu: true,
                    cpu: true,
                    ..Default::default()
                }
            ),
            None
        );
    }
}

pub(super) struct WindowUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) dim: Option<glow::UniformLocation>,
    pub(super) desat: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) ripple_progress: Option<glow::UniformLocation>,
    pub(super) ripple_amplitude: Option<glow::UniformLocation>,
}

pub(super) struct ShadowUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) shadow_color: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) spread: Option<glow::UniformLocation>,
}

pub(super) struct BlurUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) halfpixel: Option<glow::UniformLocation>,
}

pub(super) struct BlurFboLevel {
    pub(super) fbo: glow::Framebuffer,
    pub(super) texture: glow::Texture,
    pub(super) w: u32,
    pub(super) h: u32,
}

pub(super) struct TransitionUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
}

pub(super) struct PortalUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) progress: Option<glow::UniformLocation>,
    pub(super) glow: Option<glow::UniformLocation>,
    pub(super) center: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
}

pub(super) struct MonitorWallpaper {
    pub(super) mon_x: i32,
    pub(super) mon_y: i32,
    pub(super) mon_w: u32,
    pub(super) mon_h: u32,
    pub(super) texture: Option<glow::Texture>,
    pub(super) mode: WallpaperMode,
    pub(super) img_w: u32,
    pub(super) img_h: u32,
    pub(super) current_path: String,
}

pub(super) struct BorderUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) border_color: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) radius_top: Option<glow::UniformLocation>,
    pub(super) border_width: Option<glow::UniformLocation>,
}

pub(super) struct GradientBorderUniforms {
    pub(super) radius_top: Option<glow::UniformLocation>,
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) color_a: Option<glow::UniformLocation>,
    pub(super) color_b: Option<glow::UniformLocation>,
    pub(super) gradient_angle: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) border_width: Option<glow::UniformLocation>,
}

pub(super) struct PostprocessUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) color_temp: Option<glow::UniformLocation>,
    pub(super) saturation: Option<glow::UniformLocation>,
    pub(super) brightness: Option<glow::UniformLocation>,
    pub(super) contrast: Option<glow::UniformLocation>,
    pub(super) invert: Option<glow::UniformLocation>,
    pub(super) grayscale: Option<glow::UniformLocation>,
    pub(super) hdr_enabled: Option<glow::UniformLocation>,
    pub(super) hdr_peak_nits: Option<glow::UniformLocation>,
    pub(super) tone_mapping_method: Option<glow::UniformLocation>,
    pub(super) eotf_mode: Option<glow::UniformLocation>,
    pub(super) output_colorspace: Option<glow::UniformLocation>,
}

pub(super) struct HudUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) bg_color: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
}

/// Uniforms of the frosted-glass surface program used by every self-drawn
/// panel when `appearance.ui_theme = "glass"`.
pub(super) struct GlassUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) backdrop: Option<glow::UniformLocation>,
    pub(super) screen_size: Option<glow::UniformLocation>,
    pub(super) tint: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) radius_top: Option<glow::UniformLocation>,
    pub(super) corner_exp: Option<glow::UniformLocation>,
    pub(super) saturation: Option<glow::UniformLocation>,
    pub(super) luminance: Option<glow::UniformLocation>,
    pub(super) bevel_width: Option<glow::UniformLocation>,
    pub(super) refraction: Option<glow::UniformLocation>,
    pub(super) rim_width: Option<glow::UniformLocation>,
    pub(super) rim_intensity: Option<glow::UniformLocation>,
    pub(super) rim_tint: Option<glow::UniformLocation>,
    pub(super) sheen: Option<glow::UniformLocation>,
    pub(super) edge_shade: Option<glow::UniformLocation>,
    pub(super) grain: Option<glow::UniformLocation>,
    pub(super) alpha: Option<glow::UniformLocation>,
}

pub(super) struct HudTextUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
}

pub(super) struct LineUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) color: Option<glow::UniformLocation>,
}

pub(super) struct OverviewEntry {
    pub(super) x11_win: u32,
    pub(super) target_w: f32,
    pub(super) target_h: f32,
    pub(super) is_selected: bool,
    pub(super) snapshot_texture: Option<glow::Texture>,
    pub(super) title: String,
    pub(super) title_texture: Option<(glow::Texture, u32, u32)>,
    pub(super) face_index: usize,
}

pub(super) struct EdgeGlowUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) glow_color: Option<glow::UniformLocation>,
    pub(super) glow_width: Option<glow::UniformLocation>,
    pub(super) mouse: Option<glow::UniformLocation>,
    pub(super) screen_size: Option<glow::UniformLocation>,
    pub(super) time: Option<glow::UniformLocation>,
}

pub(super) struct TiltUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) dim: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) tilt: Option<glow::UniformLocation>,
    pub(super) perspective: Option<glow::UniformLocation>,
    pub(super) grid_size: Option<glow::UniformLocation>,
    pub(super) light_dir: Option<glow::UniformLocation>,
}

pub(super) struct WobblyUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) dim: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) grid_offsets: Option<glow::UniformLocation>,
    pub(super) grid_n: Option<glow::UniformLocation>,
}

pub(super) struct OverviewBgUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) angle: Option<glow::UniformLocation>,
    pub(super) time: Option<glow::UniformLocation>,
    pub(super) ground: Option<glow::UniformLocation>,
    pub(super) accent: Option<glow::UniformLocation>,
}

pub(super) struct OverviewFaceUniforms {
    pub(super) mvp: Option<glow::UniformLocation>,
    pub(super) model: Option<glow::UniformLocation>,
    pub(super) aspect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) camera: Option<glow::UniformLocation>,
    pub(super) accent: Option<glow::UniformLocation>,
    pub(super) brightness: Option<glow::UniformLocation>,
    pub(super) alpha: Option<glow::UniformLocation>,
    pub(super) desat: Option<glow::UniformLocation>,
    pub(super) reflect: Option<glow::UniformLocation>,
    pub(super) glass: Option<glow::UniformLocation>,
    pub(super) edge: Option<glow::UniformLocation>,
    pub(super) time: Option<glow::UniformLocation>,
}

pub(super) struct OverviewCapUniforms {
    pub(super) mvp: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) y: Option<glow::UniformLocation>,
    pub(super) sides: Option<glow::UniformLocation>,
    pub(super) color: Option<glow::UniformLocation>,
    pub(super) accent: Option<glow::UniformLocation>,
    pub(super) time: Option<glow::UniformLocation>,
    pub(super) reflect: Option<glow::UniformLocation>,
}

pub(super) struct MagnifierUniforms {
    pub(super) magnifier_enabled: Option<glow::UniformLocation>,
    pub(super) magnifier_center: Option<glow::UniformLocation>,
    pub(super) magnifier_radius: Option<glow::UniformLocation>,
    pub(super) magnifier_zoom: Option<glow::UniformLocation>,
    pub(super) colorblind_mode: Option<glow::UniformLocation>,
}

pub(super) struct WaterlilyUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) scene_texture: Option<glow::UniformLocation>,
    pub(super) scene_available: Option<glow::UniformLocation>,
    pub(super) screen_size: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
}

/// Uniforms of the volumetric WaterLily ray-marcher (version-2 frames).
pub(super) struct WaterlilyVolumeUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) volume: Option<glow::UniformLocation>,
    pub(super) occupancy: Option<glow::UniformLocation>,
    pub(super) scene_texture: Option<glow::UniformLocation>,
    pub(super) scene_available: Option<glow::UniformLocation>,
    pub(super) screen_size: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) camera_position: Option<glow::UniformLocation>,
    pub(super) camera_right: Option<glow::UniformLocation>,
    pub(super) camera_up: Option<glow::UniformLocation>,
    pub(super) camera_forward: Option<glow::UniformLocation>,
    pub(super) tan_half_fov: Option<glow::UniformLocation>,
    pub(super) box_half_extents: Option<glow::UniformLocation>,
    pub(super) time: Option<glow::UniformLocation>,
}

pub(super) struct ParticleUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) point_size: Option<glow::UniformLocation>,
}

pub(super) struct GenieUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) dim: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) progress: Option<glow::UniformLocation>,
    pub(super) dock_pos: Option<glow::UniformLocation>,
    pub(super) dock_size: Option<glow::UniformLocation>,
    pub(super) grid_size: Option<glow::UniformLocation>,
}

pub(super) struct ThumbnailDownsampleUniforms {
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) output_size: Option<glow::UniformLocation>,
    pub(super) has_alpha: Option<glow::UniformLocation>,
}

/// Where row zero lives in a texture used by a snapshot consumer.
///
/// CPU snapshots are top-left RGBA8. Uploading their first row to OpenGL puts
/// it at the texture's bottom, which matches a top-down compositor quad without
/// a UV transform. A texture kept directly from an FBO has the opposite
/// storage orientation. The consumer's own Y axis is accounted for separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SnapshotTextureStorage {
    CpuTopLeftUpload,
    FramebufferAttachment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SnapshotDrawCoordinates {
    /// Screen-space quads emit `v_uv.y == 0` at their visual top.
    TopDownQuad,
    /// Prism faces emit `v_uv.y == 0` at their geometric bottom.
    BottomUpFace,
}

pub(super) const fn snapshot_texture_uv_rect(
    storage: SnapshotTextureStorage,
    coordinates: SnapshotDrawCoordinates,
) -> [f32; 4] {
    match (storage, coordinates) {
        (SnapshotTextureStorage::CpuTopLeftUpload, SnapshotDrawCoordinates::TopDownQuad)
        | (SnapshotTextureStorage::FramebufferAttachment, SnapshotDrawCoordinates::BottomUpFace) => {
            [0.0, 0.0, 1.0, 1.0]
        }
        (SnapshotTextureStorage::FramebufferAttachment, SnapshotDrawCoordinates::TopDownQuad)
        | (SnapshotTextureStorage::CpuTopLeftUpload, SnapshotDrawCoordinates::BottomUpFace) => {
            [0.0, 1.0, 1.0, -1.0]
        }
    }
}

pub(super) const MINIMIZED_GPU_CACHE_MAX_BYTES: usize = 12 * 1024 * 1024;
pub(super) const MINIMIZED_GPU_CACHE_MAX_ENTRIES: usize = 64;

pub(super) struct MinimizedGpuSnapshot {
    pub(super) texture: glow::Texture,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) has_alpha: bool,
    pub(super) generation: SnapshotGeneration,
    pub(super) storage: SnapshotTextureStorage,
    pub(super) last_use: u64,
}

impl MinimizedGpuSnapshot {
    pub(super) const fn estimated_bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    pub(super) const fn uv_rect(&self) -> [f32; 4] {
        snapshot_texture_uv_rect(self.storage, SnapshotDrawCoordinates::TopDownQuad)
    }
}

pub(super) struct CapturedMinimizedSnapshot {
    pub(super) cpu: MinimizedSnapshot,
    pub(super) gpu: MinimizedGpuSnapshot,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct MinimizedThumbnailAvailability {
    pub(super) live: bool,
    pub(super) retained: bool,
    pub(super) gpu: bool,
    pub(super) cpu: bool,
}

pub(super) fn select_minimized_thumbnail_source(
    purpose: ThumbnailPurpose,
    available: MinimizedThumbnailAvailability,
) -> Option<ThumbnailSource> {
    preferred_thumbnail_source(
        purpose,
        [
            available.live.then_some(ThumbnailSource::LiveMappedTexture),
            available
                .retained
                .then_some(ThumbnailSource::RetainedVisual),
            available.gpu.then_some(ThumbnailSource::GpuSnapshot),
            available.cpu.then_some(ThumbnailSource::CpuSnapshot),
        ]
        .into_iter()
        .flatten(),
    )
}

pub(super) fn next_snapshot_generation(clock: &mut u64) -> SnapshotGeneration {
    *clock = clock.wrapping_add(1);
    if *clock == 0 {
        *clock = 1;
    }
    SnapshotGeneration::new(*clock).expect("snapshot generation clock skips zero")
}

pub(super) struct GenieAnimation {
    pub(super) x11_win: u32,
    pub(super) start: std::time::Instant,
    pub(super) start_progress: f32,
    pub(super) direction: GenieDirection,
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) w: f32,
    pub(super) h: f32,
    pub(super) gl_texture: glow::Texture,
    pub(super) has_rgba: bool,
    pub(super) target: CompositorRect,
    /// False when a restore animation borrows the newly-created live entry.
    pub(super) owns_resources: bool,
    pub(super) binding: Option<PixmapBinding>,
    pub(super) pixmap: u32,
    pub(super) damage: u32,
}

/// The last WM lifecycle request for a window that is still logically
/// minimized.
///
/// `minimized_windows` records the durable state used by the renderer.  This
/// intent is deliberately separate: an AddWindow that races a minimize means
/// "import these late pixels and retain them", while an AddWindow after an
/// explicit restore means "start the reverse animation".  Treating both as a
/// restore was especially visible while adopting IconicState clients at
/// startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MinimizedWindowIntent {
    PendingMinimize,
    ExplicitRestore,
}

pub(super) struct MinimizedVisual {
    pub(super) w: f32,
    pub(super) h: f32,
    pub(super) gl_texture: glow::Texture,
    pub(super) has_rgba: bool,
    pub(super) target: Option<CompositorRect>,
    pub(super) binding: Option<PixmapBinding>,
    pub(super) pixmap: u32,
    pub(super) damage: u32,
    pub(super) cached_at: std::time::Instant,
    pub(super) estimated_bytes: u64,
}

#[derive(Clone, Copy)]
pub(super) struct DockPreview {
    pub(super) x11_win: u32,
    pub(super) anchor: CompositorRect,
    pub(super) started: std::time::Instant,
    pub(super) lease_deadline: std::time::Instant,
    pub(super) start_opacity: f32,
    pub(super) start_scale: f32,
    pub(super) direction: PreviewDirection,
    pub(super) opacity: f32,
    pub(super) scale: f32,
    /// The Dock still owns this preview request, but its retained texture was
    /// evicted (or has not arrived yet).  Keep the intent without advancing
    /// the show animation or lease until a one-shot static import settles.
    pub(super) awaiting_source: bool,
}
