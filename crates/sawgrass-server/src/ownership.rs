//! Who simulates each vehicle and NPC (ADR 0006, section 4).
//!
//! The owner of an entity is the one player whose client simulates it and sends its state. [`Ownership::evaluate`]
//! applies the ADR's rules, in this order:
//!
//! 1. An entity with a driver is owned by the driver.
//! 2. Otherwise the current owner keeps it while connected and within [`OwnershipRules::keep_radius`].
//! 3. Otherwise the nearest player within [`OwnershipRules::grant_radius`] who owns fewer than
//!    [`OwnershipRules::cap`] entities gets it.
//! 4. Otherwise nobody owns it, and the server holds it at rest.
//!
//! Each change of owner increments the entity's epoch (wrapping), which the owner's states must carry. The rules are
//! evaluated for a tenth of the entities each tick (by slot), so a hand-off happens within half a second at 20 Hz and
//! the work per tick stays small; a player who leaves loses what it owned at once. Routing buckets are not in the
//! server yet; when they are, only players in the entity's bucket may own it.

use crate::interest::{EntityId, Nearby, Point};

/// The radii and cap of the ownership rules. Placeholders until the game's real figures are known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OwnershipRules {
    /// The current owner keeps an entity within this many metres of it (rule 2).
    pub keep_radius: f32,
    /// A new owner is the nearest player within this many metres (rule 3). Smaller than `keep_radius`, for
    /// hysteresis.
    pub grant_radius: f32,
    /// Most entities one player owns.
    pub cap: u16,
    /// Rules 2 to 4 run for each entity once in this many ticks.
    pub every_ticks: u32,
}

impl Default for OwnershipRules {
    fn default() -> Self {
        Self {
            keep_radius: 150.0,
            grant_radius: 100.0,
            cap: 24,
            every_ticks: 10,
        }
    }
}

/// A change of owner for the entity in slot `entity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// The entity's slot.
    pub entity: u32,
    /// The player slot that owned it, if any.
    pub old: Option<u32>,
    /// The player slot that owns it now, if any.
    pub new: Option<u32>,
    /// Its epoch from now on.
    pub epoch: u8,
}

#[derive(Debug, Clone, Copy, Default)]
struct Owned {
    owner: Option<u32>,
    driver: Option<u32>,
    epoch: u8,
}

/// The owner of every tracked entity, by slot.
#[derive(Debug, Default)]
pub struct Ownership {
    entities: Vec<Option<Owned>>,
    /// Entities each player slot owns.
    counts: Vec<u16>,
}

impl Ownership {
    /// No entity tracked.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts tracking the entity in `slot`, owned by nobody, at epoch 0.
    pub fn track(&mut self, slot: u32) {
        let i = slot as usize;
        if self.entities.len() <= i {
            self.entities.resize(i + 1, None);
        }
        self.entities[i] = Some(Owned::default());
    }

    /// Stops tracking the entity in `slot`, and returns the change for its owner, if it had one.
    pub fn untrack(&mut self, slot: u32) -> Option<Change> {
        let owned = self.entities.get_mut(slot as usize)?.take()?;
        let old = owned.owner?;
        self.uncount(old);
        Some(Change {
            entity: slot,
            old: Some(old),
            new: None,
            epoch: owned.epoch.wrapping_add(1),
        })
    }

    /// The owner of the entity in `slot` and its epoch; `None` if it is not tracked.
    #[must_use]
    pub fn owner(&self, slot: u32) -> Option<(Option<u32>, u8)> {
        let owned = self.entities.get(slot as usize).copied().flatten()?;
        Some((owned.owner, owned.epoch))
    }

    /// How many entities the player in `slot` owns.
    #[must_use]
    pub fn count(&self, player: u32) -> u16 {
        self.counts.get(player as usize).copied().unwrap_or(0)
    }

    /// Sets or clears the driver of the entity in `slot` (rule 1). It takes effect at the next evaluation of that
    /// entity, or at once through [`Ownership::evaluate_one`].
    pub fn set_driver(&mut self, slot: u32, driver: Option<u32>) {
        if let Some(Some(owned)) = self.entities.get_mut(slot as usize) {
            owned.driver = driver;
        }
    }

