//! Benchmark for the per-player phase of the tick (SRV-7): 4096 players move, the dense grid is rebuilt, and every
//! player's view builds its visible set and delta snapshot datagrams, on 1, 2 and 4 threads. Reports the time per 20 Hz
//! tick (p50 and p99) for a city and a crowd. The network is left out: this is the tick thread's part.
//!
//!     cargo bench -p sawgrass-server --bench snapshots
//!
//! Without `--bench` (as under `cargo test --benches`) it runs a small smoke version so CI stays fast.

use std::time::{Duration, Instant};

use rayon::prelude::*;
use sawgrass_protocol::{EntityState, Position, Rotation, Velocity};
use sawgrass_server::interest::{DenseGrid, EntityId, Point, VisibilityConfig};
use sawgrass_server::snapshot::{History, View};

const TICK_SECONDS: f32 = 1.0 / 20.0;

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

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Runs `ticks` ticks of `players` players in a square of `area` metres on `threads` threads; returns p50, p99 and
/// the mean number of datagrams per player.
fn run(players: usize, area: f32, threads: usize, ticks: usize) -> (Duration, Duration, f64) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("pool");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut movers: Vec<([f32; 2], [f32; 2])> = (0..players)
        .map(|_| {
            let speed = if rng.unit() < 0.3 { 20.0 } else { 1.5 };
            let angle = rng.unit() * std::f32::consts::TAU;
            (
                [rng.unit() * area, rng.unit() * area],
                [speed * angle.cos(), speed * angle.sin()],
            )
        })
        .collect();
    let mut grid = DenseGrid::default();
    let mut states = vec![None; players];
    let mut history = History::new();
    let mut views: Vec<View> = (0..players).map(|_| View::new()).collect();
    let config = VisibilityConfig::default();
    let mut times = Vec::with_capacity(ticks);
    let mut datagrams = 0usize;
    for tick in 0..ticks {
        for (id, (p, v)) in movers.iter_mut().enumerate() {
            for axis in 0..2 {
                p[axis] += v[axis] * TICK_SECONDS;
                if p[axis] < 0.0 || p[axis] > area {
                    v[axis] = -v[axis];
                    p[axis] = p[axis].clamp(0.0, area);
                }
            }
            states[id] = Some(EntityState {
                id: id as u32,
                position: Position::from_metres([p[0], p[1], 10.0]).expect("position"),
                velocity: Velocity::from_metres_per_second([v[0], v[1], 0.0]).expect("velocity"),
                rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
            });
        }
        let start = Instant::now();
        for (id, (p, _)) in movers.iter().enumerate() {
            grid.upsert(EntityId(id as u32), Point::new(p[0], p[1]));
        }
        grid.rebuild();
        let tick = tick as u32 + 1;
        history.record(tick, &states);
        pool.install(|| {
            views
                .par_iter_mut()
                .enumerate()
                // Every client acknowledged the previous tick, as on a link without loss.
                .for_each(|(me, view)| view.build(me, &grid, &history, &config, tick, tick - 1));
        });
        times.push(start.elapsed());
        // The gateway's part: take the datagrams and drop them once sent.
        datagrams += views.iter_mut().map(|v| v.out().count()).sum::<usize>();
    }
    times.sort_unstable();
    (
        percentile(&times, 0.5),
        percentile(&times, 0.99),
        datagrams as f64 / (ticks * players) as f64,
    )
}

fn main() {
    let full = std::env::args().any(|a| a == "--bench");
    let (players, ticks) = if full { (4096, 200) } else { (256, 5) };
    for (name, area) in [("city, 3 km square", 3000.0), ("crowd, 1.5 km square", 1500.0)] {
        for threads in [1, 2, 4] {
            let (p50, p99, datagrams) = run(players, area, threads, ticks);
            println!(
                "{players} players, {name}, {threads} threads: tick p50 {:.1} ms, p99 {:.1} ms, \
                 {datagrams:.1} datagrams per player",
                p50.as_secs_f64() * 1e3,
                p99.as_secs_f64() * 1e3
            );
        }
    }
}
