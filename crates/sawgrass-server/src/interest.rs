//! Interest management: a spatial grid and each viewer's visible set (see `docs/architecture.md`,
//! "Interest management").
//!
//! [`SpatialGrid`] buckets entities into square cells on the horizontal plane. [`VisibleSet`] keeps the
//! entities one viewer sees and, on every update, reports which entered and which left. Membership uses two
//! radii: an entity enters within [`VisibilityConfig::enter_radius`] and leaves only beyond
//! [`VisibilityConfig::leave_radius`], so an entity moving along the edge does not flicker in and out.
//!
//! Height is ignored: entities stacked vertically (interiors, floors of a building) are separated by routing
//! buckets, which arrive with a later task. Ranking by priority and the per-player byte budget are also later
//! tasks; this module only decides who is near enough to be considered.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// Server-side identifier of a replicated entity (player, vehicle, NPC, object).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(pub u32);

/// A position on the horizontal plane, in metres.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    /// East-west coordinate.
    pub x: f32,
    /// North-south coordinate.
    pub y: f32,
}

impl Point {
    /// A point at `x`, `y`.
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Squared distance to `other`.
    #[must_use]
    pub fn distance_squared(self, other: Self) -> f32 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        dx * dx + dy * dy
    }
}

/// Default cell edge, in metres (`docs/architecture.md`: "cells of about 100 m").
pub const DEFAULT_CELL_SIZE: f32 = 100.0;

/// Coordinates of a grid cell.
type CellKey = (i32, i32);

/// Multiplicative hasher for the small integer keys used here (cell coordinates, entity ids).
///
/// Entity ids come from the server. Cell keys are derived from positions that clients report, but the server
/// validates those positions against the map, so the key space is bounded, and each entity occupies at most one
/// cell: to make cells collide, an attacker needs one connected entity per colliding cell. That bounds the cost
/// of crafted collisions, so the flooding resistance of the default SipHash is not worth its price here (it took
/// about 45% of the benchmark's tick). A dense array over the bounded map would remove cell hashing entirely
/// once the map bounds are known.
#[derive(Debug, Default, Clone, Copy)]
struct IntHasher(u64);

impl Hasher for IntHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write_i32(&mut self, value: i32) {
        self.write_u32(value as u32);
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

type IntMap<K, V> = HashMap<K, V, BuildHasherDefault<IntHasher>>;

#[derive(Debug, Clone, Copy)]
struct Slot {
    position: Point,
    cell: CellKey,
    /// Index of the entity in its cell's list, so removal is O(1).
    index: usize,
}

/// Entities bucketed by square cells for radius queries.
#[derive(Debug)]
pub struct SpatialGrid {
    cell_size: f32,
    inverse_cell_size: f32,
    /// Entities and their positions per cell. Positions are kept here too so queries read one list.
    cells: IntMap<CellKey, Vec<(EntityId, Point)>>,
    slots: IntMap<EntityId, Slot>,
}

impl Default for SpatialGrid {
    fn default() -> Self {
        Self::new(DEFAULT_CELL_SIZE)
    }
}

impl SpatialGrid {
    /// An empty grid with cells `cell_size` metres wide.
    ///
    /// # Panics
    ///
    /// If `cell_size` is not a finite positive number.
    #[must_use]
    pub fn new(cell_size: f32) -> Self {
        assert!(
            cell_size.is_finite() && cell_size > 0.0,
            "cell size must be positive, got {cell_size}"
        );
        Self {
            cell_size,
            inverse_cell_size: 1.0 / cell_size,
            cells: IntMap::default(),
            slots: IntMap::default(),
        }
    }

    /// Edge of a cell, in metres.
    #[must_use]
    pub fn cell_size(&self) -> f32 {
        self.cell_size
    }

    /// Number of entities in the grid.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the grid holds no entities.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Position of `id`, if it is in the grid.
    #[must_use]
    pub fn position(&self, id: EntityId) -> Option<Point> {
        self.slots.get(&id).map(|slot| slot.position)
    }

    // Saturating float-to-int casts are intended: coordinates far outside the map clamp to the edge cells.
    fn cell_coord(&self, value: f32) -> i32 {
        (value * self.inverse_cell_size).floor() as i32
    }

    fn cell_of(&self, position: Point) -> CellKey {
        (self.cell_coord(position.x), self.cell_coord(position.y))
    }