    /// Takes every entity the player in `slot` owned away from it and clears it as a driver: it left.
    pub fn player_left(&mut self, player: u32, changes: &mut Vec<Change>) {
        for (slot, owned) in self.entities.iter_mut().enumerate() {
            let Some(owned) = owned else { continue };
            if owned.driver == Some(player) {
                owned.driver = None;
            }
            if owned.owner == Some(player) {
                owned.owner = None;
                owned.epoch = owned.epoch.wrapping_add(1);
                changes.push(Change {
                    entity: slot as u32,
                    old: Some(player),
                    new: None,
                    epoch: owned.epoch,
                });
            }
        }
        if let Some(c) = self.counts.get_mut(player as usize) {
            *c = 0;
        }
    }

    /// Runs the rules for the tracked entities whose turn `tick` is, and appends each change of owner to
    /// `changes`. `position(slot)` is where an entity or a player is; `is_player(slot)` says whether a slot holds a
    /// connected player; `grid` finds players near an entity.
    pub fn evaluate<G: Nearby + ?Sized>(
        &mut self,
        rules: &OwnershipRules,
        tick: u32,
        grid: &G,
        position: impl Fn(u32) -> Option<Point>,
        is_player: impl Fn(u32) -> bool,
        changes: &mut Vec<Change>,
    ) {
        let every = rules.every_ticks.max(1) as usize;
        let start = tick as usize % every;
        for slot in (start..self.entities.len()).step_by(every) {
            self.evaluate_one(rules, slot as u32, grid, &position, &is_player, changes);
        }
    }

    /// Runs the rules for the entity in `slot` now.
    pub fn evaluate_one<G: Nearby + ?Sized>(
        &mut self,
        rules: &OwnershipRules,
        slot: u32,
        grid: &G,
        position: &impl Fn(u32) -> Option<Point>,
        is_player: &impl Fn(u32) -> bool,
        changes: &mut Vec<Change>,
    ) {
        let Some(Some(owned)) = self.entities.get(slot as usize).copied() else {
            return;
        };
        let Some(at) = position(slot) else { return };
        let within = |player: u32, radius: f32| {
            is_player(player) && position(player).is_some_and(|p| p.distance_squared(at) <= radius * radius)
        };
        let new = match (owned.driver, owned.owner) {
            (Some(driver), _) if is_player(driver) => Some(driver),
            (_, Some(owner)) if within(owner, rules.keep_radius) => Some(owner),
            _ => {
                let mut best: Option<(f32, u32)> = None;
                grid.for_each_within(at, rules.grant_radius, |EntityId(id), _, d2| {
                    let better = best.is_none_or(|(b, s)| d2 < b || (d2 == b && id < s));
                    if better && is_player(id) && self.count(id) < rules.cap {
                        best = Some((d2, id));
                    }
                });
                best.map(|(_, id)| id)
            }
        };
        if new == owned.owner {
            return;
        }
        if let Some(old) = owned.owner {
            self.uncount(old);
        }
        if let Some(player) = new {
            let i = player as usize;
            if self.counts.len() <= i {
                self.counts.resize(i + 1, 0);
            }
            self.counts[i] += 1;
        }
        let epoch = owned.epoch.wrapping_add(1);
        self.entities[slot as usize] = Some(Owned {
            owner: new,
            epoch,
            ..owned
        });
        changes.push(Change {
            entity: slot,
            old: owned.owner,
            new,
            epoch,
        });
    }

