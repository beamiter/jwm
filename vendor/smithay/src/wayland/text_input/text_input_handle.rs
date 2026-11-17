use std::mem;
use std::sync::{Arc, Mutex, Weak};

use tracing::debug;
use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::{
    self, ChangeCause, ContentHint, ContentPurpose, ZwpTextInputV3,
};
use wayland_server::backend::{ClientId, ObjectId};
use wayland_server::{Resource, protocol::wl_surface::WlSurface};

use crate::input::SeatHandler;
use crate::utils::{Logical, Rectangle};
use crate::wayland::{Dispatch2, input_method::InputMethodHandle};

#[derive(Default, Debug)]
pub(crate) struct TextInput {
    instances: Vec<Instance>,
    focus: Option<WlSurface>,
    active_text_input_id: Option<ObjectId>,
    compositor_input_method: bool,
    text_authority: TextAuthority,
}

/// Opaque, single-use authority for an already enabled text-input context.
/// This neither enables an input method nor grants compositor input permissions.
#[derive(Debug)]
pub struct AuthorizedTextTarget {
    handle: Weak<Mutex<TextInput>>,
    resource: ZwpTextInputV3,
    surface: WlSurface,
    revision: u64,
}

#[derive(Debug)]
struct TextAuthority {
    revision: Option<u64>,
    input_method:
        Option<wayland_protocols_misc::zwp_input_method_v2::server::zwp_input_method_v2::ZwpInputMethodV2>,
    batch_dirty: bool,
    pending_preedit: Option<bool>,
    preedit_active: bool,
    uncertain: bool,
}
impl Default for TextAuthority {
    fn default() -> Self {
        Self {
            revision: Some(0),
            input_method: None,
            batch_dirty: false,
            pending_preedit: None,
            preedit_active: false,
            uncertain: false,
        }
    }
}
impl TextAuthority {
    fn invalidate(&mut self) {
        self.revision = self.revision.and_then(|v| v.checked_add(1));
    }
    fn begin_batch(&mut self, preedit: Option<bool>) {
        self.invalidate();
        self.batch_dirty = true;
        if preedit.is_some() {
            self.pending_preedit = preedit;
        }
    }
    fn finish_batch(&mut self, discard: bool) {
        self.invalidate();
        if discard {
            self.uncertain = true;
        } else if let Some(active) = self.pending_preedit.take() {
            self.preedit_active = active;
        }
        self.batch_dirty = false;
        self.pending_preedit = None;
    }
    fn clean(&self) -> bool {
        self.revision.is_some()
            && !self.batch_dirty
            && !self.preedit_active
            && !self.uncertain
            && self.input_method.as_ref().is_some_and(Resource::is_alive)
    }
}

impl TextInput {
    fn authorized_instance(&self, surface: &WlSurface) -> Option<&Instance> {
        if !surface.is_alive()
            || self.focus.as_ref() != Some(surface)
            || self.compositor_input_method
            || !self.text_authority.clean()
        {
            return None;
        }
        let id = self.active_text_input_id.as_ref()?;
        self.instances.iter().find(|i| {
            !i.pending_context
                && i.instance.is_alive()
                && i.instance.id() == *id
                && i.instance.id().same_client_as(&surface.id())
        })
    }
    fn matches_authorized(&self, target: &AuthorizedTextTarget) -> bool {
        self.text_authority.revision == Some(target.revision)
            && target.resource.is_alive()
            && self
                .authorized_instance(&target.surface)
                .is_some_and(|i| i.instance == target.resource)
    }
    fn with_focused_client_all_text_inputs<F>(&mut self, mut f: F)
    where
        F: FnMut(&ZwpTextInputV3, &WlSurface, u32),
    {
        if let Some(surface) = self.focus.as_ref().filter(|surface| surface.is_alive()) {
            for text_input in self.instances.iter() {
                let instance_id = text_input.instance.id();
                if instance_id.same_client_as(&surface.id()) {
                    f(&text_input.instance, surface, text_input.serial);
                }
            }
        };
    }

