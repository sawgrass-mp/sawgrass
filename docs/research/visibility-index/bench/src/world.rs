//! The players and how they move. Every scenario has 4096 players at 20 Hz; the uniform part moves as in the
//! IM-1 benchmark (`crates/sawgrass-server/benches/visibility.rs`): one in four drives at up to 40 m/s, the rest
//! walk or run at up to 7 m/s, bouncing off the edges of their square.
//!
//! The `bots-*` scenarios move every player with the load-test bots' own [`Walker`] (waypoints, 8 hotspots that
//! draw 30% of them, pauses), over the square the bots' `--area` sets, so a run here can be set beside a run of
//! the real server under `sawgrass-bots` with the same area and seed.

use sawgrass_bots::walker::{Area, Walker};

const TICK: f32 = 1.0 / 20.0;

pub struct Scenario {
    pub name: &'static str,
    /// Edge of the square the uniform players move in, in metres.
    area: f32,
    /// Players gathered at the centre of the square instead: (count, radius of their disc in metres).
    /// Radius 0 means all of them on exactly the same point, standing still.
    gathered: (usize, f32),
}

/// Scenarios that move players with the bots' walker: (name, side of the square in metres).
pub const BOT_SCENARIOS: [(&str, f32); 2] = [("bots-4km", 4000.0), ("bots-1500m", 1500.0)];

pub const SCENARIOS: [Scenario; 6] = [
    Scenario {
        name: "spread",
        area: 8000.0,
        gathered: (0, 0.0),
    },
    Scenario {
        name: "city",
        area: 3000.0,
        gathered: (0, 0.0),
    },
    Scenario {
        name: "crowd-1km",
        area: 1000.0,
        gathered: (0, 0.0),
    },
    Scenario {
        name: "event-512",
        area: 3000.0,
        gathered: (512, 40.0),
    },
    Scenario {
        name: "event-2048",
        area: 3000.0,
        gathered: (2048, 60.0),
    },
    Scenario {
        name: "spawn-512",
        area: 3000.0,
        gathered: (512, 0.0),
    },
];

/// Xorshift: deterministic runs without a dependency.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

pub struct World {
    area: f32,
    centre: [f32; 2],
    /// The first `gathered` players stay within `disc` metres of the centre.
    gathered: usize,
    disc: f32,
    points: Vec<[f32; 2]>,
    velocity: Vec<[f32; 2]>,
    /// Set for a `bots-*` scenario: every player follows its own walker instead.
    walkers: Vec<Walker>,
}

impl World {
    /// # Panics
    ///
    /// On an unknown scenario name.
    pub fn new(name: &str, players: usize) -> Self {
        if let Some(&(_, size)) = BOT_SCENARIOS.iter().find(|(n, _)| *n == name) {
            return Self::bots(size, players);
        }
        let s = SCENARIOS
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("unknown scenario {name}"));
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let centre = [s.area / 2.0, s.area / 2.0];
        let (gathered, disc) = s.gathered;
        let mut points = Vec::with_capacity(players);
        let mut velocity = Vec::with_capacity(players);
        for i in 0..players {
            let angle = rng.unit() * std::f32::consts::TAU;
            if i < gathered {
                // Milling about at up to 1.5 m/s, spread evenly over the disc.
                let r = disc * rng.unit().sqrt();
                let a = rng.unit() * std::f32::consts::TAU;
                points.push([centre[0] + r * a.cos(), centre[1] + r * a.sin()]);
                let speed = if disc > 0.0 { 1.5 * rng.unit() } else { 0.0 };
                velocity.push([speed * angle.cos(), speed * angle.sin()]);
            } else {
                let speed = if i % 4 == 0 { 40.0 } else { 7.0 } * rng.unit();
                velocity.push([speed * angle.cos(), speed * angle.sin()]);
                points.push([rng.unit() * s.area, rng.unit() * s.area]);
            }
        }
        Self {
            area: s.area,
            centre,
            gathered,
            disc,
            points,
            velocity,
            walkers: Vec::new(),
        }
    }

    /// Players moved by the bots' walker, seed 1 as `sawgrass-bots run` uses by default, centred in the map.
    fn bots(size: f32, players: usize) -> Self {
        let area = Area {
            center: [4096.0, 4096.0],
            size,
            ..Area::default()
        };
        let walkers: Vec<Walker> = (0..players as u64).map(|i| Walker::new(area, i)).collect();
        let points = walkers
            .iter()
            .map(|w| [w.position()[0], w.position()[1]])
            .collect();
        Self {
            area: size,
            centre: area.center,
            gathered: 0,
            disc: 0.0,
            points,
            velocity: Vec::new(),
            walkers,
        }
    }

    pub fn points(&self) -> &[[f32; 2]] {
        &self.points
    }

    pub fn step(&mut self) {
        if !self.walkers.is_empty() {
            for (p, w) in self.points.iter_mut().zip(&mut self.walkers) {
                w.step(TICK);
                let [x, y, _] = w.position();
                *p = [x, y];
            }
            return;
        }
        for (i, (p, v)) in self.points.iter_mut().zip(&mut self.velocity).enumerate() {
            p[0] += v[0] * TICK;
            p[1] += v[1] * TICK;
            if i < self.gathered {
                let (dx, dy) = (p[0] - self.centre[0], p[1] - self.centre[1]);
                if dx * dx + dy * dy > self.disc * self.disc {
                    // Turn back towards the centre.
                    p[0] -= v[0] * TICK;
                    p[1] -= v[1] * TICK;
                    *v = [-v[0], -v[1]];
                }
                continue;
            }
            for a in 0..2 {
                if !(0.0..=self.area).contains(&p[a]) {
                    v[a] = -v[a];
                    p[a] = p[a].clamp(0.0, self.area);
                }
            }
        }
    }
}
