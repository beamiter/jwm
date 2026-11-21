//! Conservative, owned provenance for one freshly imported opaque native root.
//! A receipt only proves sampling; only the KMS submission batch may present it.
use crate::backend::common_define::WindowId;
use crate::backend::wayland::state::JwmWaylandState;
use crate::backend::wayland_udev::compositor::{FboPresentationCandidate, FboPresentationReceipt};
use crate::sync_ext::MutexExt;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{RendererSurfaceStateUserData, import_surface_tree};
use smithay::backend::renderer::{Renderer, Texture, buffer_has_alpha};
use smithay::reexports::wayland_server::{
    Resource,
    protocol::{wl_output, wl_surface::WlSurface},
};
use smithay::utils::{Rectangle, Transform};
use smithay::wayland::compositor::{SurfaceAttributes, get_children, with_states};
use smithay::wayland::presentation::{
    PresentationFeedbackCachedState, PresentationFeedbackCallback,
};
use smithay::wayland::shell::xdg::SurfaceCachedState;
use smithay::wayland::viewporter::ViewportCachedState;
use std::time::{Duration, Instant};

const MAX_RETRY_AGE: Duration = Duration::from_secs(2);
const RETRY_INTERVAL: Duration = Duration::from_millis(32);

/// One live KMS output and its slice of the desktop FBO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputRoute {
    pub(crate) generation: u64,
    pub(crate) desktop_rect: (i32, i32, u32, u32),
}

impl OutputRoute {
    /// Translate a nonempty, fully contained rectangle to output-local pixels.
    pub(crate) fn local_rect(
        self,
        desktop_rect: (i32, i32, u32, u32),
    ) -> Option<(i32, i32, u32, u32)> {
        rect_within(desktop_rect, self.desktop_rect)
    }
}

fn rect_within(
    rect: (i32, i32, u32, u32),
    bounds: (i32, i32, u32, u32),
) -> Option<(i32, i32, u32, u32)> {
    let (x, y, width, height) = rect;
    let (bound_x, bound_y, bound_width, bound_height) = bounds;
    if width == 0 || height == 0 {
        return None;
    }
    let x = x.checked_sub(bound_x).filter(|x| *x >= 0)?;
    let y = y.checked_sub(bound_y).filter(|y| *y >= 0)?;
    let right = x.checked_add(i32::try_from(width).ok()?)?;
    let bottom = y.checked_add(i32::try_from(height).ok()?)?;
    (right as u32 <= bound_width && bottom as u32 <= bound_height).then_some((x, y, width, height))
}

pub(crate) struct Prepared {
    candidate: FboPresentationCandidate,
    surface: WlSurface,
    generation: u64,
    callbacks: Vec<PresentationFeedbackCallback>,
    expires: Instant,
    retry_at: Instant,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        for callback in self.callbacks.drain(..) {
            callback.discarded();
        }
    }
}
impl Prepared {
    pub(crate) fn candidate(&self) -> FboPresentationCandidate {
        self.candidate
    }
    fn live(&self, state: &JwmWaylandState) -> bool {
        let win = WindowId::from_raw(self.candidate.window);
        self.surface.is_alive()
            && !state.session_locked
            && state.presentation_generation == Some(self.generation)
            && state.mapped_windows.contains(&win)
            && state.surface_for_window(win).as_ref() == Some(&self.surface)
            && state.surface_commit_epoch(win) == Some(self.candidate.token)
    }

    pub(crate) fn may_retry(&self, state: &JwmWaylandState, now: Instant) -> bool {
        now < self.expires && self.live(state)
    }

    pub(crate) fn next_wakeup(&self, can_present: bool) -> Instant {
        if can_present {
            self.retry_at.min(self.expires)
        } else {
            self.expires
        }
    }

    pub(crate) fn defer(mut self, state: &JwmWaylandState, retry_draw: bool) -> Option<Self> {
        let now = Instant::now();
        if !self.may_retry(state, now) {
            return None;
        }
        self.retry_at = if retry_draw {
            now + RETRY_INTERVAL
        } else {
            self.expires
        };
        Some(self)
    }