    fn with_active_text_input<F>(&mut self, mut f: F)
    where
        F: FnMut(&ZwpTextInputV3, &WlSurface, u32),
    {
        let active_id = match &self.active_text_input_id {
            Some(active_text_input_id) => active_text_input_id,
            None => return,
        };

        let surface = match self.focus.as_ref().filter(|surface| surface.is_alive()) {
            Some(surface) => surface,
            None => return,
        };

        let surface_id = surface.id();
        if let Some(text_input) = self
            .instances
            .iter()
            .filter(|instance| instance.instance.id().same_client_as(&surface_id))
            .find(|instance| &instance.instance.id() == active_id)
        {
            f(&text_input.instance, surface, text_input.serial);
        }
    }
}

/// Handle to text input instances
#[derive(Default, Debug, Clone)]
pub struct TextInputHandle {
    pub(crate) inner: Arc<Mutex<TextInput>>,
}

impl TextInputHandle {
    pub(super) fn add_instance(&self, instance: &ZwpTextInputV3) {
        let mut inner = self.inner.lock().unwrap();
        inner.text_authority.invalidate();
        inner.instances.push(Instance {
            instance: instance.clone(),
            serial: 0,
            pending_state: Default::default(),
            pending_context: false,
        });
    }

    fn increment_serial(&self, text_input: &ZwpTextInputV3) {
        if let Some(instance) = self
            .inner
            .lock()
            .unwrap()
            .instances
            .iter_mut()
            .find(|instance| instance.instance == *text_input)
        {
            instance.serial = instance.serial.wrapping_add(1)
        }
    }

    /// Freeze the exact active resource of the expected surface. Requires an
    /// existing real IME, a committed context and no pending/local composition.
    pub fn snapshot_authorized_text(&self, surface: &WlSurface) -> Option<AuthorizedTextTarget> {
        let inner = self.inner.lock().unwrap();
        let instance = inner.authorized_instance(surface)?;
        Some(AuthorizedTextTarget {
            handle: Arc::downgrade(&self.inner),
            resource: instance.instance.clone(),
            surface: surface.clone(),
            revision: inner.text_authority.revision?,
        })
    }

    /// Check a frozen context without enabling, switching or refreshing it.
    pub fn authorized_text_is_current(&self, target: &AuthorizedTextTarget) -> bool {
        if !target.handle.ptr_eq(&Arc::downgrade(&self.inner)) {
            return false;
        }
        self.inner.lock().unwrap().matches_authorized(target)
    }

