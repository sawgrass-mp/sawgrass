//! The clients: where each player moves and what state it reports every tick.
//!
//! Runs outside the timed tick, on the main thread, like a gateway handing the world its inbox.

use sawgrass_server::interest::Point;

use crate::logic::{Input, mix};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Everyone spread over an 8 km square.
    Spread,
    /// Everyone in a 3 km square, a city.
    City,
    /// The city, with a quarter of the players at one event, within 100 m of a point.
    Crowd,
}

impl Scenario {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "spread" => Self::Spread,
            "city" => Self::City,
            "crowd" => Self::Crowd,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Spread => "spread",
            Self::City => "city",
            Self::Crowd => "crowd",
        }
    }

    pub fn area(self) -> f32 {
        match self {
            Self::Spread => 8000.0,
            Self::City | Self::Crowd => 3000.0,
        }
    }
}

const CROWD_RADIUS: f32 = 100.0;

struct Mover {
    pos: Point,
    vel: Point,
    speed: f32,
    in_crowd: bool,
}

pub struct Sim {
    movers: Vec<Mover>,
    area: f32,
    centre: Point,
    dt: f32,
    tick: u64,
    rng: u64,
}

impl Sim {
    pub fn new(scenario: Scenario, players: u32, rate_hz: u32) -> Self {
        let area = scenario.area();
        let centre = Point::new(area / 2.0, area / 2.0);
        let mut sim = Self {
            movers: Vec::with_capacity(players as usize),
            area,
            centre,
            dt: 1.0 / rate_hz as f32,
            tick: 0,
            rng: 0x2545_F491_4F6C_DD1D,
        };
        for i in 0..players {
            let in_crowd = scenario == Scenario::Crowd && i % 4 == 0;
            let (pos, speed) = if in_crowd {
                let a = sim.unit() * std::f32::consts::TAU;
                let r = CROWD_RADIUS * sim.unit().sqrt();
                (
                    Point::new(centre.x + r * a.cos(), centre.y + r * a.sin()),
                    2.0 * sim.unit(),
                )
            } else {
                // One in four drives (up to 40 m/s), the rest walk or run (up to 7 m/s), as in the IM-1 benchmark.
                let top = if i % 4 == 1 { 40.0 } else { 7.0 };
                (Point::new(sim.unit() * area, sim.unit() * area), top * sim.unit())
            };
            let mut m = Mover {
                pos,
                vel: Point::default(),
                speed,
                in_crowd,
            };
            let a = sim.unit() * std::f32::consts::TAU;
            m.vel = Point::new(speed * a.cos(), speed * a.sin());
            sim.movers.push(m);
        }
        sim
    }

    fn unit(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 40) as f32 / (1u64 << 24) as f32
    }

    /// The states the clients report for the current tick, then moves them one tick on.
    pub fn inputs(&self, out: &mut Vec<Input>) {
        out.clear();
        out.extend(self.movers.iter().enumerate().map(|(i, m)| {
            let mut pos = m.pos;
            // One state in 20,000 claims a 300 m jump, which validation must reject.
            if mix(i as u64, self.tick ^ 0xC0FFEE) % 20_000 == 0 {
                pos.x = (pos.x + 300.0).min(self.area);
            }
            Input { pos, vel: m.vel }
        }));
    }

    pub fn advance(&mut self) {
        self.tick += 1;
        let (area, centre, dt, tick) = (self.area, self.centre, self.dt, self.tick);
        for (i, m) in self.movers.iter_mut().enumerate() {
            // Each mover picks a new heading about every 5 seconds.
            let r = mix(i as u64 ^ 0xABCD, tick);
            if r % ((5.0 / dt) as u64).max(1) == 0 {
                let a = (r >> 16) as f32 / (1u64 << 48) as f32 * std::f32::consts::TAU;
                m.vel = Point::new(m.speed * a.cos(), m.speed * a.sin());
            }
            m.pos.x += m.vel.x * dt;
            m.pos.y += m.vel.y * dt;
            if m.in_crowd {
                if m.pos.distance_squared(centre) > CROWD_RADIUS * CROWD_RADIUS {
                    m.vel = Point::new(-m.vel.x, -m.vel.y);
                    m.pos.x += 2.0 * m.vel.x * dt;
                    m.pos.y += 2.0 * m.vel.y * dt;
                }
            } else {
                if !(0.0..=area).contains(&m.pos.x) {
                    m.vel.x = -m.vel.x;
                    m.pos.x = m.pos.x.clamp(0.0, area);
                }
                if !(0.0..=area).contains(&m.pos.y) {
                    m.vel.y = -m.vel.y;
                    m.pos.y = m.pos.y.clamp(0.0, area);
                }
            }
        }
    }
}
