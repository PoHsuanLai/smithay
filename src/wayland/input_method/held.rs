//! Input methods held back by [`InputMethodHandler::input_method_gate`](super::InputMethodHandler::input_method_gate).

use std::sync::{Arc, Mutex, PoisonError};

use wayland_protocols_misc::zwp_input_method_v2::server::zwp_input_method_v2::ZwpInputMethodV2;
use wayland_server::{DisplayHandle, Resource, backend::ClientId};

use crate::wayland::{
    gate::{Gate, HeldByClient, Standing, StandingCell},
    text_input::TextInputHandle,
};

use super::{InputMethodHandle, InputMethodManagerState};

/// An input method that was created while its client was on hold.
#[derive(Debug)]
pub(super) struct HeldInputMethod {
    pub(super) object: ZwpInputMethodV2,
    /// The same cell as in the object's user data.
    pub(super) standing: Arc<StandingCell>,
    pub(super) handle: InputMethodHandle,
    pub(super) text_input_handle: TextInputHandle,
}

impl HeldInputMethod {
    fn is_alive(&self) -> bool {
        self.object.is_alive()
    }

    /// Tell the client that no input method is available to it, for good.
    pub(super) fn refuse(self) {
        self.standing.set(Standing::Refused);
        self.object.unavailable();
    }

    /// Make this the seat's input method, as a late-binding one is: when another input method is
    /// bound already this one is told `unavailable` instead.
    fn open(self) {
        if self.handle.add_instance(&self.object) {
            self.standing.set(Standing::Live);
            // A text input enabled before this input method was released is activated for it.
            self.text_input_handle
                .activate_late_input_method_detached(&self.handle);
        } else {
            self.standing.set(Standing::Refused);
        }
    }
}

/// The input methods on hold, shared between the state and the manager's user data, which has no
/// other way to reach the state.
#[derive(Debug, Default, Clone)]
pub(super) struct HeldInputMethods(Arc<Mutex<HeldByClient<ClientId, HeldInputMethod>>>);

impl HeldInputMethods {
    /// Keep `held` for `client`; gives it back when the client has too many on hold already.
    pub(super) fn hold(&self, client: ClientId, held: HeldInputMethod) -> Result<(), HeldInputMethod> {
        let mut by_client = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        by_client.hold(client, held, HeldInputMethod::is_alive)
    }

    fn take(&self, client: &ClientId) -> Vec<HeldInputMethod> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take(client)
    }
}

impl InputMethodManagerState {
    /// Settle the input methods that `client` created while it was on hold.
    ///
    /// With [`Gate::Open`] each becomes the seat's input method as one that binds late does: it is
    /// activated for a text input that is enabled already, or told `unavailable` when the seat
    /// has an input method. With [`Gate::Refuse`] each is told `unavailable`. [`Gate::Hold`] does
    /// nothing. Input methods that were destroyed, or whose client is gone, are dropped silently.
    ///
    /// `dh` is not needed by this protocol; it is taken for symmetry with the other gated protocols.
    pub fn release(&mut self, dh: &DisplayHandle, client: &ClientId, gate: Gate) {
        let _ = dh;
        if gate == Gate::Hold {
            return;
        }
        for held in self
            .held
            .take(client)
            .into_iter()
            .filter(HeldInputMethod::is_alive)
        {
            match gate {
                Gate::Open => held.open(),
                Gate::Refuse => held.refuse(),
                Gate::Hold => {}
            }
        }
    }
}
