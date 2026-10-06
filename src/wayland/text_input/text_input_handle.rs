use std::mem;
use std::sync::{Arc, Mutex};

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
    /// What the active text input has committed so far (reset by `enable`), kept so an input
    /// method that binds after the field was enabled can be told it.
    applied: TextInputState,
    compositor_input_method: bool,
}

impl TextInput {
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
        inner.instances.push(Instance {
            instance: instance.clone(),
            serial: 0,
            pending_state: Default::default(),
        });
        // `enter` is "sent to each text input object of the client" whose surface has the
        // focus, so one created later gets it too (only this new object: the others have it).
        if let Some(surface) = inner.focus.as_ref().filter(|surface| surface.is_alive()) {
            if instance.id().same_client_as(&surface.id()) {
                instance.enter(surface);
            }
        }
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
            instance.serial += 1
        }
    }

    /// Return the currently focused surface.
    pub fn focus(&self) -> Option<WlSurface> {
        self.inner.lock().unwrap().focus.clone()
    }

    /// Advance the focus for the client to `surface`.
    ///
    /// This doesn't send any 'enter' or 'leave' events.
    pub fn set_focus(&self, surface: Option<WlSurface>) {
        self.inner.lock().unwrap().focus = surface;
    }

    /// Send `leave` on the text-input instance for the currently focused
    /// surface.
    pub fn leave(&self) {
        let mut inner = self.inner.lock().unwrap();
        // Leaving clears the active text input.
        inner.active_text_input_id = None;
        inner.applied = TextInputState::default();
        // NOTE: we implement it in a symmetrical way with `enter`.
        inner.with_focused_client_all_text_inputs(|text_input, focus, _| {
            text_input.leave(focus);
        });
    }

    /// Send `enter` on the text-input instance for the currently focused
    /// surface.
    pub fn enter(&self) {
        let mut inner = self.inner.lock().unwrap();
        // NOTE: protocol states that if we have multiple text inputs enabled, `enter` must
        // be send for each of them.
        inner.with_focused_client_all_text_inputs(|text_input, focus, _| {
            text_input.enter(focus);
        });
    }

    /// Have the compositor act as the input method for this seat.
    ///
    /// While enabled, the focused text input's `enable` and `commit` are processed even when no
    /// real `zwp_input_method_v2` is bound, so the compositor can `commit_string` into it (e.g.
    /// for remote-desktop text injection). `enter` and `leave` follow keyboard focus either way,
    /// so toggling this sends none.
    pub fn set_compositor_input_method(&self, active: bool) {
        self.inner.lock().unwrap().compositor_input_method = active;
    }

    /// Whether the compositor is currently acting as the input method for this seat
    /// (see [`set_compositor_input_method`](Self::set_compositor_input_method)).
    pub fn compositor_input_method(&self) -> bool {
        self.inner.lock().unwrap().compositor_input_method
    }

    /// Activate `input_method`, which has just bound, for the text input that is enabled
    /// already: `activate`, then the state it committed (surrounding text, change cause, content
    /// type, cursor rectangle), then `done`. Does nothing when no text input is enabled.
    pub(crate) fn activate_late_input_method<D: SeatHandler + 'static>(
        &self,
        input_method: &InputMethodHandle,
        state: &mut D,
    ) {
        let mut focus = None;
        let mut applied = TextInputState::default();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.with_active_text_input(|_, surface, _| focus = Some(surface.clone()));
            if focus.is_some() {
                applied = inner.applied.clone();
            }
        }
        if let Some(focus) = focus {
            input_method.activate_input_method(state, &focus);
            relay_state(input_method, state, applied);
        }
    }

    /// The `discard_state` is used when the input-method signaled that
    /// the state should be discarded and wrong serial sent.
    pub fn done(&self, discard_state: bool) {
        let mut inner = self.inner.lock().unwrap();
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
        // Always increment serial to not desync with clients.
        if matches!(request, zwp_text_input_v3::Request::Commit) {
            self.handle.increment_serial(resource);
        }

        // Requests are taken without an input method too: a field is enabled before an IME that
        // starts later is bound, and that IME is activated for it from the state kept here.
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
                let new_state = mem::take(pending_state);
                let _ = pending_state;
                let inner = &mut *guard;
                let active_text_input_id = &mut inner.active_text_input_id;

                if active_text_input_id.is_some() && *active_text_input_id != Some(resource.id()) {
                    debug!("discarding text_input request since we already have an active one");
                    return;
                }

                match new_state.enable {
                    Some(true) => {
                        *active_text_input_id = Some(resource.id());
                        // Enabling starts the state afresh: the client sends all of it again.
                        inner.applied = TextInputState::default();
                        inner.applied.merge(&new_state);
                        // Drop the guard before calling to other subsystem.
                        drop(guard);
                        self.input_method_handle.activate_input_method(state, &focus);
                    }
                    Some(false) => {
                        *active_text_input_id = None;
                        inner.applied = TextInputState::default();
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
                        inner.applied.merge(&new_state);

                        // Drop the guard before calling to other subsystems later on.
                        drop(guard);
                    }
                }

                relay_state(&self.input_method_handle, state, new_state);
            }
            zwp_text_input_v3::Request::Destroy => {
                // Nothing to do
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, text_input: &ZwpTextInputV3) {
        let destroyed_id = text_input.id();
        // The input method serves the active text input only: it is deactivated when that one is
        // destroyed (an unenabled one going away changes nothing for it).
        let deactivate_im = {
            let mut inner = self.handle.inner.lock().unwrap();
            inner.instances.retain(|inst| inst.instance.id() != destroyed_id);
            let was_active = inner.active_text_input_id.as_ref() == Some(&destroyed_id);
            if was_active {
                // A destroyed text input is disabled; it must not keep the seat's one active slot.
                inner.active_text_input_id = None;
                inner.applied = TextInputState::default();
            }
            was_active
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
}

/// Sends the input method the parts of a text input's state that are set, closed by `done`.
fn relay_state<D: SeatHandler + 'static>(
    input_method: &InputMethodHandle,
    state: &mut D,
    mut new_state: TextInputState,
) {
    if let Some((text, cursor, anchor)) = new_state.surrounding_text.take() {
        input_method.with_instance(move |input_method| {
            input_method.object.surrounding_text(text, cursor, anchor)
        });
    }

    if let Some(cause) = new_state.text_change_cause.take() {
        input_method.with_instance(move |input_method| {
            input_method.object.text_change_cause(cause);
        });
    }

    if let Some((hint, purpose)) = new_state.content_type.take() {
        input_method.with_instance(move |input_method| {
            input_method.object.content_type(hint, purpose);
        });
    }

    if let Some(rect) = new_state.cursor_rectangle.take() {
        input_method.set_text_input_rectangle::<D>(state, rect);
    }

    input_method.with_instance(|input_method| {
        input_method.done();
    });
}

#[derive(Debug, Default, Clone)]
struct TextInputState {
    enable: Option<bool>,
    surrounding_text: Option<(String, u32, u32)>,
    content_type: Option<(ContentHint, ContentPurpose)>,
    cursor_rectangle: Option<Rectangle<i32, Logical>>,
    text_change_cause: Option<ChangeCause>,
}

impl TextInputState {
    /// Overwrites each part of `self` that `newer` sets.
    fn merge(&mut self, newer: &TextInputState) {
        self.enable = newer.enable.or(self.enable);
        self.surrounding_text = newer.surrounding_text.clone().or(self.surrounding_text.take());
        self.content_type = newer.content_type.or(self.content_type);
        self.cursor_rectangle = newer.cursor_rectangle.or(self.cursor_rectangle);
        self.text_change_cause = newer.text_change_cause.or(self.text_change_cause);
    }
}
