//! Internal experiment: receipts are produced only by real viewer presentation.
use super::*;
use crate::remote::wayland_capture::interactive_popup::{
    PopupPresentationBinding, PopupPresentationCandidate,
};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
pub(super) struct Live {
    state: Mutex<LiveState>,
}
struct LiveState {
    generation: u64,
    alive: bool,
}
impl Default for LiveState {
    fn default() -> Self {
        Self {
            generation: 0,
            alive: true,
        }
    }
}
impl Live {
    pub(super) fn invalidate(&self) {
        if let Ok(mut state) = self.state.lock() {
            match state.generation.checked_add(1) {
                Some(next) => state.generation = next,
                None => state.alive = false,
            }
        }
    }
    pub(super) fn end(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.alive = false;
        }
    }
    fn ticket(self: &Arc<Self>) -> RemoteResult<PopupPresentationValidity> {
        let state = self
            .state
            .lock()
            .map_err(|_| "popup viewer liveness poisoned")?;
        if !state.alive {
            return Err("popup viewer no longer live".into());
        }
        Ok(PopupPresentationValidity {
            live: self.clone(),
            generation: state.generation,
        })
    }
}
pub(crate) struct PopupPresentationValidity {
    live: Arc<Live>,
    generation: u64,
}
pub(crate) struct PopupPresentationGuard<'a> {
    _guard: MutexGuard<'a, LiveState>,
}
impl PopupPresentationValidity {
    pub(crate) fn lock_current(&self) -> RemoteResult<PopupPresentationGuard<'_>> {
        let state = self
            .live
            .state
            .lock()
            .map_err(|_| "popup viewer liveness poisoned")?;
        if !state.alive || state.generation != self.generation {
            return Err("popup presentation invalidated after receipt".into());
        }
        Ok(PopupPresentationGuard { _guard: state })
    }
}
pub(crate) struct PopupPresentationReceipt {
    binding: PopupPresentationBinding,
    validity: PopupPresentationValidity,
    viewer: Arc<()>,
    presented_at: Instant,
    token: u64,
    generation: u64,
    scope: (u64, u64),
}
impl PopupPresentationReceipt {
    pub(crate) fn into_parts(
        self,
    ) -> (
        PopupPresentationBinding,
        Arc<()>,
        Instant,
        PopupPresentationValidity,
    ) {
        (self.binding, self.viewer, self.presented_at, self.validity)
    }
}
struct Submitted {
    validity: PopupPresentationValidity,
    binding: PopupPresentationBinding,
    sequence: u64,
    token: u64,
    generation: u64,
    scope: (u64, u64),
}
pub(super) struct Bridge {
    viewer: Arc<()>,
    submitted: Option<Submitted>,
    completed: Option<PopupPresentationReceipt>,
}
impl Default for Bridge {
    fn default() -> Self {
        Self {
            viewer: Arc::new(()),
            submitted: None,
            completed: None,
        }
    }
}
impl Bridge {
    pub(super) fn occupied(&self) -> bool {
        self.submitted.is_some() || self.completed.is_some()
    }
    pub(super) fn observe_presented(
        &mut self,
        sequences: &[u64],
        token: u64,
        generation: u64,
        scope: Option<(u64, u64)>,
    ) -> RemoteResult<()> {
        let Some(submitted) = self.submitted.as_ref() else {
            return Ok(());
        };
        if sequences.is_empty() {
            return Ok(());
        }
        if !matches_completion(
            sequences,
            submitted.sequence,
            token == submitted.token,
            generation == submitted.generation,
            scope == Some(submitted.scope),
        ) {
            self.submitted = None;
            return Err("popup presentation does not match submitted capture".into());
        }
        let submitted = self.submitted.take().unwrap();
        {
            let _valid = submitted.validity.lock_current()?;
        }
        self.completed = Some(PopupPresentationReceipt {
            binding: submitted.binding,
            validity: submitted.validity,
            viewer: self.viewer.clone(),
            presented_at: Instant::now(),
            token: submitted.token,
            generation: submitted.generation,
            scope: submitted.scope,
        });
        Ok(())
    }
}
fn matches_completion(
    sequences: &[u64],
    sequence: u64,
    token: bool,
    generation: bool,
    scope: bool,
) -> bool {
    sequences == [sequence] && token && generation && scope
}
impl WaylandViewer {
    pub(crate) fn draw_popup_capture(
        &mut self,
        candidate: PopupPresentationCandidate,
    ) -> RemoteResult<bool> {
        if self.popup_presentation.occupied() {
            return Err("popup presentation already pending".into());
        }
        let (frame, binding) = candidate.into_parts();
        let proof = binding.proof();
        let sequence = proof.capture_sequence;
        if frame.image.dimensions() != proof.root_size
            || (
                u32::from(frame.source_width),
                u32::from(frame.source_height),
            ) != proof.root_size
        {
            return Err("popup capture image does not match its native proof".into());
        }
        let decoded = super::super::frame::RecyclableDecodedFrame::new(
            sequence,
            frame.source_width,
            frame.source_height,
            frame.image,
            super::super::frame::new_decode_buffer_pool(),
        )
        .with_geometry_epoch(frame.geometry_epoch);
        if !self.draw_recyclable(decoded)? {
            return Ok(false);
        }
        let pending = self
            .state
            .pending
            .as_ref()
            .ok_or("popup draw did not commit")?;
        self.popup_presentation.submitted = Some(Submitted {
            validity: self.state.popup_live.ticket()?,
            binding,
            sequence,
            token: pending.token,
            generation: pending.generation,
            scope: (pending.source_epoch, pending.geometry_epoch),
        });
        Ok(true)
    }
    /// Process queued compositor events before handing an already-taken receipt
    /// to the capture adapter. Dispatch and send are separately synchronized;
    /// the adapter must also hold the receipt's live guard through native flush.
    pub(crate) fn refresh_popup_presentation(
        &mut self,
        receipt: PopupPresentationReceipt,
    ) -> RemoteResult<PopupPresentationReceipt> {
        self.pump()?;
        if self.state.closed || !Arc::ptr_eq(&receipt.viewer, &self.popup_presentation.viewer) {
            return Err("popup receipt belongs to an unavailable viewer".into());
        }
        {
            let _guard = receipt.validity.lock_current()?;
        }
        Ok(receipt)
    }
    pub(crate) fn take_presented_popup_frame(
        &mut self,
    ) -> RemoteResult<Option<PopupPresentationReceipt>> {
        if self.state.closed {
            return Err("popup viewer closed".into());
        }
        self.take_presented_sequences()?;
        if self.state.closed {
            return Err("popup viewer closed".into());
        }
        let receipt = self.popup_presentation.completed.take();
        if receipt.as_ref().is_some_and(|r| {
            r.token != self.token
                || r.generation != self.state.generation
                || self.state.scope != Some(r.scope)
        }) {
            return Err("popup presentation became obsolete".into());
        }
        if let Some(receipt) = &receipt {
            let _valid = receipt.validity.lock_current()?;
        }
        Ok(receipt)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_ticket_is_invalid_after_change_end_or_counter_exhaustion() {
        let live = Arc::new(Live::default());
        let first = live.ticket().unwrap();
        assert!(first.lock_current().is_ok());
        live.invalidate();
        assert!(first.lock_current().is_err());
        let next = live.ticket().unwrap();
        live.end();
        assert!(next.lock_current().is_err());
        assert!(live.ticket().is_err());
        let exhausted = Arc::new(Live::default());
        exhausted.state.lock().unwrap().generation = u64::MAX;
        let last = exhausted.ticket().unwrap();
        exhausted.invalidate();
        assert!(last.lock_current().is_err());
        assert!(exhausted.ticket().is_err());
    }
    #[test]
    fn send_guard_holds_the_same_invalidation_mutex() {
        let live = Arc::new(Live::default());
        let ticket = live.ticket().unwrap();
        let guard = ticket.lock_current().unwrap();
        assert!(live.state.try_lock().is_err());
        drop(guard);
        live.invalidate();
        assert!(ticket.lock_current().is_err());
    }
    #[test]
    fn popup_receipt_requires_exact_single_completion_and_all_scope_checks() {
        assert!(matches_completion(&[0], 0, true, true, true));
        assert!(!matches_completion(&[], 0, true, true, true));
        assert!(!matches_completion(&[1], 0, true, true, true));
        assert!(!matches_completion(&[0, 0], 0, true, true, true));
        for flags in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            assert!(!matches_completion(&[0], 0, flags.0, flags.1, flags.2));
        }
    }
}

impl Drop for WaylandViewer {
    fn drop(&mut self) {
        self.state.popup_live.end();
    }
}
