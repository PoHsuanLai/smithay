//! Ending the lists of a client, see [`ForeignToplevelListState::revoke`].

use wayland_server::{Resource, backend::ClientId};

use super::{ForeignToplevelListState, ForeignToplevelWeakHandle, held::HeldList};

impl ForeignToplevelListState {
    /// Take back the power to see the windows from `client`, which had been given it.
    ///
    /// Every list the client bound is sent `finished` and is announced no window from now on, so a
    /// new toplevel never reaches it. The window entries it already received are sent `closed` and
    /// no longer get title or app id updates. Lists the client still has on hold are sent
    /// `finished` as [`ForeignToplevelListState::release`] with [`Gate::Refuse`](crate::wayland::Gate::Refuse)
    /// would. The client can bind the list again; that asks
    /// [`ForeignToplevelListHandler::foreign_toplevel_list_gate`](super::ForeignToplevelListHandler::foreign_toplevel_list_gate) afresh.
    pub fn revoke(&mut self, client: &ClientId) {
        for held in self.held.take(client).into_iter().filter(HeldList::is_alive) {
            held.refuse();
        }

        let (ended, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.list_instances)
            .into_iter()
            .filter(|list| list.is_alive())
            .partition(|list| list.client().is_some_and(|owner| owner.id() == *client));
        self.list_instances = kept;
        for list in &ended {
            list.finished();
        }

        for handle in self
            .toplevels
            .iter()
            .filter_map(ForeignToplevelWeakHandle::upgrade)
        {
            let theirs = handle
                .resources()
                .into_iter()
                .filter(|entry| entry.client().is_some_and(|owner| owner.id() == *client));
            for entry in theirs {
                entry.closed();
                handle.remove_instance(&entry);
            }
        }
    }
}
