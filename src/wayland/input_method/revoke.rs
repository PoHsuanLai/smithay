//! Ending the seat's input method, see [`InputMethodHandle::revoke`].

use wayland_server::{Resource, backend::ClientId};

use crate::{input::SeatHandler, wayland::gate::Standing};

use super::{InputMethodHandle, InputMethodUserData};

impl InputMethodHandle {
    /// Take back the power to type for the people from `client`, which had been given it.
    ///
    /// When `client` owns the seat's input method, the seat forgets it, so that another input method
    /// can take its place, it is sent `unavailable`, and its requests are ignored from now on. Its
    /// popup is dismissed and the keyboard grab it holds, if any, is released. Returns whether
    /// there was an input method of `client` to end; any other input method is left alone.
    ///
    /// Input methods the client still has on hold are not touched; settle them with
    /// [`InputMethodManagerState::release`](super::InputMethodManagerState::release).
    pub fn revoke<D>(&self, state: &mut D, client: &ClientId) -> bool
    where
        D: SeatHandler + 'static,
    {
        let (instance, popup, grab) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(instance) = inner.instance.take_if(|instance| {
                instance
                    .object
                    .client()
                    .is_some_and(|owner| owner.id() == *client)
            }) else {
                return false;
            };
            let popup = inner.popup_handle.surface.take();
            let grab = inner.keyboard_grab.inner.lock().unwrap().grab.take();
            (instance, popup, grab)
        };

        // The callbacks run without the handle locked, as the compositor may call back into it.
        if let Some(data) = instance.object.data::<InputMethodUserData<D>>() {
            data.standing.set(Standing::Refused);
            if let Some(mut popup) = popup {
                if popup.get_parent().is_some() {
                    (data.dismiss_popup)(state, popup.clone());
                }
                popup.set_parent(None);
            }
            if grab.is_some() {
                data.keyboard_handle.unset_grab(state);
            }
        }
        instance.object.unavailable();
        true
    }
}
