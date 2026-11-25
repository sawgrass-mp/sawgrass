//! Dense entity slots with generations (task IM-4).
//!
//! Every networked entity takes a slot: a small index, below the number of entities alive at once, that per-entity
//! arrays (wire states, visible-set bits) are indexed by. A freed slot is handed out again, so those arrays stay as
//! small as the world. Reuse must never make one entity look like another to a viewer that saw the old one
//! (`docs/research/visibility-index`, "The precondition"), so two rules hold:
//!
//! - **A freed slot waits a full visibility pass before reuse.** [`Slots::free`] puts it in quarantine; the server
//!   calls [`Slots::release`] after every viewer's visible set has been recomputed without it, and only then can
//!   [`Slots::alloc`] return it. Every viewer that had the old entity has by then been told it left.
//! - **The id on the wire changes with every reuse.** Each slot carries a generation, bumped when it is freed, and
//!   [`Slot::wire_id`] packs both, so a client never sees the same id for two entities.

/// Slots the wire id can address: the index takes the high 16 bits.
pub const MAX_SLOTS: usize = 1 << 16;

/// One allocated slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slot {
    /// Index into per-entity arrays.
    pub index: u32,
    /// How many times this index was freed before.
    pub generation: u16,
}

impl Slot {
    /// The entity's id in snapshots and events: index in the high 16 bits, generation in the low 16. Index first,
    /// so entities listed in slot order are also in id order, which delta snapshots need, with no sort.
    #[must_use]
    pub const fn wire_id(self) -> u32 {
        (self.index << 16) | self.generation as u32
    }
}

/// Allocator of dense slots.
#[derive(Debug, Default)]
pub struct Slots {
    generations: Vec<u16>,
    live: Vec<bool>,
    free: Vec<u32>,
    quarantine: Vec<u32>,
}

impl Slots {
    /// An allocator with no slots in use.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes a free slot, the lowest-numbered first among those never used. `None` once [`MAX_SLOTS`] are in use
    /// or in quarantine.
    pub fn alloc(&mut self) -> Option<Slot> {
        let index = match self.free.pop() {
            Some(index) => index,
            None if self.generations.len() < MAX_SLOTS => {
                self.generations.push(0);
                self.live.push(false);
                (self.generations.len() - 1) as u32
            }
            None => return None,
        };
        self.live[index as usize] = true;
        Some(Slot {
            index,
            generation: self.generations[index as usize],
        })
    }

    /// Frees `slot` into quarantine. Returns false, and changes nothing, if it is not the live slot.
    pub fn free(&mut self, slot: Slot) -> bool {
        let i = slot.index as usize;
        if !self.is_live(slot) {
            return false;
        }
        self.live[i] = false;
        self.generations[i] = self.generations[i].wrapping_add(1);
        self.quarantine.push(slot.index);
        true
    }

    /// Makes the slots freed since the last call available. Call it once every visible set has been recomputed.
    pub fn release(&mut self) {
        self.free.append(&mut self.quarantine);
    }

    /// Whether `slot` is allocated with this generation.
    #[must_use]
    pub fn is_live(&self, slot: Slot) -> bool {
        let i = slot.index as usize;
        self.live.get(i) == Some(&true) && self.generations[i] == slot.generation
    }

    /// One past the highest index ever handed out: the length per-entity arrays need.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.generations.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interest::{EntityId, Point, SpatialGrid, VisibilityChanges, VisibilityConfig, VisibleSet};

    #[test]
    fn slots_stay_dense_and_reuse_waits_for_release() {
        let mut slots = Slots::new();
        let a = slots.alloc().expect("a");
        let b = slots.alloc().expect("b");
        assert_eq!((a.index, b.index), (0, 1));
        assert!(slots.free(a));
        assert!(!slots.free(a), "a slot is freed once");
        // In quarantine: a new entity gets a new index, not a's.
        let c = slots.alloc().expect("c");
        assert_eq!(c.index, 2);
        slots.release();
        let d = slots.alloc().expect("d");
        assert_eq!(d.index, a.index);
        assert_ne!(d.wire_id(), a.wire_id(), "a reused slot has a new wire id");
        // Slot order is id order.
        assert!(a.wire_id() < b.wire_id() && b.wire_id() < c.wire_id());
        assert!(!slots.is_live(a));
        assert!(slots.is_live(d));
        assert_eq!(slots.capacity(), 3);
    }

    #[test]
    fn slots_run_out_at_the_limit() {
        let mut slots = Slots::new();
        for _ in 0..MAX_SLOTS {
            assert!(slots.alloc().is_some());
        }
        assert!(slots.alloc().is_none());
    }

    /// The server's order within a tick: a leaving entity is taken out of the grid, every set is recomputed, and
    /// only then is its slot released. A viewer that saw it is told it left before a new entity can take the slot,
    /// and is told the new one entered, even though both stand on the same spot inside the hysteresis band.
    #[test]
    fn a_viewer_sees_left_then_entered_when_a_slot_is_reused() {
        let config = VisibilityConfig::default();
        let mut slots = Slots::new();
        let mut grid = SpatialGrid::default();
        let mut changes = VisibilityChanges::default();
        let viewer = slots.alloc().expect("viewer");
        let old = slots.alloc().expect("old");
        let at = Point::new(10.0, 0.0);
        grid.upsert(EntityId(viewer.index), Point::new(0.0, 0.0));
        grid.upsert(EntityId(old.index), at);
        let mut set = VisibleSet::new();
        set.update(
            &grid,
            Some(EntityId(viewer.index)),
            Point::new(0.0, 0.0),
            &config,
            &mut changes,
        );
        assert_eq!(changes.entered, vec![EntityId(old.index)]);

        // The old entity leaves; the tick recomputes the set, then releases the slot.
        grid.remove(EntityId(old.index));
        slots.free(old);
        set.update(
            &grid,
            Some(EntityId(viewer.index)),
            Point::new(0.0, 0.0),
            &config,
            &mut changes,
        );
        assert_eq!(changes.left, vec![EntityId(old.index)]);
        slots.release();

        // A new entity takes the slot at the same spot.
        let new = slots.alloc().expect("new");
        assert_eq!(new.index, old.index);
        grid.upsert(EntityId(new.index), at);
        set.update(
            &grid,
            Some(EntityId(viewer.index)),
            Point::new(0.0, 0.0),
            &config,
            &mut changes,
        );
        assert_eq!(changes.entered, vec![EntityId(new.index)]);
        assert_ne!(new.wire_id(), old.wire_id());
    }
}
