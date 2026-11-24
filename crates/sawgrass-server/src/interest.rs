//! Interest management: a spatial grid and each viewer's visible set (see `docs/architecture.md`,
//! "Interest management").
//!
//! [`DenseGrid`] is the server's index (task IM-5, chosen in IM-2, `docs/research/visibility-index`): one array
//! of 50 m cells over fixed map bounds, rebuilt every tick by counting entities into cells, so a query reads one
//! contiguous run of entities per row of cells and nothing is hashed or allocated after the first tick.
//! [`SpatialGrid`], the IM-1 hashed grid, stays for the benchmarks that compare against it. Both answer
//! [`Nearby`] queries. [`VisibleSet`] keeps the entities one viewer sees as one bit per entity id, and on every
//! update reports which entered and which left. Ids must be dense (the server's slots, [`crate::slots`]): a set
//! takes one bit per id up to the largest id in the index. Membership uses two
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
    /// One past the largest id ever inserted.
    id_bound: usize,
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
            id_bound: 0,
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
        self.id_bound = self.id_bound.max(id.0 as usize + 1);
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

/// A spatial index a [`VisibleSet`] can be computed from.
pub trait Nearby {
    /// Calls `visit` with the id, position and squared distance of every entity within `radius` metres of
    /// `center`, in no particular order.
    fn for_each_within(&self, center: Point, radius: f32, visit: impl FnMut(EntityId, Point, f32));

    /// One past the largest id the index can return.
    fn id_bound(&self) -> usize;
}

impl Nearby for SpatialGrid {
    fn for_each_within(&self, center: Point, radius: f32, visit: impl FnMut(EntityId, Point, f32)) {
        SpatialGrid::for_each_within(self, center, radius, visit);
    }

    fn id_bound(&self) -> usize {
        self.id_bound
    }
}

/// Cell edge of the [`DenseGrid`], in metres (IM-2 measured 50 m best or tied in every scenario but the nearest-128
/// cut in the largest crowd).
pub const DENSE_CELL_SIZE: f32 = 50.0;

/// The square the [`DenseGrid`] covers. Positions outside it are counted into the nearest edge cell, so queries stay
/// exact; they only get slower if many entities end up there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapBounds {
    /// South-west corner, in metres.
    pub min: Point,
    /// Edge length, in metres.
    pub size: f32,
}

impl Default for MapBounds {
    /// 16,384 m centred on the origin. The game's real map bounds are not known yet; this is twice IM-2's 8 km
    /// placeholder on each axis, as a margin, and costs 430 KB of cell offsets at 50 m.
    fn default() -> Self {
        Self {
            min: Point::new(-8192.0, -8192.0),
            size: 16_384.0,
        }
    }
}

/// A grid over fixed map bounds stored as one array: [`DenseGrid::rebuild`] counts the entities into cells and
/// writes them out sorted by cell, row by row, so a query reads one contiguous run of entities per row of cells.
///
/// [`DenseGrid::upsert`] and [`DenseGrid::remove`] only record positions; call [`DenseGrid::rebuild`] once after a
/// tick's changes and before its queries.
#[derive(Debug, Clone)]
pub struct DenseGrid {
    origin: Point,
    inverse_cell: f32,
    /// Cells per side.
    side: usize,
    /// Position of each id, `None` for ids not in the grid.
    positions: Vec<Option<Point>>,
    len: usize,
    /// `start[c]..start[c + 1]` index the entities of cell `c` in `xs`, `ys` and `ids`.
    start: Vec<u32>,
    xs: Vec<f32>,
    ys: Vec<f32>,
    ids: Vec<u32>,
    cell_of: Vec<u32>,
    dirty: bool,
}

impl Default for DenseGrid {
    fn default() -> Self {
        Self::new(MapBounds::default(), DENSE_CELL_SIZE)
    }
}

impl DenseGrid {
    /// An empty grid over `bounds` with cells `cell_size` metres wide.
    ///
    /// # Panics
    ///
    /// If `cell_size` or the bounds' size is not a finite positive number, or the grid would have more than 2^26
    /// cells.
    #[must_use]
    pub fn new(bounds: MapBounds, cell_size: f32) -> Self {
        assert!(
            cell_size.is_finite() && cell_size > 0.0 && bounds.size.is_finite() && bounds.size > 0.0,
            "cell size and bounds must be positive"
        );
        let side = (bounds.size / cell_size).ceil().max(1.0) as usize;
        assert!(side * side <= 1 << 26, "too many cells: {side} per side");
        Self {
            origin: bounds.min,
            inverse_cell: 1.0 / cell_size,
            side,
            positions: Vec::new(),
            len: 0,
            start: vec![0; side * side + 1],
            xs: Vec::new(),
            ys: Vec::new(),
            ids: Vec::new(),
            cell_of: Vec::new(),
            dirty: false,
        }
    }