    fn uncount(&mut self, player: u32) {
        if let Some(c) = self.counts.get_mut(player as usize) {
            *c = c.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interest::{DenseGrid, Point};

    /// Players in slots below 100, entities from 100, all on a line along x.
    struct World {
        grid: DenseGrid,
        at: Vec<Option<Point>>,
        connected: Vec<bool>,
    }

    impl World {
        fn new() -> Self {
            Self {
                grid: DenseGrid::default(),
                at: vec![None; 200],
                connected: vec![false; 100],
            }
        }

        fn put(&mut self, slot: u32, x: f32) {
            self.at[slot as usize] = Some(Point::new(x, 0.0));
            self.grid.upsert(EntityId(slot), Point::new(x, 0.0));
            if slot < 100 {
                self.connected[slot as usize] = true;
            }
        }

        fn run(&mut self, o: &mut Ownership, rules: &OwnershipRules, slot: u32) -> Vec<Change> {
            self.grid.rebuild();
            let mut changes = Vec::new();
            let (at, connected) = (&self.at, &self.connected);
            o.evaluate_one(
                rules,
                slot,
                &self.grid,
                &|s| at[s as usize],
                &|s| connected.get(s as usize) == Some(&true),
                &mut changes,
            );
            changes
        }
    }

    #[test]
    fn the_nearest_player_gets_it_and_keeps_it_within_the_keep_radius() {
        let rules = OwnershipRules::default();
        let (mut w, mut o) = (World::new(), Ownership::new());
        o.track(100);
        w.put(100, 0.0);
        // Nobody near: nobody owns it.
        w.put(1, 120.0);
        assert!(w.run(&mut o, &rules, 100).is_empty());
        assert_eq!(o.owner(100), Some((None, 0)));
        // Within the grant radius: the nearest one.
        w.put(2, 60.0);
        w.put(3, 40.0);
        let c = w.run(&mut o, &rules, 100);
        assert_eq!(
            c,
            vec![Change {
                entity: 100,
                old: None,
                new: Some(3),
                epoch: 1
            }]
        );
        // Someone nearer arrives, the owner moves to 140 m: it keeps it (hysteresis).
        w.put(2, 5.0);
        w.put(3, 140.0);
        assert!(w.run(&mut o, &rules, 100).is_empty());
        // Past the keep radius: the nearest takes it, with a new epoch.
        w.put(3, 160.0);
        let c = w.run(&mut o, &rules, 100);
        assert_eq!(
            c,
            vec![Change {
                entity: 100,
                old: Some(3),
                new: Some(2),
                epoch: 2
            }]
        );
        assert_eq!((o.count(2), o.count(3)), (1, 0));
    }

    #[test]
    fn a_driver_owns_it_and_a_player_who_leaves_loses_everything() {
        let rules = OwnershipRules::default();
        let (mut w, mut o) = (World::new(), Ownership::new());
        o.track(100);
        w.put(100, 0.0);
        w.put(1, 10.0);
        w.put(2, 90.0);
        w.run(&mut o, &rules, 100);
        assert_eq!(o.owner(100), Some((Some(1), 1)));
        o.set_driver(100, Some(2));
        let c = w.run(&mut o, &rules, 100);
        assert_eq!(
            c,
            vec![Change {
                entity: 100,
                old: Some(1),
                new: Some(2),
                epoch: 2
            }]
        );
        // The driver leaves: nobody owns it until the next evaluation, which gives it to the nearest.
        w.connected[2] = false;
        let mut c = Vec::new();
        o.player_left(2, &mut c);
        assert_eq!(
            c,
            vec![Change {
                entity: 100,
                old: Some(2),
                new: None,
                epoch: 3
            }]
        );
        assert_eq!(w.run(&mut o, &rules, 100)[0].new, Some(1));
    }

    #[test]
    fn the_cap_sends_entities_to_the_next_nearest() {
        let rules = OwnershipRules {
            cap: 2,
            ..OwnershipRules::default()
        };
        let (mut w, mut o) = (World::new(), Ownership::new());
        w.put(1, 0.0);
        w.put(2, 50.0);
        for e in 100..104 {
            o.track(e);
            w.put(e, 1.0);
        }
        let owners: Vec<_> = (100..104).map(|e| w.run(&mut o, &rules, e)[0].new).collect();
        assert_eq!(owners, vec![Some(1), Some(1), Some(2), Some(2)]);
        // Untracking gives the owner its count back.
        assert_eq!(o.untrack(100).map(|c| c.old), Some(Some(1)));
        assert_eq!(o.count(1), 1);
    }

    #[test]
    fn evaluate_covers_every_entity_once_in_every_ticks() {
        let rules = OwnershipRules::default();
        let (mut w, mut o) = (World::new(), Ownership::new());
        w.put(1, 0.0);
        for e in 100..130 {
            o.track(e);
            w.put(e, 1.0);
        }
        w.grid.rebuild();
        let mut changes = Vec::new();
        let (at, connected) = (&w.at, &w.connected);
        for tick in 0..rules.every_ticks {
            o.evaluate(
                &rules,
                tick,
                &w.grid,
                |s| at[s as usize],
                |s| connected.get(s as usize) == Some(&true),
                &mut changes,
            );
        }
        assert_eq!(changes.len(), 24, "the cap");
        let mut entities: Vec<u32> = changes.iter().map(|c| c.entity).collect();
        entities.dedup();
        assert_eq!(entities.len(), 24);
    }
}