    pub(crate) fn resume(
        mut self,
        state: &JwmWaylandState,
        renderer: &mut GlesRenderer,
        scene: &[(u64, i32, i32, u32, u32)],
    ) -> Option<Self> {
        let &[(window, x, y, _, _)] = scene else {
            return None;
        };
        let (_, _, w, h) = self.candidate.rect;
        if !self.may_retry(state, Instant::now())
            || window != self.candidate.window
            || !get_children(&self.surface).is_empty()
            || surface_texture(renderer, &self.surface)?
                != (self.candidate.texture, self.candidate.texture_size, (w, h))
        {
            return None;
        }
        // Use the new draw position after layout animation. The committed
        // buffer and surface extents remain fixed across retries.
        self.candidate.rect = (x, y, w, h);
        Some(self)
    }
    pub(crate) fn finish(
        self,
        receipt: FboPresentationReceipt,
        state: &JwmWaylandState,
    ) -> Option<Proof> {
        if !self.live(state)
            || receipt.candidate_token != self.candidate.token
            || receipt.window != self.candidate.window
            || receipt.texture != self.candidate.texture
            || receipt.source_rect != self.candidate.rect
            || rect_within(receipt.visible_rect, receipt.source_rect).is_none()
        {
            return None;
        }
        Some(Proof {
            prepared: self,
            receipt,
            output: None,
        })
    }
}

pub(crate) struct Proof {
    prepared: Prepared,
    receipt: FboPresentationReceipt,
    output: Option<OutputRoute>,
}
impl Proof {
    /// Full window quad drawn in desktop FBO pixels.
    pub(crate) fn source_rect(&self) -> (i32, i32, u32, u32) {
        self.receipt.source_rect
    }

    /// Interior actually sampled by the compositor receipt, in desktop FBO pixels.
    pub(crate) fn target_rect(&self) -> (i32, i32, u32, u32) {
        self.receipt.visible_rect
    }

    pub(crate) fn bind_output(mut self, output: OutputRoute) -> Option<Self> {
        output.local_rect(self.source_rect())?;
        self.output = Some(output);
        Some(self)
    }

    pub(crate) fn output(&self) -> Option<OutputRoute> {
        self.output
    }

    pub(crate) fn consume(
        mut self,
        state: &JwmWaylandState,
        output_texture: u32,
        allocation_generation: u64,
        content_serial: Option<u64>,
    ) -> Vec<PresentationFeedbackCallback> {
        if self.prepared.live(state)
            && self.receipt.output_texture == output_texture
            && self.receipt.allocation_generation == allocation_generation
            && Some(self.receipt.content_serial) == content_serial
        {
            std::mem::take(&mut self.prepared.callbacks)
        } else {
            Vec::new()
        }
    }
}