    /// Number of entities in the grid.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the grid holds no entity.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Where `id` is, if it is in the grid.
    #[must_use]
    pub fn position(&self, id: EntityId) -> Option<Point> {
        self.positions.get(id.0 as usize).copied().flatten()
    }

    /// Adds `id` at `position`, or moves it there.
    pub fn upsert(&mut self, id: EntityId, position: Point) {
        let i = id.0 as usize;
        if i >= self.positions.len() {
            self.positions.resize(i + 1, None);
        }
        if self.positions[i].replace(position).is_none() {
            self.len += 1;
        }
        self.dirty = true;
    }

    /// Removes `id`. Returns whether it was in the grid.
    pub fn remove(&mut self, id: EntityId) -> bool {
        let removed = self
            .positions
            .get_mut(id.0 as usize)
            .and_then(Option::take)
            .is_some();
        if removed {
            self.len -= 1;
            self.dirty = true;
        }
        removed
    }

    fn coord(&self, v: f32, origin: f32) -> usize {
        // Saturating cast: below the origin (or NaN) gives 0, far above gives the last cell.
        (((v - origin) * self.inverse_cell).floor().max(0.0) as usize).min(self.side - 1)
    }

    fn cell(&self, p: Point) -> usize {
        self.coord(p.y, self.origin.y) * self.side + self.coord(p.x, self.origin.x)
    }

    /// Sorts every entity into its cell. Allocates only when the number of entities grows.
    pub fn rebuild(&mut self) {
        self.start.fill(0);
        self.cell_of.clear();
        for p in self.positions.iter().flatten() {
            let c = self.cell(*p);
            self.cell_of.push(c as u32);
            self.start[c + 1] += 1;
        }
        for c in 1..self.start.len() {
            self.start[c] += self.start[c - 1];
        }
        self.xs.resize(self.len, 0.0);
        self.ys.resize(self.len, 0.0);
        self.ids.resize(self.len, 0);
        // Scatter, using start[c] as a cursor, then shift the cursors back by one cell.
        let present = self
            .positions
            .iter()
            .enumerate()
            .filter_map(|(id, p)| p.map(|p| (id, p)));
        for ((id, p), &c) in present.zip(&self.cell_of) {
            let j = self.start[c as usize] as usize;
            self.xs[j] = p.x;
            self.ys[j] = p.y;
            self.ids[j] = id as u32;
            self.start[c as usize] += 1;
        }
        for c in (1..self.start.len()).rev() {
            self.start[c] = self.start[c - 1];
        }
        self.start[0] = 0;
        self.dirty = false;
    }
}

impl Nearby for DenseGrid {
    /// # Panics
    ///
    /// In debug builds, if the grid changed since the last [`DenseGrid::rebuild`].
    fn for_each_within(&self, center: Point, radius: f32, mut visit: impl FnMut(EntityId, Point, f32)) {
        debug_assert!(!self.dirty, "DenseGrid queried before rebuild");
        let radius_squared = radius * radius;
        let x0 = self.coord(center.x - radius, self.origin.x);
        let x1 = self.coord(center.x + radius, self.origin.x);
        let y0 = self.coord(center.y - radius, self.origin.y);
        let y1 = self.coord(center.y + radius, self.origin.y);
        for y in y0..=y1 {
            let row = y * self.side;
            for j in self.start[row + x0] as usize..self.start[row + x1 + 1] as usize {
                let position = Point::new(self.xs[j], self.ys[j]);
                let distance_squared = center.distance_squared(position);
                if distance_squared <= radius_squared {
                    visit(EntityId(self.ids[j]), position, distance_squared);
                }
            }
        }
    }

