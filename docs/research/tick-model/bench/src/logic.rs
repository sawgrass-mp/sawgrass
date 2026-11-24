//! The work of one tick, shared by every model so that they differ only in storage and scheduling.
//!
//! Each function here works on one player and is pure apart from its arguments, so a model may call it from
//! any thread in any order within a phase and still produce the same world as every other model.

use sawgrass_server::interest::{
    EntityId, Point, SpatialGrid, VisibilityChanges, VisibilityConfig, VisibleSet,
};

/// Fastest a client may move, in metres per second (a fast car), before its state is rejected.
pub const MAX_SPEED: f32 = 60.0;
/// Entities kept in one player's snapshot: the nearest ones, a stand-in for ranking by priority.
pub const SNAPSHOT_CAP: usize = 128;
/// Chance per player and tick, in thousandths, of handing money to the nearest visible player.
pub const TRANSFER_PER_MILLE: u64 = 3;
/// Seconds between two wages of a job.
pub const JOB_PERIOD_SECS: u32 = 30;

/// What one client reported for one tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct Input {
    pub pos: Point,
    pub vel: Point,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vitals {
    pub health: f32,
    pub hunger: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    pub timer: u32,
    pub wage: i64,
}

/// Money handed from one player to another, applied in the serial phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Transfer {
    pub from: u32,
    pub to: u32,
    pub amount: i64,
}

/// One entity in a snapshot: id and position quantized to centimetres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapEntry {
    pub id: u32,
    pub x: i32,
    pub y: i32,
}

/// A player's snapshot and the scratch space for building it, kept per player to avoid allocations.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    scratch: Vec<(f32, u32)>,
    pub entries: Vec<SnapEntry>,
}

/// Everything a player owns, as one record; the models store these fields as arrays or components.
#[derive(Debug, Clone)]
pub struct PlayerState {
    pub pos: Point,
    pub vel: Point,
    pub vitals: Vitals,
    pub job: Job,
    pub wallet: i64,
    pub violations: u32,
    pub vis: VisibleSet,
    pub changes: VisibilityChanges,
    pub snap: Snapshot,
}

impl PlayerState {
    pub fn new(index: u32, input: Input) -> Self {
        let r = mix(u64::from(index), u64::MAX);
        Self {
            pos: input.pos,
            vel: input.vel,
            vitals: Vitals {
                health: 100.0,
                hunger: 50.0 + (r % 50) as f32,
            },
            job: Job {
                timer: (r >> 8) as u32 % 1000,
                wage: 100 + (r >> 20) as i64 % 400,
            },
            wallet: 1000,
            violations: 0,
            vis: VisibleSet::new(),
            changes: VisibilityChanges::default(),
            snap: Snapshot::default(),
        }
    }
}

/// Per-run constants every phase needs.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub dt: f32,
    pub area: f32,
    pub job_period_ticks: u32,
    pub vis: VisibilityConfig,
}

impl Params {
    pub fn new(rate_hz: u32, area: f32) -> Self {
        Self {
            dt: 1.0 / rate_hz as f32,
            area,
            job_period_ticks: JOB_PERIOD_SECS * rate_hz,
            vis: VisibilityConfig::default(),
        }
    }
}

