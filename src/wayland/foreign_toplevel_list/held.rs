//! Lists held back by [`ForeignToplevelListHandler::foreign_toplevel_list_gate`](super::ForeignToplevelListHandler::foreign_toplevel_list_gate).

use std::fmt;

use wayland_protocols::ext::foreign_toplevel_list::v1::server::ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1;
use wayland_server::{Client, DisplayHandle, Resource, backend::ClientId};

use crate::wayland::gate::Gate;

use super::ForeignToplevelListState;

/// Announces the toplevels to a held list, as a bind would have.
type OpenList = Box<dyn FnOnce(&mut ForeignToplevelListState, &DisplayHandle, &Client) + Send>;

/// A list that was bound while its client was on hold.
pub(super) struct HeldList {
    pub(super) instance: ExtForeignToplevelListV1,
    pub(super) open: OpenList,
}

impl fmt::Debug for HeldList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeldList")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl HeldList {
    /// Whether the list still exists.
    fn is_alive(&self) -> bool {
        self.instance.is_alive()
    }

    /// Tell the client that the list is finished, for good.
    pub(super) fn refuse(self) {
        self.instance.finished();
    }
}

impl ForeignToplevelListState {
    /// Keep `held` for `client`; gives it back when the client has too many on hold already.
    pub(super) fn hold(&mut self, client: ClientId, held: HeldList) -> Result<(), HeldList> {
        self.held.hold(client, held, HeldList::is_alive)
    }

    /// Settle the lists that `client` bound while it was on hold.
    ///
    /// With [`Gate::Open`] each list is sent every current toplevel, as on bind, and then receives
    /// the toplevels that come later. With [`Gate::Refuse`] each list is sent `finished`.
    /// [`Gate::Hold`] does nothing. Lists that were destroyed, or whose client is gone, are dropped
    /// silently.
    pub fn release(&mut self, dh: &DisplayHandle, client: &ClientId, gate: Gate) {
        if gate == Gate::Hold {
            return;
        }
        for held in self.held.take(client).into_iter().filter(HeldList::is_alive) {
            match gate {
                Gate::Open => {
                    if let Some(client) = held.instance.client() {
                        (held.open)(self, dh, &client);
                    }
                }
                Gate::Refuse => held.refuse(),
                Gate::Hold => {}
            }
        }
    }
}
