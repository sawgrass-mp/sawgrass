//! A small world moving over time, and the snapshots its players would get.
//!
//! Compression and delta encoding gain from what repeats between packets: entities that stand still, move in
//! straight lines, or stay in view from one tick to the next. Independent random snapshots (as NET-4 used) would
//! hide that, so this simulates a dense area at 20 Hz: people standing, walking and driving over rolling ground,
//! with each player's visible set chosen by distance with hysteresis, as architecture.md describes.

use std::f64::consts::{PI, TAU};

use sawgrass_protocol::{Position, Rotation, Velocity};

use crate::model::{Entity, Snapshot};

/// Seconds per tick at 20 Hz.
pub const DT: f64 = 0.05;
/// Entities in one player's snapshot (architecture.md: about 100 to 150).
pub const VISIBLE: usize = 100;
/// An entity already in view stays until it falls below this rank, so the edge of the set does not flicker.
const KEEP_RANK: usize = 120;

/// Deterministic xorshift64* generator.
#[derive(Debug, Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Standing,
    Walking,
    Driving,
}

#[derive(Debug, Clone)]
struct Body {
    id: u32,
    kind: Kind,
    x: f64,
    y: f64,
    heading: f64,
    speed: f64,
    target_speed: f64,
    /// Radians per second.
    turn: f64,
}

/// Height of the ground in metres: gentle hills, as in a city with slopes.
fn ground(x: f64, y: f64) -> f64 {
    20.0 + 8.0 * (x / 90.0).sin() + 6.0 * (y / 70.0).cos()
}

fn slope(x: f64, y: f64, heading: f64) -> f64 {
    let dx = 8.0 / 90.0 * (x / 90.0).cos();
    let dy = -6.0 / 70.0 * (y / 70.0).sin();
    dx * heading.cos() + dy * heading.sin()
}

pub struct World {
    bodies: Vec<Body>,
    rng: Rng,
    size: f64,
    pub tick: u32,
}

