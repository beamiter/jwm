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
use smithay::wayland::compositor::{SurfaceAttributes, get_children, with_states};
use smithay::wayland::presentation::{
    PresentationFeedbackCachedState, PresentationFeedbackCallback,
};
use smithay::wayland::shell::xdg::SurfaceCachedState;
use smithay::wayland::viewporter::ViewportCachedState;

pub(crate) struct Prepared {
    candidate: FboPresentationCandidate,
    surface: WlSurface,
    generation: u64,
    callbacks: Vec<PresentationFeedbackCallback>,
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
    pub(crate) fn finish(
        self,
        receipt: FboPresentationReceipt,
        state: &JwmWaylandState,
    ) -> Option<Proof> {
        if !self.live(state)
            || receipt.candidate_token != self.candidate.token
            || receipt.window != self.candidate.window
            || receipt.texture != self.candidate.texture
        {
            return None;
        }
        Some(Proof {
            prepared: self,
            receipt,
        })
    }
}

pub(crate) struct Proof {
    prepared: Prepared,
    receipt: FboPresentationReceipt,
}
impl Proof {
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
    let &[(window, x, y, width, height)] = scene else {
        return None;
    };
    let win = WindowId::from_raw(window);
    if state.session_locked
        || !state.toplevels.contains_key(&win)
        || state.x11_surfaces.contains_key(&win)
        || !state.mapped_windows.contains(&win)
        || width == 0
        || height == 0
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
    let simple = with_states(&surface, |states| {
        let mut attrs = states.cached_state.get::<SurfaceAttributes>();
        let attrs = attrs.current();
        let mut viewport = states.cached_state.get::<ViewportCachedState>();
        let viewport = viewport.current();
        let mut shell = states.cached_state.get::<SurfaceCachedState>();
        let geometry = shell.current().geometry;
        attrs.buffer_scale == 1
            && attrs.buffer_transform == wl_output::Transform::Normal
            && attrs
                .buffer_delta
                .is_none_or(|delta| delta.x == 0 && delta.y == 0)
            && viewport.src.is_none()
            && viewport.dst.is_none()
            && geometry.is_none_or(|r| {
                r.loc.x == 0
                    && r.loc.y == 0
                    && r.size.w == width as i32
                    && r.size.h == height as i32
            })
    });
    let has_feedback = with_states(&surface, |states| {
        let mut cached = states.cached_state.get::<PresentationFeedbackCachedState>();
        !cached.current().callbacks.is_empty()
    });
    if !simple || !has_feedback || import_surface_tree(renderer, &surface).is_err() {
        return None;
    }
    let texture = with_states(&surface, |states| {
        let data = states.data_map.get::<RendererSurfaceStateUserData>()?;
        let data = data.lock_safe();
        if buffer_has_alpha(&**data.buffer()?).unwrap_or(true) {
            return None;
        }
        let texture = data.texture::<GlesTexture>(renderer.context_id())?;
        let size = texture.size();
        (size.w == width as i32 && size.h == height as i32).then(|| texture.tex_id())
    })?;
    let callbacks = with_states(&surface, |states| {
        let mut cached = states.cached_state.get::<PresentationFeedbackCachedState>();
        std::mem::take(&mut cached.current().callbacks)
    });
    Some(Prepared {
        candidate: FboPresentationCandidate {
            token,
            window,
            texture,
            rect: (x, y, width, height),
        },
        surface,
        generation,
        callbacks,
    })
}
