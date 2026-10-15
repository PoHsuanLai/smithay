//! Subsurface positions as double-buffered state of the PARENT surface.
//!
//! `wl_subsurface.set_position` says the position "takes effect whenever the state of the
//! parent surface is applied". So the position a child asks for is not part of the child's
//! own state: it waits in the parent's pending state, is cached with the parent's commit (and,
//! while the parent is itself synchronized, waits for the commit of the parent's parent), and
//! reaches the child's current [`SubsurfaceCachedState`](super::SubsurfaceCachedState) only when
//! the parent's cached state is applied.

use wayland_server::{DisplayHandle, Weak, protocol::wl_surface::WlSurface};

use crate::utils::{IsAlive, Logical, Point};

use super::{cache::Cacheable, handlers::SubsurfaceCachedState, tree::PrivateSurfaceData};

/// Something a position can be applied to when the parent's state is applied.
pub(crate) trait Placed {
    /// Make `location` the current position.
    fn place(&self, location: Point<i32, Logical>);
}

impl Placed for Weak<WlSurface> {
    fn place(&self, location: Point<i32, Logical>) {
        let Ok(surface) = self.upgrade() else {
            return;
        };
        // A destroyed child has no data left to move.
        if !surface.alive() {
            return;
        }
        PrivateSurfaceData::with_states(&surface, |states| {
            states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location = location;
        });
    }
}

/// The positions the children of one surface asked for since its last commit, oldest first.
///
/// At most one entry per child: a later `set_position` of the same child replaces the earlier one.
#[derive(Debug)]
pub(crate) struct Placements<K> {
    moves: Vec<(K, Point<i32, Logical>)>,
}

impl<K> Default for Placements<K> {
    fn default() -> Self {
        Self { moves: Vec::new() }
    }
}

impl<K: PartialEq> Placements<K> {
    /// Record the position `key` asked for, replacing an earlier ask of the same child.
    pub(crate) fn set(&mut self, key: K, location: Point<i32, Logical>) {
        self.moves.retain(|(k, _)| *k != key);
        self.moves.push((key, location));
    }
}

impl<K: Placed + Send + 'static> Cacheable for Placements<K> {
    /// The asks leave the pending state: they apply once, with this commit.
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        Self {
            moves: std::mem::take(&mut self.moves),
        }
    }

    fn merge_into(self, _into: &mut Self, _dh: &DisplayHandle) {
        for (key, location) in self.moves {
            key.place(location);
        }
    }
}

/// The placements of the children of a surface, as kept in that surface's [`MultiCache`](super::MultiCache).
pub(crate) type SubsurfacePlacements = Placements<Weak<WlSurface>>;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{utils::Serial, wayland::compositor::cache::MultiCache};

    /// A child that records every position it is given.
    #[derive(Clone, Debug)]
    struct Probe {
        name: &'static str,
        log: Arc<Mutex<Vec<(&'static str, Point<i32, Logical>)>>>,
    }

    impl PartialEq for Probe {
        fn eq(&self, other: &Self) -> bool {
            self.name == other.name
        }
    }

    impl Placed for Probe {
        fn place(&self, location: Point<i32, Logical>) {
            self.log.lock().unwrap().push((self.name, location));
        }
    }

    fn probe(name: &'static str, log: &Arc<Mutex<Vec<(&'static str, Point<i32, Logical>)>>>) -> Probe {
        Probe {
            name,
            log: log.clone(),
        }
    }

    fn pt(x: i32, y: i32) -> Point<i32, Logical> {
        (x, y).into()
    }

    fn handle() -> (wayland_server::Display<()>, DisplayHandle) {
        let display = wayland_server::Display::<()>::new().unwrap();
        let handle = display.handle();
        (display, handle)
    }

    #[test]
    fn later_ask_of_the_same_child_replaces_the_earlier() {
        let log = Arc::default();
        let mut placements = Placements::default();
        placements.set(probe("a", &log), pt(1, 1));
        placements.set(probe("b", &log), pt(2, 2));
        placements.set(probe("a", &log), pt(3, 3));
        assert_eq!(placements.moves.len(), 2);

        let (_display, dh) = handle();
        let committed = placements.commit(&dh);
        committed.merge_into(&mut Placements::default(), &dh);
        assert_eq!(*log.lock().unwrap(), vec![("b", pt(2, 2)), ("a", pt(3, 3))]);
    }

    #[test]
    fn nothing_is_placed_until_the_parent_state_is_applied() {
        let log = Arc::default();
        let (_display, dh) = handle();
        let mut cache = MultiCache::new();
        cache
            .get::<Placements<Probe>>()
            .pending()
            .set(probe("child", &log), pt(20, 20));
        assert!(log.lock().unwrap().is_empty(), "set_position alone moves nothing");

        cache.commit(Some(Serial::from(1)), &dh);
        assert!(
            log.lock().unwrap().is_empty(),
            "a committed ask waits for the apply"
        );

        cache.apply_state(Serial::from(1), &dh);
        assert_eq!(*log.lock().unwrap(), vec![("child", pt(20, 20))]);
    }

    #[test]
    fn an_ask_after_a_commit_waits_for_the_next_commit() {
        let log = Arc::default();
        let (_display, dh) = handle();
        let mut cache = MultiCache::new();
        cache
            .get::<Placements<Probe>>()
            .pending()
            .set(probe("child", &log), pt(1, 1));
        cache.commit(Some(Serial::from(1)), &dh);
        cache
            .get::<Placements<Probe>>()
            .pending()
            .set(probe("child", &log), pt(2, 2));

        cache.apply_state(Serial::from(1), &dh);
        assert_eq!(*log.lock().unwrap(), vec![("child", pt(1, 1))]);

        cache.commit(Some(Serial::from(2)), &dh);
        cache.apply_state(Serial::from(2), &dh);
        assert_eq!(
            *log.lock().unwrap(),
            vec![("child", pt(1, 1)), ("child", pt(2, 2))]
        );
    }

    #[test]
    fn an_applied_ask_is_not_placed_twice() {
        let log = Arc::default();
        let (_display, dh) = handle();
        let mut cache = MultiCache::new();
        cache
            .get::<Placements<Probe>>()
            .pending()
            .set(probe("child", &log), pt(5, 5));
        cache.commit(Some(Serial::from(1)), &dh);
        cache.commit(Some(Serial::from(2)), &dh);
        cache.apply_state(Serial::from(2), &dh);
        assert_eq!(log.lock().unwrap().len(), 1);
    }
}