/// Uses the actual renderer import and its texture, not a caller-supplied success flag.
/// Call on the dispatch thread; no client dispatch may occur before the draw.
pub(crate) fn prepare(
    state: &JwmWaylandState,
    renderer: &mut GlesRenderer,
    scene: &[(u64, i32, i32, u32, u32)],
) -> Option<Prepared> {
    let &[(window, x, y, _, _)] = scene else {
        return None;
    };
    let win = WindowId::from_raw(window);
    if state.session_locked
        || !state.toplevels.contains_key(&win)
        || state.x11_surfaces.contains_key(&win)
        || !state.mapped_windows.contains(&win)
    {
        return None;
    }
    let generation = state.presentation_generation?;
    let token = state.surface_commit_epoch(win)?;
    if token == 0 || token == u64::MAX {
        return None;
    }
    let surface = state.surface_for_window(win)?;
    if !surface.is_alive() || !get_children(&surface).is_empty() {
        return None;
    }
    let (simple, geometry) = with_states(&surface, |states| {
        let mut attrs = states.cached_state.get::<SurfaceAttributes>();
        let attrs = attrs.current();
        let mut viewport = states.cached_state.get::<ViewportCachedState>();
        let viewport = viewport.current();
        let mut shell = states.cached_state.get::<SurfaceCachedState>();
        let geometry = shell.current().geometry;
        let simple = attrs.buffer_scale > 0
            && attrs.buffer_transform == wl_output::Transform::Normal
            && attrs
                .buffer_delta
                .is_none_or(|delta| delta.x == 0 && delta.y == 0)
            && viewport.src.is_none()
            && viewport.dst.is_none();
        (simple, geometry)
    });
    let has_feedback = with_states(&surface, |states| {
        let mut cached = states.cached_state.get::<PresentationFeedbackCachedState>();
        !cached.current().callbacks.is_empty()
    });
    if !simple || !has_feedback {
        return None;
    }
    let (texture, texture_size, (width, height)) = surface_texture(renderer, &surface)?;
    if geometry.is_some_and(|r| {
        r.loc.x != 0 || r.loc.y != 0 || r.size.w != width as i32 || r.size.h != height as i32
    }) {
        return None;
    }
    let callbacks = with_states(&surface, |states| {
        let mut cached = states.cached_state.get::<PresentationFeedbackCachedState>();
        std::mem::take(&mut cached.current().callbacks)
    });
    log::debug!(target: "jwm::kms::presentation",
        "fbo_feedback_prepared window={window} commit={token} buffer={texture_size:?} surface={width}x{height}");
    let now = Instant::now();
    Some(Prepared {
        candidate: FboPresentationCandidate {
            token,
            window,
            texture,
            texture_size,
            rect: (x, y, width, height),
        },
        surface,
        generation,
        callbacks,
        expires: now + MAX_RETRY_AGE,
        retry_at: now,
    })
}

fn surface_texture(
    renderer: &mut GlesRenderer,
    surface: &WlSurface,
) -> Option<(u32, (u32, u32), (u32, u32))> {
    import_surface_tree(renderer, surface).ok()?;
    with_states(surface, |states| {
        let data = states.data_map.get::<RendererSurfaceStateUserData>()?;
        let data = data.lock_safe();
        if buffer_has_alpha(&**data.buffer()?).unwrap_or(true)
            || data.buffer_transform() != Transform::Normal
        {
            return None;
        }
        let texture = data.texture::<GlesTexture>(renderer.context_id())?;
        let size = texture.size();
        let scale = u32::try_from(data.buffer_scale()).ok()?;
        let view = data.view()?;
        let (w, h) = (
            u32::try_from(view.dst.w).ok()?,
            u32::try_from(view.dst.h).ok()?,
        );
        let buffer_size = (u32::try_from(size.w).ok()?, u32::try_from(size.h).ok()?);
        // The entire opaque committed attachment must contribute to its
        // surface. A high-DPI buffer still draws at the WM's physical extent.
        if scale == 0
            || w == 0
            || h == 0
            || view.offset != (0, 0).into()
            || view.src != Rectangle::from_size(view.dst.to_f64())
            || buffer_size != (w.checked_mul(scale)?, h.checked_mul(scale)?)
        {
            return None;
        }
        Some((texture.tex_id(), buffer_size, (w, h)))
    })
}

#[cfg(test)]
mod tests {
    use super::OutputRoute;

    #[test]
    fn fbo_provenance_output_route_contains_only_nonempty_checked_rectangles() {
        let output = OutputRoute {
            generation: 1,
            desktop_rect: (40, 20, 30, 25),
        };
        assert_eq!(output.local_rect((40, 20, 30, 25)), Some((0, 0, 30, 25)));
        assert_eq!(output.local_rect((69, 44, 1, 1)), Some((29, 24, 1, 1)));
        for rect in [
            (39, 21, 2, 1),
            (41, 19, 1, 2),
            (69, 44, 2, 1),
            (69, 44, 1, 2),
            (40, 20, 0, 1),
            (40, 20, 1, 0),
            (i32::MIN, 20, 1, 1),
            (40, i32::MIN, 1, 1),
            (41, 20, i32::MAX as u32, 1),
            (40, 21, 1, i32::MAX as u32),
        ] {
            assert!(output.local_rect(rect).is_none(), "{rect:?}");
        }
    }
}