    /// Adds `id` at `position`, or moves it there if it is already in the grid.
    pub fn upsert(&mut self, id: EntityId, position: Point) {
        let cell = self.cell_of(position);
        if let Some(slot) = self.slots.get_mut(&id) {
            slot.position = position;
            if slot.cell == cell {
                if let Some(entry) = self
                    .cells
                    .get_mut(&cell)
                    .and_then(|members| members.get_mut(slot.index))
                {
                    entry.1 = position;
                }
                return;
            }
            let (old_cell, old_index) = (slot.cell, slot.index);
            self.detach(old_cell, old_index);
        }
        let members = self.cells.entry(cell).or_default();
        members.push((id, position));
        let index = members.len() - 1;
        self.slots.insert(
            id,
            Slot {
                position,
                cell,
                index,
            },
        );
    }

    /// Removes `id`. Returns whether it was in the grid.
    pub fn remove(&mut self, id: EntityId) -> bool {
        match self.slots.remove(&id) {
            Some(slot) => {
                self.detach(slot.cell, slot.index);
                true
            }
            None => false,
        }
    }

    /// Takes the entity at `index` out of `cell`'s list and fixes the index of the one moved into its place.
    fn detach(&mut self, cell: CellKey, index: usize) {
        let Some(members) = self.cells.get_mut(&cell) else {
            return;
        };
        members.swap_remove(index);
        if let Some(&(moved, _)) = members.get(index) {
            if let Some(slot) = self.slots.get_mut(&moved) {
                slot.index = index;
            }
        } else if members.is_empty() {
            self.cells.remove(&cell);
        }
    }

    /// Calls `visit` for every entity within `radius` metres of `center`, in no particular order.
    pub fn for_each_within(&self, center: Point, radius: f32, mut visit: impl FnMut(EntityId, Point, f32)) {
        let radius_squared = radius * radius;
        let (x0, y0) = self.cell_of(Point::new(center.x - radius, center.y - radius));
        let (x1, y1) = self.cell_of(Point::new(center.x + radius, center.y + radius));
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                let Some(members) = self.cells.get(&(cx, cy)) else {
                    continue;
                };
                for &(id, position) in members {
                    let distance_squared = center.distance_squared(position);
                    if distance_squared <= radius_squared {
                        visit(id, position, distance_squared);
                    }
                }
            }
        }
    }
}

/// Radii that decide when an entity enters and leaves a viewer's visible set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisibilityConfig {
    /// An entity outside the set enters it at this distance or closer, in metres.
    enter_radius: f32,
    /// An entity inside the set leaves it only beyond this distance, in metres. At least `enter_radius`.
    leave_radius: f32,
}

impl Default for VisibilityConfig {
    /// 200 m to enter and 230 m to leave: two cells around the viewer, with a 15% margin against flicker.
    fn default() -> Self {
        Self {
            enter_radius: 200.0,
            leave_radius: 230.0,
        }
    }
}

impl VisibilityConfig {
    /// An entity outside the set enters it at this distance or closer, in metres.
    #[must_use]
    pub fn enter_radius(&self) -> f32 {
        self.enter_radius
    }

    /// An entity inside the set leaves it only beyond this distance, in metres.
    #[must_use]
    pub fn leave_radius(&self) -> f32 {
        self.leave_radius
    }

    /// A configuration with these radii.
    ///
    /// # Panics
    ///
    /// If either radius is negative or not finite, or `leave_radius` is smaller than `enter_radius`.
    #[must_use]
    pub fn new(enter_radius: f32, leave_radius: f32) -> Self {
        assert!(
            enter_radius.is_finite() && enter_radius >= 0.0,
            "enter radius must be non-negative"
        );
        assert!(
            leave_radius.is_finite() && leave_radius >= enter_radius,
            "leave radius must be at least the enter radius"
        );
        Self {
            enter_radius,
            leave_radius,
        }
    }
}

/// What changed in a visible set during one update. Both lists are sorted by id.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VisibilityChanges {
    /// Entities that became visible.
    pub entered: Vec<EntityId>,
    /// Entities that stopped being visible, including ones removed from the grid.
    pub left: Vec<EntityId>,
}

impl VisibilityChanges {
    /// Empties both lists, keeping their allocations.
    pub fn clear(&mut self) {
        self.entered.clear();
        self.left.clear();
    }