/// SplitMix64 of two values: deterministic per-player, per-tick randomness without shared state.
pub fn mix(a: u64, b: u64) -> u64 {
    let mut z = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_add(0xD1B5_4A32_D192_ED03);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Phase 1: accept the client's state if it is physically possible, as the server validates movement.
pub fn ingest(p: &Params, input: Input, pos: &mut Point, vel: &mut Point, violations: &mut u32) {
    let max_step = MAX_SPEED * p.dt * 1.5 + 0.5;
    let in_bounds = (0.0..=p.area).contains(&input.pos.x) && (0.0..=p.area).contains(&input.pos.y);
    let speed_ok = input.vel.x.hypot(input.vel.y) <= MAX_SPEED;
    if in_bounds && speed_ok && pos.distance_squared(input.pos) <= max_step * max_step {
        *pos = input.pos;
        *vel = input.vel;
    } else {
        *violations += 1;
        *vel = Point::default();
    }
}

/// Phase 2 is the grid rebuild: `grid.upsert` for every player, on one thread in every model.
pub fn grid_upsert(grid: &mut SpatialGrid, index: u32, pos: Point) {
    grid.upsert(EntityId(index), pos);
}

/// Phase 3: the project's visible-set update (IM-1).
pub fn visibility(
    p: &Params,
    grid: &SpatialGrid,
    index: u32,
    pos: Point,
    vis: &mut VisibleSet,
    changes: &mut VisibilityChanges,
) {
    vis.update(grid, Some(EntityId(index)), pos, &p.vis, changes);
}

/// Phase 4: roleplay logic for one player. Returns a transfer to apply in the serial phase, if any.
///
/// `pos_of` reads another player's position: an array index, an ECS lookup, whatever the model has.
#[allow(clippy::too_many_arguments)]
pub fn gameplay(
    p: &Params,
    tick: u64,
    index: u32,
    pos: Point,
    vis: &VisibleSet,
    vitals: &mut Vitals,
    job: &mut Job,
    wallet: &mut i64,
    pos_of: impl Fn(u32) -> Point,
) -> Option<Transfer> {
    vitals.hunger = (vitals.hunger - 0.05 * p.dt).max(0.0);
    if vitals.hunger == 0.0 {
        vitals.health = (vitals.health - 1.0 * p.dt).max(0.0);
    } else {
        vitals.health = (vitals.health + 0.5 * p.dt).min(100.0);
    }
    job.timer += 1;
    if job.timer >= p.job_period_ticks {
        job.timer = 0;
        *wallet += job.wage;
    }
    let r = mix(u64::from(index), tick);
    if r % 1000 >= TRANSFER_PER_MILLE {
        return None;
    }
    let mut best: Option<(f32, u32)> = None;
    for &EntityId(other) in vis.members() {
        let d = pos.distance_squared(pos_of(other));
        if best.is_none_or(|(bd, bi)| d < bd || (d == bd && other < bi)) {
            best = Some((d, other));
        }
    }
    best.map(|(_, to)| Transfer {
        from: index,
        to,
        amount: 1 + ((r >> 16) % 50) as i64,
    })
}

/// Phase 5, serial: sorts `transfers` by sender and applies each one the sender can still pay.
pub fn apply_transfers(
    transfers: &mut [Transfer],
    mut get: impl FnMut(u32) -> i64,
    mut set: impl FnMut(u32, i64),
) -> u32 {
    transfers.sort_unstable();
    let mut applied = 0;
    for t in transfers.iter() {
        let from = get(t.from);
        if from >= t.amount {
            set(t.from, from - t.amount);
            let to = get(t.to);
            set(t.to, to + t.amount);
            applied += 1;
        }
    }
    applied
}

/// Phase 6: the player's snapshot: the nearest [`SNAPSHOT_CAP`] visible entities, by id, in centimetres.
pub fn snapshot(pos: Point, vis: &VisibleSet, snap: &mut Snapshot, pos_of: impl Fn(u32) -> Point) {
    let Snapshot { scratch, entries } = snap;
    scratch.clear();
    for &EntityId(other) in vis.members() {
        scratch.push((pos.distance_squared(pos_of(other)), other));
    }
    let by_distance = |a: &(f32, u32), b: &(f32, u32)| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1));
    if scratch.len() > SNAPSHOT_CAP {
        scratch.select_nth_unstable_by(SNAPSHOT_CAP - 1, by_distance);
        scratch.truncate(SNAPSHOT_CAP);
    }
    scratch.sort_unstable_by_key(|e| e.1);
    entries.clear();
    entries.extend(scratch.iter().map(|&(_, id)| {
        let q = pos_of(id);
        SnapEntry {
            id,
            x: (q.x * 100.0) as i32,
            y: (q.y * 100.0) as i32,
        }
    }));
}

/// Folds one player's state into a checksum; models call it in player order.
#[allow(clippy::too_many_arguments)]
pub fn fold(
    sum: u64,
    pos: Point,
    vitals: Vitals,
    job: Job,
    wallet: i64,
    violations: u32,
    vis: &VisibleSet,
    snap: &Snapshot,
) -> u64 {
    let mut h = mix(sum, u64::from(pos.x.to_bits()) << 32 | u64::from(pos.y.to_bits()));
    h = mix(
        h,
        u64::from(vitals.health.to_bits()) << 32 | u64::from(vitals.hunger.to_bits()),
    );
    h = mix(
        h,
        u64::from(job.timer) ^ (wallet as u64) << 20 ^ u64::from(violations) << 50,
    );
    for &EntityId(id) in vis.members() {
        h = mix(h, u64::from(id));
    }
    for e in &snap.entries {
        h = mix(h, u64::from(e.id) ^ (e.x as u64) << 20 ^ (e.y as u64) << 42);
    }
    h
}