    fn id_bound(&self) -> usize {
        self.positions.len()
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

/// The entities one viewer currently sees, as one bit per entity id (IM-2: the sorted lists this replaces were
/// most of the visibility cost in a crowd).
#[derive(Debug, Default, Clone)]
pub struct VisibleSet {
    bits: Vec<u64>,
    /// Scratch for the next update's bits, kept to avoid an allocation per update.
    next: Vec<u64>,
    /// The members in id order, rebuilt from the bits on every update.
    members: Vec<EntityId>,
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
        self.bits
            .get(id.0 as usize / 64)
            .is_some_and(|word| word & (1 << (id.0 % 64)) != 0)
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
    pub fn update<I: Nearby + ?Sized>(
        &mut self,
        index: &I,
        viewer: Option<EntityId>,
        viewer_position: Point,
        config: &VisibilityConfig,
        changes: &mut VisibilityChanges,
    ) {
        changes.clear();
        let words = index.id_bound().div_ceil(64).max(self.bits.len());
        self.bits.resize(words, 0);
        self.next.clear();
        self.next.resize(words, 0);
        let enter_squared = config.enter_radius() * config.enter_radius();
        let (bits, next) = (&self.bits, &mut self.next);
        index.for_each_within(
            viewer_position,
            config.leave_radius(),
            |id, _, distance_squared| {
                if Some(id) == viewer {
                    return;
                }
                let (word, bit) = (id.0 as usize / 64, 1u64 << (id.0 % 64));
                // Within the leave radius: members stay, others need to be within the enter radius.
                if let (Some(old), Some(new)) = (bits.get(word), next.get_mut(word))
                    && (distance_squared <= enter_squared || old & bit != 0)
                {
                    *new |= bit;
                }
            },
        );
        self.members.clear();
        for (word, (&old, &new)) in self.bits.iter().zip(&self.next).enumerate() {
            let base = (word * 64) as u32;
            push_bits(&mut changes.entered, base, new & !old);
            push_bits(&mut changes.left, base, old & !new);
            push_bits(&mut self.members, base, new);
        }
        std::mem::swap(&mut self.bits, &mut self.next);
    }
}

/// Appends the id of every set bit of `word`, lowest first; bit `i` is id `base + i`.
fn push_bits(out: &mut Vec<EntityId>, base: u32, mut word: u64) {
    while word != 0 {
        out.push(EntityId(base + word.trailing_zeros()));
        word &= word - 1;
    }
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
    fn dense_grid_queries_match_brute_force_including_outside_the_bounds() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let bounds = MapBounds {
            min: Point::new(-300.0, -300.0),
            size: 600.0,
        };
        let mut grid = DenseGrid::new(bounds, 50.0);
        let mut positions: Vec<Option<Point>> = vec![None; 300];
        for round in 0..20 {
            for i in 0..300u32 {
                // Some of them up to 100 m beyond the bounds on every side.
                let p = Point::new(
                    next_random(&mut state) * 800.0 - 400.0,
                    next_random(&mut state) * 800.0 - 400.0,
                );
                if round > 0 && i % 7 == round % 7 {
                    assert_eq!(grid.remove(EntityId(i)), positions[i as usize].is_some());
                    positions[i as usize] = None;
                } else {
                    grid.upsert(EntityId(i), p);
                    positions[i as usize] = Some(p);
                }
            }
            grid.rebuild();
            let center = Point::new(next_random(&mut state) * 800.0 - 400.0, 0.0);
            let radius = next_random(&mut state) * 300.0;
            let mut found = Vec::new();
            Nearby::for_each_within(&grid, center, radius, |id, _, _| found.push(id));
            found.sort_unstable();
            let expected: Vec<EntityId> = positions
                .iter()
                .enumerate()
                .filter(|(_, p)| p.is_some_and(|p| center.distance_squared(p) <= radius * radius))
                .map(|(i, _)| EntityId(i as u32))
                .collect();
            assert_eq!(found, expected, "round {round}");
            assert_eq!(grid.len(), positions.iter().flatten().count());
        }
    }

    /// The server's grid and the IM-1 grid give every viewer the same set and the same changes.
    #[test]
    fn visible_sets_are_the_same_on_both_grids() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let (mut dense, mut hashed) = (DenseGrid::default(), SpatialGrid::default());
        let mut sets = vec![(VisibleSet::new(), VisibleSet::new()); 50];
        let (mut a, mut b) = (VisibilityChanges::default(), VisibilityChanges::default());
        let mut points: Vec<Point> = (0..400)
            .map(|_| Point::new(next_random(&mut state) * 1000.0, next_random(&mut state) * 1000.0))
            .collect();
        for _ in 0..30 {
            for (i, p) in points.iter_mut().enumerate() {
                p.x += next_random(&mut state) * 40.0 - 20.0;
                p.y += next_random(&mut state) * 40.0 - 20.0;
                dense.upsert(EntityId(i as u32), *p);
                hashed.upsert(EntityId(i as u32), *p);
            }
            dense.rebuild();
            for (viewer, (on_dense, on_hashed)) in sets.iter_mut().enumerate() {
                let (id, at) = (EntityId(viewer as u32), points[viewer]);
                on_dense.update(&dense, Some(id), at, &config(), &mut a);
                on_hashed.update(&hashed, Some(id), at, &config(), &mut b);
                assert_eq!(a, b);
                assert_eq!(on_dense.members(), on_hashed.members());
            }
        }
    }

    #[test]
    #[should_panic(expected = "leave radius")]
    fn leave_radius_below_enter_radius_is_rejected() {
        let _ = VisibilityConfig::new(200.0, 100.0);
    }
}