    /// Deliver exactly one bounded UTF-8 commit to the frozen resource. Both
    /// events and authority consumption happen under one text-input lock; no
    /// focus lookup, IME callback, keyboard or clipboard fallback follows it.
    /// Success means events queued, not that the application accepted text.
    pub fn commit_authorized_text(&self, target: &AuthorizedTextTarget, text: &str) -> bool {
        if text.is_empty()
            || text.len() > 4000
            || text.contains('\0')
            || !target.handle.ptr_eq(&Arc::downgrade(&self.inner))
        {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        if !inner.matches_authorized(target) {
            return false;
        }
        let serial = inner.authorized_instance(&target.surface).unwrap().serial;
        inner.text_authority.invalidate();
        // Overflow must fail closed before producing either event.
        if inner.text_authority.revision.is_none() {
            return false;
        }
        target.resource.commit_string(Some(text.to_owned()));
        target.resource.done(serial);
        true
    }

    pub(crate) fn set_authorized_input_method(
        &self,
        method: Option<
            wayland_protocols_misc::zwp_input_method_v2::server::zwp_input_method_v2::ZwpInputMethodV2,
        >,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.text_authority.invalidate();
        inner.text_authority.input_method = method;
        // Do not declare pending/preedit state clean merely because the IME
        // changed. A partially delivered local batch cannot be reconstructed.
    }

    pub(crate) fn send_input_method_text(&self, text: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.text_authority.begin_batch(None);
        inner.with_active_text_input(|ti, _, _| ti.commit_string(Some(text.clone())));
    }
    pub(crate) fn send_input_method_preedit(&self, text: String, begin: i32, end: i32) {
        let mut inner = self.inner.lock().unwrap();
        inner.text_authority.begin_batch(Some(!text.is_empty()));
        inner.with_active_text_input(|ti, _, _| ti.preedit_string(Some(text.clone()), begin, end));
    }
    pub(crate) fn send_input_method_delete(&self, before: u32, after: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.text_authority.begin_batch(None);
        inner.with_active_text_input(|ti, _, _| ti.delete_surrounding_text(before, after));
    }

    /// Return the currently focused surface.
    pub fn focus(&self) -> Option<WlSurface> {
        self.inner.lock().unwrap().focus.clone()
    }

    /// Advance the focus for the client to `surface`.
    ///
    /// This doesn't send any 'enter' or 'leave' events.
    pub fn set_focus(&self, surface: Option<WlSurface>) {
        let mut inner = self.inner.lock().unwrap();
        inner.text_authority.invalidate();
        inner.focus = surface;
    }

    /// Send `leave` on the text-input instance for the currently focused
    /// surface.
    pub fn leave(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.text_authority.invalidate();
        // Leaving clears the active text input.
        inner.active_text_input_id = None;
        // NOTE: we implement it in a symmetrical way with `enter`.
        inner.with_focused_client_all_text_inputs(|text_input, focus, _| {
            text_input.leave(focus);
        });
    }

    /// Send `enter` on the text-input instance for the currently focused
    /// surface.
    pub fn enter(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.text_authority.invalidate();
        // NOTE: protocol states that if we have multiple text inputs enabled, `enter` must
        // be send for each of them.
        inner.with_focused_client_all_text_inputs(|text_input, focus, _| {
            text_input.enter(focus);
        });
    }

    /// Have the compositor act as the input method for this seat.
    ///
    /// While enabled, `enter` is delivered to the focused text-input even when no real
    /// `zwp_input_method_v2` is bound, so the compositor can `commit_string` into it (e.g. for
    /// remote-desktop text injection). Toggling this sends `enter`/`leave` for the current
    /// focus immediately; subsequent focus changes are handled by the keyboard focus logic.
    pub fn set_compositor_input_method(&self, active: bool) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.compositor_input_method == active {
                return;
            }
            inner.text_authority.invalidate();
            inner.compositor_input_method = active;
        }
        if active {
            self.enter();
        } else {
            self.leave();
        }
    }

    /// Whether the compositor is currently acting as the input method for this seat
    /// (see [`set_compositor_input_method`](Self::set_compositor_input_method)).
    pub fn compositor_input_method(&self) -> bool {
        self.inner.lock().unwrap().compositor_input_method
    }

    /// The `discard_state` is used when the input-method signaled that
    /// the state should be discarded and wrong serial sent.
    pub fn done(&self, discard_state: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.text_authority.finish_batch(discard_state);
        inner.with_active_text_input(|text_input, _, serial| {
            if discard_state {
                debug!("discarding text-input state due to serial");
                // Discarding is done by sending non-matching serial.
                text_input.done(0);
            } else {
                text_input.done(serial);
            }
        });
    }

    /// Access the text-input instances for the currently focused surface.
    pub fn with_focused_text_input<F>(&self, mut f: F)
    where
        F: FnMut(&ZwpTextInputV3, &WlSurface),
    {
        let mut inner = self.inner.lock().unwrap();
        inner.with_focused_client_all_text_inputs(|ti, surface, _| {
            f(ti, surface);
        });
    }

    /// Access the active text-input instance for the currently focused surface.
    pub fn with_active_text_input<F>(&self, mut f: F)
    where
        F: FnMut(&ZwpTextInputV3, &WlSurface),
    {
        let mut inner = self.inner.lock().unwrap();
        inner.with_active_text_input(|ti, surface, _| {
            f(ti, surface);
        });
    }

    /// Call the callback with the serial of the active text_input or with the passed
    /// `default` one when empty.
    pub(crate) fn active_text_input_serial_or_default<F>(&self, default: u32, mut callback: F)
    where
        F: FnMut(u32),
    {
        let mut inner = self.inner.lock().unwrap();
        let mut should_default = true;
        inner.with_active_text_input(|_, _, serial| {
            should_default = false;
            callback(serial);
        });
        if should_default {
            callback(default)
        }
    }
}

/// User data of ZwpTextInputV3 object
#[derive(Debug)]
pub struct TextInputUserData {
    pub(super) handle: TextInputHandle,
    pub(crate) input_method_handle: InputMethodHandle,
}

