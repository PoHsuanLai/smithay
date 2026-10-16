//! Consent hooks for protocols that smithay serves as a whole.
//!
//! Most privileged protocols are implemented by the compositor, which can decide at the point of
//! use. A few are served entirely by smithay, with private user data, so the compositor cannot hold
//! or refuse them from the outside: the data control protocols, the input method protocol and the
//! foreign toplevel list. Each of those has a defaulted handler method that returns a [`Gate`] for
//! the requesting client, and a `release` method on its state that settles the objects that were
//! held, once the answer is known.
//!
//! The default answer is [`Gate::Open`], which is the behavior smithay has without any gate.

use std::{
    collections::HashMap,
    fmt,
    hash::Hash,
    sync::{Mutex, PoisonError},
};

/// The most objects of one protocol that are kept on hold for one client.
///
/// A client that asks for more while all of them are still held is refused.
pub const MAX_HELD_PER_CLIENT: usize = 16;

/// The compositor's answer to a client that wants to use a gated protocol.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Gate {
    /// Let the client use the protocol. This is how smithay behaves without a gate.
    #[default]
    Open,
    /// The answer is not known yet, for example because the person is being asked.
    ///
    /// The object is created but stays inert: it receives no events and its requests are ignored,
    /// except `destroy`. It is remembered, per client and per protocol, up to
    /// [`MAX_HELD_PER_CLIENT`] objects; past that the client is treated as [`Gate::Refuse`]d. The
    /// compositor settles held objects later with the `release` method of the protocol's state.
    Hold,
    /// Deny the client. The object is created and told that the protocol is unavailable to it
    /// (`finished`, `cancelled` or `unavailable`, whichever the protocol has), is registered
    /// nowhere, and ignores every later request.
    Refuse,
}

/// Whether an object created under a gate takes part in the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Registered and working as without a gate.
    Live,
    /// Inert until it is released.
    Held,
    /// Inert for good.
    Refused,
}

impl From<Gate> for Standing {
    fn from(gate: Gate) -> Self {
        match gate {
            Gate::Open => Standing::Live,
            Gate::Hold => Standing::Held,
            Gate::Refuse => Standing::Refused,
        }
    }
}

/// A [`Standing`] that can be changed through a shared reference, as user data is.
#[derive(Debug)]
pub(crate) struct StandingCell(Mutex<Standing>);

impl StandingCell {
    pub(crate) fn new(standing: Standing) -> Self {
        Self(Mutex::new(standing))
    }

    pub(crate) fn get(&self) -> Standing {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set(&self, standing: Standing) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = standing;
    }
}

/// Objects on hold, grouped by the client that created them and bounded per client.
pub(crate) struct HeldByClient<K, T> {
    by_client: HashMap<K, Vec<T>>,
}

impl<K, T> Default for HeldByClient<K, T> {
    fn default() -> Self {
        Self {
            by_client: HashMap::new(),
        }
    }
}

impl<K: fmt::Debug, T> fmt::Debug for HeldByClient<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.by_client.iter().map(|(client, held)| (client, held.len())))
            .finish()
    }
}

impl<K: Hash + Eq, T> HeldByClient<K, T> {
    /// Keep `item` on hold for `client`.
    ///
    /// Items for which `alive` is false are dropped first, so destroyed objects and vanished
    /// clients do not count. Gives `item` back when `client` already has
    /// [`MAX_HELD_PER_CLIENT`] items on hold.
    pub(crate) fn hold(&mut self, client: K, item: T, alive: impl Fn(&T) -> bool) -> Result<(), T> {
        self.by_client.retain(|_, held| {
            held.retain(&alive);
            !held.is_empty()
        });
        let held = self.by_client.entry(client).or_default();
        if held.len() >= MAX_HELD_PER_CLIENT {
            return Err(item);
        }
        held.push(item);
        Ok(())
    }

    /// Take every item on hold for `client`, oldest first.
    pub(crate) fn take(&mut self, client: &K) -> Vec<T> {
        self.by_client.remove(client).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_alive(_: &u32) -> bool {
        true
    }

    #[test]
    fn default_gate_is_open() {
        assert_eq!(Gate::default(), Gate::Open);
    }

    #[test]
    fn standing_follows_the_gate() {
        assert_eq!(Standing::from(Gate::Open), Standing::Live);
        assert_eq!(Standing::from(Gate::Hold), Standing::Held);
        assert_eq!(Standing::from(Gate::Refuse), Standing::Refused);
    }

    #[test]
    fn standing_cell_changes_through_a_shared_reference() {
        let cell = StandingCell::new(Standing::Held);
        assert_eq!(cell.get(), Standing::Held);
        cell.set(Standing::Live);
        assert_eq!(cell.get(), Standing::Live);
    }

    #[test]
    fn hold_is_bounded_per_client() {
        let mut held = HeldByClient::<u8, u32>::default();
        for n in 0..MAX_HELD_PER_CLIENT as u32 {
            assert_eq!(held.hold(1, n, all_alive), Ok(()));
        }
        assert_eq!(held.hold(1, 99, all_alive), Err(99));
        // Another client has its own allowance.
        assert_eq!(held.hold(2, 100, all_alive), Ok(()));
    }

    #[test]
    fn take_returns_oldest_first_and_empties_the_client() {
        let mut held = HeldByClient::<u8, u32>::default();
        for n in [3, 1, 2] {
            held.hold(1, n, all_alive).unwrap();
        }
        held.hold(2, 7, all_alive).unwrap();
        assert_eq!(held.take(&1), vec![3, 1, 2]);
        assert!(held.take(&1).is_empty());
        assert_eq!(held.take(&2), vec![7]);
        assert!(held.take(&3).is_empty());
    }

    #[test]
    fn dead_items_free_their_slots() {
        let mut held = HeldByClient::<u8, u32>::default();
        for n in 0..MAX_HELD_PER_CLIENT as u32 {
            held.hold(1, n, all_alive).unwrap();
        }
        // Items below 8 are gone: the next hold prunes them first.
        assert_eq!(held.hold(1, 50, |n| *n >= 8 || *n == 50), Ok(()));
        assert_eq!(held.take(&1).len(), MAX_HELD_PER_CLIENT - 8 + 1);
    }

    #[test]
    fn clients_with_only_dead_items_are_forgotten() {
        let mut held = HeldByClient::<u8, u32>::default();
        held.hold(1, 1, all_alive).unwrap();
        held.hold(2, 2, |n| *n == 2).unwrap();
        assert!(held.take(&1).is_empty());
        assert_eq!(held.take(&2), vec![2]);
    }
}