impl World {
    /// `count` entities in a square of `size` metres: 30% standing, 40% walking, 30% driving.
    pub fn new(seed: u64, count: usize, size: f64) -> Self {
        let mut rng = Rng(seed | 1);
        let mut ids: Vec<u32> = Vec::with_capacity(count);
        while ids.len() < count {
            let id = (rng.next() % 20_000) as u32 + 1;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let bodies = ids
            .into_iter()
            .map(|id| {
                let r = rng.unit();
                let kind = if r < 0.3 {
                    Kind::Standing
                } else if r < 0.7 {
                    Kind::Walking
                } else {
                    Kind::Driving
                };
                let speed = match kind {
                    Kind::Standing => 0.0,
                    Kind::Walking => rng.range(0.5, 6.0),
                    Kind::Driving => rng.range(5.0, 45.0),
                };
                Body {
                    id,
                    kind,
                    x: rng.range(0.0, size),
                    y: rng.range(0.0, size),
                    heading: rng.range(0.0, TAU),
                    speed,
                    target_speed: speed,
                    turn: 0.0,
                }
            })
            .collect();
        Self {
            bodies,
            rng,
            size,
            tick: 0,
        }
    }

    pub fn step(&mut self) {
        self.tick += 1;
        let size = self.size;
        let rng = &mut self.rng;
        for b in &mut self.bodies {
            match b.kind {
                Kind::Standing => {
                    if rng.chance(0.002) {
                        b.kind = Kind::Walking;
                        b.speed = rng.range(0.5, 6.0);
                        b.heading = rng.range(0.0, TAU);
                    }
                }
                Kind::Walking => {
                    if rng.chance(0.002) {
                        b.kind = Kind::Standing;
                        b.speed = 0.0;
                        b.turn = 0.0;
                    } else {
                        // A slow random walk of the heading, and now and then a corner.
                        b.turn = (b.turn + rng.range(-0.2, 0.2)).clamp(-0.6, 0.6);
                        if rng.chance(0.005) {
                            b.heading += rng.range(-PI / 2.0, PI / 2.0);
                        }
                    }
                }
                Kind::Driving => {
                    if rng.chance(0.01) {
                        b.target_speed = rng.range(0.0, 45.0);
                    }
                    if rng.chance(0.02) {
                        b.turn = if b.turn == 0.0 { rng.range(-0.5, 0.5) } else { 0.0 };
                    }
                    let accel = if b.target_speed > b.speed { 4.0 } else { -8.0 } * DT;
                    b.speed = if (b.target_speed - b.speed).abs() < accel.abs() {
                        b.target_speed
                    } else {
                        b.speed + accel
                    };
                }
            }
            b.heading = (b.heading + b.turn * DT).rem_euclid(TAU);
            b.x += b.speed * b.heading.cos() * DT;
            b.y += b.speed * b.heading.sin() * DT;
            // Turn back at the edge of the area.
            if !(0.0..=size).contains(&b.x) {
                b.x = b.x.clamp(0.0, size);
                b.heading = (PI - b.heading).rem_euclid(TAU);
            }
            if !(0.0..=size).contains(&b.y) {
                b.y = b.y.clamp(0.0, size);
                b.heading = (-b.heading).rem_euclid(TAU);
            }
        }
    }

    /// The entity as the client that owns it would report it: quantized, with a centimetre of physics jitter on
    /// anything that moves.
    fn entity(&mut self, i: usize) -> Entity {
        let b = &self.bodies[i];
        let moving = b.speed > 0.0;
        let jitter = |rng: &mut Rng| if moving { rng.range(-0.01, 0.01) } else { 0.0 };
        let lift = if b.kind == Kind::Driving { 0.5 } else { 1.0 };
        let (x, y, h, s) = (b.x, b.y, b.heading, b.speed);
        let rise = slope(x, y, h);
        let pos = [
            (x + jitter(&mut self.rng)) as f32,
            (y + jitter(&mut self.rng)) as f32,
            (ground(x, y) + lift + jitter(&mut self.rng)) as f32,
        ];
        let vel = [(s * h.cos()) as f32, (s * h.sin()) as f32, (s * rise) as f32];
        // Yaw from the heading; vehicles also pitch with the slope they drive on.
        let pitch = if b.kind == Kind::Driving {
            -rise.atan()
        } else {
            0.0
        };
        let (sy, cy) = (h / 2.0).sin_cos();
        let (sp, cp) = (pitch / 2.0).sin_cos();
        let q = [-sy * sp, cy * sp, cp * sy, cy * cp].map(|v| v as f32);
        Entity {
            id: b.id,
            pos: Position::from_metres(pos).expect("in range").cm(),
            vel: Velocity::from_metres_per_second(vel).expect("in range").cm_per_s,
            rot: Rotation::from_quaternion(q).expect("unit quaternion").bits,
        }
    }

    /// Every entity this tick, quantized, by index.
    pub fn entities(&mut self) -> Vec<Entity> {
        (0..self.bodies.len()).map(|i| self.entity(i)).collect()
    }

    pub fn len(&self) -> usize {
        self.bodies.len()
    }
}

/// One player and what they see.
pub struct Observer {
    /// Index of the player's own entity.
    pub me: usize,
    visible: Vec<usize>,
}

impl Observer {
    pub fn new(me: usize) -> Self {
        Self {
            me,
            visible: Vec::new(),
        }
    }

    /// The player's snapshot this tick: the nearest entities, keeping ones already in view down to rank 120.
    pub fn snapshot(&mut self, tick: u32, all: &[Entity]) -> Snapshot {
        let here = all[self.me].pos;
        let mut ranked: Vec<(i64, usize)> = all
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != self.me)
            .map(|(i, e)| {
                let d: i64 = (0..3).map(|k| i64::from(e.pos[k] - here[k]).pow(2)).sum();
                (d, i)
            })
            .collect();
        ranked.sort_unstable();
        let mut next: Vec<usize> = ranked[..KEEP_RANK.min(ranked.len())]
            .iter()
            .map(|&(_, i)| i)
            .filter(|i| self.visible.contains(i))
            .take(VISIBLE)
            .collect();
        for &(_, i) in &ranked {
            if next.len() >= VISIBLE {
                break;
            }
            if !next.contains(&i) {
                next.push(i);
            }
        }
        self.visible = next;
        let mut entities: Vec<Entity> = self.visible.iter().map(|&i| all[i]).collect();
        entities.sort_unstable_by_key(|e| e.id);
        Snapshot {
            seq: tick as u16,
            tick,
            entities,
        }
    }
}