impl<D> Dispatch2<ZwpTextInputV3, D> for TextInputUserData
where
    D: SeatHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &wayland_server::Client,
        resource: &ZwpTextInputV3,
        request: zwp_text_input_v3::Request,
        _dhandle: &wayland_server::DisplayHandle,
        _data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        {
            let mut inner = self.handle.inner.lock().unwrap();
            inner.text_authority.invalidate();
            if let Some(instance) = inner.instances.iter_mut().find(|i| i.instance == *resource) {
                // Keep the context unavailable throughout a Commit's multiple
                // subsystem calls. Only its successful completion clears this.
                instance.pending_context = true;
            }
        }
        // Always increment serial to not desync with clients.
        if matches!(request, zwp_text_input_v3::Request::Commit) {
            self.handle.increment_serial(resource);
        }

        // Discard requests without any active input method instance, unless the compositor
        // itself is acting as the input method for this seat (the text-injection escape hatch).
        // In that case we still process enable/commit so the focused client becomes the active
        // text input and the compositor can `commit_string` into it.
        if !self.input_method_handle.has_instance() && !self.handle.compositor_input_method() {
            debug!("discarding text-input request without IME running");
            return;
        }

        let focus = match self.handle.focus() {
            Some(focus) if focus.id().same_client_as(&resource.id()) => focus,
            _ => {
                debug!("discarding text-input request for unfocused client");
                return;
            }
        };

        let mut guard = self.handle.inner.lock().unwrap();
        let pending_state = match guard.instances.iter_mut().find_map(|instance| {
            if instance.instance == *resource {
                Some(&mut instance.pending_state)
            } else {
                None
            }
        }) {
            Some(pending_state) => pending_state,
            None => {
                debug!("got request for untracked text-input");
                return;
            }
        };

        match request {
            zwp_text_input_v3::Request::Enable => {
                pending_state.enable = Some(true);
            }
            zwp_text_input_v3::Request::Disable => {
                pending_state.enable = Some(false);
            }
            zwp_text_input_v3::Request::SetSurroundingText { text, cursor, anchor } => {
                pending_state.surrounding_text = Some((text, cursor as u32, anchor as u32));
            }
            zwp_text_input_v3::Request::SetTextChangeCause { cause } => {
                // Guard against clients sending us unknown values from future versions.
                let cause = cause.into_result().unwrap_or(ChangeCause::Other);
                pending_state.text_change_cause = Some(cause);
            }
            zwp_text_input_v3::Request::SetContentType { hint, purpose } => {
                // Guard against clients sending us unknown values from future versions.
                let hint = ContentHint::from_bits_truncate(u32::from(hint));
                let purpose = purpose.into_result().unwrap_or(ContentPurpose::Normal);
                pending_state.content_type = Some((hint, purpose));
            }
            zwp_text_input_v3::Request::SetCursorRectangle { x, y, width, height } => {
                pending_state.cursor_rectangle = Some(Rectangle::new((x, y).into(), (width, height).into()));
            }
            zwp_text_input_v3::Request::Commit => {
                let mut new_state = mem::take(pending_state);
                let _ = pending_state;
                let active_text_input_id = &mut guard.active_text_input_id;

                if active_text_input_id.is_some() && *active_text_input_id != Some(resource.id()) {
                    debug!("discarding text_input request since we already have an active one");
                    return;
                }

                match new_state.enable {
                    Some(true) => {
                        *active_text_input_id = Some(resource.id());
                        // Drop the guard before calling to other subsystem.
                        drop(guard);
                        self.input_method_handle.activate_input_method(state, &focus);
                    }
                    Some(false) => {
                        *active_text_input_id = None;
                        // Drop the guard before calling to other subsystem.
                        drop(guard);
                        self.input_method_handle.deactivate_input_method(state);
                        return;
                    }
                    None => {
                        if *active_text_input_id != Some(resource.id()) {
                            debug!("discarding text_input requests before enabling it");
                            return;
                        }

                        // Drop the guard before calling to other subsystems later on.
                        drop(guard);
                    }
                }

                if let Some((text, cursor, anchor)) = new_state.surrounding_text.take() {
                    self.input_method_handle.with_instance(move |input_method| {
                        input_method.object.surrounding_text(text, cursor, anchor)
                    });
                }

                if let Some(cause) = new_state.text_change_cause.take() {
                    self.input_method_handle.with_instance(move |input_method| {
                        input_method.object.text_change_cause(cause);
                    });
                }

                if let Some((hint, purpose)) = new_state.content_type.take() {
                    self.input_method_handle.with_instance(move |input_method| {
                        input_method.object.content_type(hint, purpose);
                    });
                }

                if let Some(rect) = new_state.cursor_rectangle.take() {
                    self.input_method_handle
                        .set_text_input_rectangle::<D>(state, rect);
                }

                self.input_method_handle.with_instance(|input_method| {
                    input_method.done();
                });
                // No IME lock is held here. A concurrent snapshot cannot see
                // the new revision paired with the pre-Commit active context.
                let mut inner = self.handle.inner.lock().unwrap();
                inner.text_authority.invalidate();
                if let Some(instance) = inner.instances.iter_mut().find(|i| i.instance == *resource) {
                    instance.pending_context = false;
                }
            }
            zwp_text_input_v3::Request::Destroy => {
                // Nothing to do
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, text_input: &ZwpTextInputV3) {
        let destroyed_id = text_input.id();
        let deactivate_im = {
            let mut inner = self.handle.inner.lock().unwrap();
            inner.text_authority.invalidate();
            if inner.active_text_input_id.as_ref() == Some(&destroyed_id) {
                inner.active_text_input_id = None;
            }
            inner.instances.retain(|inst| inst.instance.id() != destroyed_id);
            let destroyed_focused = inner
                .focus
                .as_ref()
                .map(|focus| focus.id().same_client_as(&destroyed_id))
                .unwrap_or(true);

            // Deactivate IM when we either lost focus entirely or destroyed text-input for the
            // currently focused client.
            destroyed_focused
                && !inner
                    .instances
                    .iter()
                    .any(|inst| inst.instance.id().same_client_as(&destroyed_id))
        };

        if deactivate_im {
            self.input_method_handle.deactivate_input_method(state);
        }
    }
}