    /// Whether nothing entered or left.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entered.is_empty() && self.left.is_empty()
    }
}

/// The entities one viewer currently sees.
#[derive(Debug, Default, Clone)]
pub struct VisibleSet {
    /// Current members, sorted by id.
    members: Vec<EntityId>,
    /// Scratch buffer for the next members, kept to avoid an allocation per update.
    next: Vec<EntityId>,
}

impl VisibleSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current members, sorted by id.
    #[must_use]
    pub fn members(&self) -> &[EntityId] {
        &self.members
    }

    /// Whether `id` is visible.
    #[must_use]
    pub fn contains(&self, id: EntityId) -> bool {
        self.members.binary_search(&id).is_ok()
    }

    /// Number of visible entities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether nothing is visible.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Recomputes the set for a viewer at `viewer_position` and writes what entered and left into `changes`
    /// (cleared first). `viewer` itself is never a member; pass `None` for a viewer that is not an entity,
    /// such as a spectator camera.
    pub fn update(
        &mut self,
        grid: &SpatialGrid,
        viewer: Option<EntityId>,
        viewer_position: Point,
        config: &VisibilityConfig,
        changes: &mut VisibilityChanges,
    ) {
        changes.clear();
        let enter_squared = config.enter_radius() * config.enter_radius();
        let members = &self.members;
        let next = &mut self.next;
        next.clear();
        grid.for_each_within(
            viewer_position,
            config.leave_radius(),
            |id, _, distance_squared| {
                if Some(id) == viewer {
                    return;
                }
                // Within the leave radius: members stay, others need to be within the enter radius.
                if distance_squared <= enter_squared || members.binary_search(&id).is_ok() {
                    next.push(id);
                }
            },
        );
        next.sort_unstable();
        diff_sorted(&self.members, &self.next, changes);
        std::mem::swap(&mut self.members, &mut self.next);
    }
}

