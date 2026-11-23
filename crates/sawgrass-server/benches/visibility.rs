//! Benchmark for the spatial grid and visible sets (IM-1).
//!
//! Moves 4096 points at walking and driving speeds, then for every point updates the grid and recomputes its
//! visible set, once per 20 Hz tick. Reports the time per tick (p50, p99, mean) for several densities.
//!
//!     cargo bench -p sawgrass-server --bench visibility
//!
//! Without `--bench` (as under `cargo test --benches`) it runs a small smoke version so CI stays fast.

use std::time::{Duration, Instant};

use sawgrass_server::interest::{
    EntityId, Point, SpatialGrid, VisibilityChanges, VisibilityConfig, VisibleSet,
};

const TICK_SECONDS: f32 = 1.0 / 20.0;

struct Scenario {
    name: &'static str,
    /// Edge of the square the points move in, in metres.
    area: f32,
}

const SCENARIOS: [Scenario; 3] = [
    Scenario {
        name: "spread (8 km square)",
        area: 8000.0,
    },
    Scenario {
        name: "city (3 km square)",
        area: 3000.0,
    },
    Scenario {
        name: "crowd (1 km square)",
        area: 1000.0,
    },
];

/// Xorshift generator: deterministic runs without a dependency.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

struct Mover {
    position: Point,
    velocity: Point,
}

struct Report {
    p50: Duration,
    p99: Duration,
    mean: Duration,
    visible: f64,
    changes: f64,
}

fn run(scenario: &Scenario, points: u32, warmup: usize, ticks: usize) -> Report {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut movers: Vec<Mover> = (0..points)
        .map(|i| {
            // One in four drives (up to 40 m/s), the rest walk or run (up to 7 m/s).
            let speed = if i % 4 == 0 { 40.0 } else { 7.0 } * rng.unit();
            let angle = rng.unit() * std::f32::consts::TAU;
            Mover {
                position: Point::new(rng.unit() * scenario.area, rng.unit() * scenario.area),
                velocity: Point::new(speed * angle.cos(), speed * angle.sin()),
            }
        })
        .collect();
    let config = VisibilityConfig::default();
    let mut grid = SpatialGrid::default();
    let mut sets: Vec<VisibleSet> = (0..points).map(|_| VisibleSet::new()).collect();
    let mut changes = VisibilityChanges::default();
    let mut samples = Vec::with_capacity(ticks);
    let (mut visible, mut changed) = (0u64, 0u64);

    for tick in 0..warmup + ticks {
        for mover in &mut movers {
            let p = &mut mover.position;
            let v = &mut mover.velocity;
            p.x += v.x * TICK_SECONDS;
            p.y += v.y * TICK_SECONDS;
            if !(0.0..=scenario.area).contains(&p.x) {
                v.x = -v.x;
                p.x = p.x.clamp(0.0, scenario.area);
            }
            if !(0.0..=scenario.area).contains(&p.y) {
                v.y = -v.y;
                p.y = p.y.clamp(0.0, scenario.area);
            }
        }

        let start = Instant::now();
        for (i, mover) in movers.iter().enumerate() {
            grid.upsert(EntityId(i as u32), mover.position);
        }
        let mut tick_changes = 0;
        for (i, (mover, set)) in movers.iter().zip(&mut sets).enumerate() {
            set.update(
                &grid,
                Some(EntityId(i as u32)),
                mover.position,
                &config,
                &mut changes,
            );
            tick_changes += changes.entered.len() + changes.left.len();
        }
        let elapsed = start.elapsed();

        if tick >= warmup {
            samples.push(elapsed);
            visible += sets.iter().map(|s| s.len() as u64).sum::<u64>();
            changed += tick_changes as u64;
        }
    }

    samples.sort_unstable();
    let percentile = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    let per_viewer_tick = f64::from(points) * ticks as f64;
    Report {
        p50: percentile(0.50),
        p99: percentile(0.99),
        mean: samples.iter().sum::<Duration>() / ticks as u32,
        visible: visible as f64 / per_viewer_tick,
        changes: changed as f64 / per_viewer_tick,
    }
}

fn main() {
    let full = std::env::args().any(|arg| arg == "--bench");
    let (points, warmup, ticks) = if full { (4096, 40, 400) } else { (256, 2, 5) };
    let config = VisibilityConfig::default();
    println!(
        "visibility: {points} points, {ticks} ticks at 20 Hz, cell {} m, enter {} m, leave {} m",
        sawgrass_server::interest::DEFAULT_CELL_SIZE,
        config.enter_radius(),
        config.leave_radius()
    );
    println!(
        "{:<22} {:>10} {:>10} {:>10} {:>12} {:>16}",
        "scenario", "p50", "p99", "mean", "visible avg", "changes/viewer"
    );
    for scenario in &SCENARIOS {
        let r = run(scenario, points, warmup, ticks);
        println!(
            "{:<22} {:>10.3?} {:>10.3?} {:>10.3?} {:>12.1} {:>16.3}",
            scenario.name, r.p50, r.p99, r.mean, r.visible, r.changes
        );
    }
}