#[derive(Debug)]
struct Instance {
    instance: ZwpTextInputV3,
    serial: u32,
    pending_state: TextInputState,
    pending_context: bool,
}

#[derive(Debug, Default)]
struct TextInputState {
    enable: Option<bool>,
    surrounding_text: Option<(String, u32, u32)>,
    content_type: Option<(ContentHint, ContentPurpose)>,
    cursor_rectangle: Option<Rectangle<i32, Logical>>,
    text_change_cause: Option<ChangeCause>,
}

#[cfg(test)]
mod authorized_text_tests {
    use super::TextAuthority;

    #[test]
    fn authorized_text_revision_never_restores_or_wraps() {
        let mut state = TextAuthority::default();
        let initial = state.revision;
        state.begin_batch(None);
        state.finish_batch(false);
        assert_ne!(state.revision, initial);
        state.revision = Some(u64::MAX);
        state.invalidate();
        assert_eq!(state.revision, None);
        state.begin_batch(Some(false));
        state.finish_batch(false);
        assert_eq!(state.revision, None);
        assert!(!state.clean());
    }

    #[test]
    fn authorized_text_preedit_survives_done_until_explicit_empty_done() {
        let mut state = TextAuthority::default();
        state.begin_batch(Some(true));
        assert!(state.batch_dirty);
        state.finish_batch(false);
        assert!(state.preedit_active);
        assert!(!state.batch_dirty);
        state.begin_batch(None); // unrelated commit/delete is not preedit cancellation
        state.finish_batch(false);
        assert!(state.preedit_active);
        state.begin_batch(Some(false));
        assert!(state.preedit_active && state.batch_dirty);
        state.finish_batch(false);
        assert!(!state.preedit_active && !state.batch_dirty);
        assert!(!state.clean(), "no real input method is never authorized");
    }

    #[test]
    fn authorized_text_invalid_done_cannot_restore_clean_state() {
        let mut state = TextAuthority::default();
        state.begin_batch(Some(true));
        state.finish_batch(true);
        assert!(state.uncertain);
        state.begin_batch(Some(false));
        state.finish_batch(false);
        assert!(state.uncertain);
        assert!(!state.clean());
    }
}