/// Fills `changes` with the ids in `new` but not in `old` (entered) and in `old` but not in `new` (left).
fn diff_sorted(old: &[EntityId], new: &[EntityId], changes: &mut VisibilityChanges) {
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        match old[i].cmp(&new[j]) {
            std::cmp::Ordering::Less => {
                changes.left.push(old[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                changes.entered.push(new[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    changes.left.extend_from_slice(&old[i..]);
    changes.entered.extend_from_slice(&new[j..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWER: EntityId = EntityId(0);
    const OTHER: EntityId = EntityId(1);

    fn config() -> VisibilityConfig {
        VisibilityConfig::new(200.0, 230.0)
    }

    /// Grid with the viewer at the origin and `OTHER` at (`x`, 0); returns the changes of one update.
    fn step(grid: &mut SpatialGrid, set: &mut VisibleSet, x: f32) -> VisibilityChanges {
        grid.upsert(VIEWER, Point::default());
        grid.upsert(OTHER, Point::new(x, 0.0));
        let mut changes = VisibilityChanges::default();
        set.update(grid, Some(VIEWER), Point::default(), &config(), &mut changes);
        changes
    }

    #[test]
    fn entity_enters_within_enter_radius() {
        let (mut grid, mut set) = (SpatialGrid::default(), VisibleSet::new());
        let changes = step(&mut grid, &mut set, 150.0);
        assert_eq!(changes.entered, vec![OTHER]);
        assert!(changes.left.is_empty());
        assert!(set.contains(OTHER));
    }

    #[test]
    fn entity_leaves_beyond_leave_radius() {
        let (mut grid, mut set) = (SpatialGrid::default(), VisibleSet::new());
        step(&mut grid, &mut set, 150.0);
        let changes = step(&mut grid, &mut set, 231.0);
        assert_eq!(changes.left, vec![OTHER]);
        assert!(changes.entered.is_empty());
        assert!(set.is_empty());
    }

    #[test]
    fn outsider_between_radii_does_not_enter() {
        let (mut grid, mut set) = (SpatialGrid::default(), VisibleSet::new());
        let changes = step(&mut grid, &mut set, 215.0);
        assert!(changes.is_empty());
        assert!(!set.contains(OTHER));
    }

    #[test]
    fn member_between_radii_stays() {
        let (mut grid, mut set) = (SpatialGrid::default(), VisibleSet::new());
        step(&mut grid, &mut set, 190.0);
        // Oscillating across the enter radius never makes the entity leave.
        for x in [210.0, 195.0, 229.0, 201.0, 230.0] {
            assert!(step(&mut grid, &mut set, x).is_empty(), "flickered at {x} m");
            assert!(set.contains(OTHER));
        }
    }

    #[test]
    fn entity_reenters_only_within_enter_radius() {
        let (mut grid, mut set) = (SpatialGrid::default(), VisibleSet::new());
        step(&mut grid, &mut set, 100.0);
        step(&mut grid, &mut set, 300.0);
        assert!(step(&mut grid, &mut set, 220.0).is_empty());
        assert_eq!(step(&mut grid, &mut set, 200.0).entered, vec![OTHER]);
    }

    #[test]
    fn removed_entity_leaves() {
        let (mut grid, mut set) = (SpatialGrid::default(), VisibleSet::new());
        step(&mut grid, &mut set, 50.0);
        assert!(grid.remove(OTHER));
        let mut changes = VisibilityChanges::default();
        set.update(&grid, Some(VIEWER), Point::default(), &config(), &mut changes);
        assert_eq!(changes.left, vec![OTHER]);
    }

    #[test]
    fn viewer_never_sees_itself() {
        let (mut grid, mut set) = (SpatialGrid::default(), VisibleSet::new());
        step(&mut grid, &mut set, 10.0);
        assert!(!set.contains(VIEWER));
        assert_eq!(set.members(), &[OTHER]);
    }

    #[test]
    fn grid_moves_entities_between_cells_including_negative_ones() {
        let mut grid = SpatialGrid::new(100.0);
        let ids: Vec<EntityId> = (0..5).map(EntityId).collect();
        for (i, &id) in ids.iter().enumerate() {
            grid.upsert(id, Point::new(i as f32 * 10.0, 0.0));
        }
        grid.upsert(ids[0], Point::new(-150.0, -0.5));
        grid.upsert(ids[2], Point::new(-150.0, -0.5));
        assert!(grid.remove(ids[3]));
        assert!(!grid.remove(ids[3]));
        assert_eq!(grid.len(), 4);
        let mut near_origin = Vec::new();
        grid.for_each_within(Point::new(20.0, 0.0), 25.0, |id, _, _| near_origin.push(id));
        near_origin.sort_unstable();
        assert_eq!(near_origin, vec![ids[1], ids[4]]);
        let mut far = Vec::new();
        grid.for_each_within(Point::new(-150.0, 0.0), 1.0, |id, _, _| far.push(id));
        far.sort_unstable();
        assert_eq!(far, vec![ids[0], ids[2]]);
    }

    /// Small deterministic generator so the test needs no dependency.
    fn next_random(state: &mut u64) -> f32 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state >> 40) as f32 / (1u64 << 24) as f32
    }

    #[test]
    fn grid_queries_match_brute_force() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut grid = SpatialGrid::new(37.0);
        let mut positions = Vec::new();
        for round in 0..20 {
            for i in 0..300u32 {
                let p = Point::new(
                    next_random(&mut state) * 800.0 - 400.0,
                    next_random(&mut state) * 800.0 - 400.0,
                );
                if round > 0 && i % 7 == round % 7 {
                    grid.remove(EntityId(i));
                    positions.retain(|&(id, _)| id != EntityId(i));
                } else {
                    grid.upsert(EntityId(i), p);
                    positions.retain(|&(id, _)| id != EntityId(i));
                    positions.push((EntityId(i), p));
                }
            }
            let center = Point::new(next_random(&mut state) * 400.0 - 200.0, 0.0);
            let radius = next_random(&mut state) * 300.0;
            let mut found = Vec::new();
            grid.for_each_within(center, radius, |id, _, _| found.push(id));
            found.sort_unstable();
            let mut expected: Vec<EntityId> = positions
                .iter()
                .filter(|(_, p)| center.distance_squared(*p) <= radius * radius)
                .map(|&(id, _)| id)
                .collect();
            expected.sort_unstable();
            assert_eq!(found, expected, "round {round}");
            assert_eq!(grid.len(), positions.len());
        }
    }

    #[test]
    #[should_panic(expected = "leave radius")]
    fn leave_radius_below_enter_radius_is_rejected() {
        let _ = VisibilityConfig::new(200.0, 100.0);
    }
}
