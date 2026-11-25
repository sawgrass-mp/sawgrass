//! Movement that looks like players: walk, run or drive between waypoints, pause, pick the next one.
//!
//! Waypoints are spread over a square area, and a share of them lies in a few hotspots (a hospital, a garage, a
//! square), so crowds form and break up the way they do on a roleplay server. Everything comes from a seeded
//! generator: the same seed gives the same route.

/// How bots move over the map.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    /// Centre of the area, in metres.
    pub center: [f32; 2],
    /// Side of the square, in metres.
    pub size: f32,
    /// Number of hotspots, placed by the seed of the run.
    pub hotspots: u32,
    /// Share of waypoints, 0 to 1, that lie in a hotspot.
    pub hotspot_share: f32,
    /// Radius of a hotspot, in metres.
    pub hotspot_radius: f32,
    /// Seed for the hotspot positions, shared by every bot of a run.
    pub seed: u64,
}

impl Default for Area {
    /// A 4 km square at the origin with 8 hotspots of 40 m that draw 30% of the waypoints.
    fn default() -> Self {
        Self {
            center: [0.0, 0.0],
            size: 4000.0,
            hotspots: 8,
            hotspot_share: 0.3,
            hotspot_radius: 40.0,
            seed: 1,
        }
    }
}

/// Deterministic xorshift generator, so bots need no dependency and a run can be repeated.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator for `seed`; any seed, zero included, works.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        // splitmix64 of the seed, so neighbouring seeds start far apart and the state is never zero.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Self((z ^ (z >> 31)) | 1)
    }

    /// A number in `[0, 1)`.
    pub fn unit(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }

    /// A number in `[low, high)`.
    pub fn range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.unit()
    }
}

/// Walking, running and driving speeds in metres per second, with the share of legs that use each.
const GAITS: [(f32, f32); 3] = [(1.4, 0.55), (5.5, 0.25), (18.0, 0.20)];
/// Longest pause at a waypoint, in seconds.
const MAX_PAUSE: f32 = 5.0;
/// Height the bots move at, in metres.
const HEIGHT: f32 = 30.0;

/// One bot's position and the leg it is on.
#[derive(Debug, Clone)]
pub struct Walker {
    rng: Rng,
    area: Area,
    hotspots: Vec<[f32; 2]>,
    position: [f32; 2],
    target: [f32; 2],
    speed: f32,
    pause: f32,
    velocity: [f32; 2],
    yaw: f32,
}

impl Walker {
    /// A walker for bot number `index` of a run over `area`.
    #[must_use]
    pub fn new(area: Area, index: u64) -> Self {
        let mut shared = Rng::new(area.seed);
        let half = area.size / 2.0;
        let hotspots = (0..area.hotspots)
            .map(|_| {
                [
                    area.center[0] + shared.range(-half, half),
                    area.center[1] + shared.range(-half, half),
                ]
            })
            .collect();
        let mut walker = Self {
            rng: Rng::new(area.seed ^ index.wrapping_mul(0xD6E8_FEB8_6659_FD93)),
            area,
            hotspots,
            position: area.center,
            target: area.center,
            speed: 0.0,
            pause: 0.0,
            velocity: [0.0; 2],
            yaw: 0.0,
        };
        walker.position = walker.waypoint();
        walker.next_leg();
        walker
    }

    fn waypoint(&mut self) -> [f32; 2] {
        let half = self.area.size / 2.0;
        if !self.hotspots.is_empty() && self.rng.unit() < self.area.hotspot_share {
            let spot =
                self.hotspots[(self.rng.unit() * self.hotspots.len() as f32) as usize % self.hotspots.len()];
            let r = self.area.hotspot_radius;
            [spot[0] + self.rng.range(-r, r), spot[1] + self.rng.range(-r, r)]
        } else {
            [
                self.area.center[0] + self.rng.range(-half, half),
                self.area.center[1] + self.rng.range(-half, half),
            ]
        }
    }

    fn next_leg(&mut self) {
        self.target = self.waypoint();
        let mut pick = self.rng.unit();
        self.speed = GAITS[0].0;
        for (speed, share) in GAITS {
            if pick < share {
                self.speed = speed;
                break;
            }
            pick -= share;
        }
        self.pause = self.rng.range(0.0, MAX_PAUSE);
    }

    /// Advances by `dt` seconds.
    pub fn step(&mut self, dt: f32) {
        let dx = self.target[0] - self.position[0];
        let dy = self.target[1] - self.position[1];
        let distance = (dx * dx + dy * dy).sqrt();
        let travel = self.speed * dt;
        if distance <= travel {
            self.position = self.target;
            self.velocity = [0.0; 2];
            self.pause -= dt;
            if self.pause <= 0.0 {
                self.next_leg();
            }
            return;
        }
        self.velocity = [dx / distance * self.speed, dy / distance * self.speed];
        self.position[0] += dx / distance * travel;
        self.position[1] += dy / distance * travel;
        self.yaw = dy.atan2(dx);
    }

    /// Heads straight for `target` at `speed`, then pauses there until told otherwise (BOT-5: walking to a
    /// vehicle).
    pub fn go_to(&mut self, target: [f32; 2], speed: f32) {
        self.target = target;
        self.speed = speed;
        self.pause = f32::INFINITY;
    }

    /// Starts a leg at driving speed to a new waypoint (BOT-5: a bot that took the driver's seat).
    pub fn drive(&mut self) {
        self.next_leg();
        self.speed = GAITS[2].0;
    }

    /// Ends whatever the walker was doing and picks a new leg, as after a pause.
    pub fn walk_on(&mut self) {
        self.next_leg();
    }

    /// Position in metres.
    #[must_use]
    pub fn position(&self) -> [f32; 3] {
        [self.position[0], self.position[1], HEIGHT]
    }

    /// Velocity in metres per second.
    #[must_use]
    pub fn velocity(&self) -> [f32; 3] {
        [self.velocity[0], self.velocity[1], 0.0]
    }

    /// Facing as a quaternion `[x, y, z, w]`: a turn of `yaw` about the vertical axis.
    #[must_use]
    pub fn rotation(&self) -> [f32; 4] {
        let half = self.yaw / 2.0;
        [0.0, 0.0, half.sin(), half.cos()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_route() {
        let mut a = Walker::new(Area::default(), 7);
        let mut b = Walker::new(Area::default(), 7);
        let mut c = Walker::new(Area::default(), 8);
        for _ in 0..1000 {
            a.step(0.05);
            b.step(0.05);
            c.step(0.05);
        }
        assert_eq!(a.position(), b.position());
        assert_ne!(a.position(), c.position());
    }

    #[test]
    fn stays_in_the_area_and_keeps_its_speed() {
        let area = Area::default();
        let limit = area.size / 2.0 + area.hotspot_radius;
        for index in 0..50 {
            let mut walker = Walker::new(area, index);
            let mut moved = 0.0;
            for _ in 0..2000 {
                let before = walker.position();
                walker.step(0.05);
                let after = walker.position();
                let step = ((after[0] - before[0]).powi(2) + (after[1] - before[1]).powi(2)).sqrt();
                assert!(step <= 18.0 * 0.05 + 1e-3, "jumped {step} m");
                moved += step;
                assert!(after[0].abs() <= limit && after[1].abs() <= limit, "{after:?}");
            }
            assert!(moved > 10.0, "bot {index} barely moved: {moved} m");
        }
    }
}
