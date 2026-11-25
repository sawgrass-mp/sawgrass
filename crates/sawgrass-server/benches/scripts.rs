//! The `scripts` phase against the tick budget: several resources, well-behaved and misbehaving, at the default
//! limits (4 ms per call, 16 MiB) and script budget (8 ms). Run with `cargo bench -p sawgrass-server --bench
//! scripts`.
//!
//! Columns: the phase's p50, p99 and max over the measured ticks; `join ticks`, how many ticks the 4,096 joins
//! took to deliver; `skipped`, tick events skipped for the budget over the measured ticks (out of 400 per
//! resource).

use std::time::{Duration, Instant};

use sawgrass_script::{Host, Limits, Resource};
use sawgrass_server::scripts::DEFAULT_BUDGET;

const PLAYERS: u32 = 4096;
const TICKS: u64 = 400;
/// Ticks before measuring: 4,096 joins per resource take a few ticks of budget to deliver.
const WARM_UP: u64 = 20;

/// A typical resource: keeps a record per player and walks all of them every tick (a job timer).
const TYPICAL: &str = "
    local players = {}
    sawgrass.on('playerJoin', function(p) players[p] = { paid = 0, timer = 0 } end)
    sawgrass.on('playerLeave', function(p) players[p] = nil end)
    sawgrass.on('tick', function(tick, dt)
        for _, rec in players do
            rec.timer += dt
            if rec.timer >= 60 then rec.timer = 0 rec.paid += 1 end
        end
    end)";

const LOOP: &str = "sawgrass.on('tick', function() while true do end end)";
const MEMORY: &str = "sawgrass.on('tick', function() local t = {} while true do t[#t + 1] = {} end end)";
/// A stall the interrupt cannot cut, every tick: a 7 MiB string, so that the last tick's string, not yet collected,
/// and this one both fit under the 16 MiB cap. Its length changes every tick because Luau interns strings: the
/// same string again would cost nothing.
const STALL: &str =
    "sawgrass.on('tick', function(tick) local s = string.rep('x', 7 * 1024 * 1024 - tick % 1024) end)";

fn host(sources: &[&str]) -> Host {
    let mut host = Host::default();
    for (i, source) in sources.iter().enumerate() {
        let name = format!("r{i}");
        match Resource::from_sources(
            &name,
            &[("main.luau".into(), (*source).into())],
            Limits::default(),
        ) {
            Ok(r) => host.push(r),
            Err(e) => panic!("{e}"),
        }
    }
    host
}

fn run(name: &str, sources: &[&str]) {
    let mut host = host(sources);
    let joined: Vec<u32> = (1..=PLAYERS).collect();
    let mut times = Vec::with_capacity(TICKS as usize);
    let mut skipped = 0;
    let mut catching_up = 0;
    for tick in 0..WARM_UP + TICKS {
        let join = if tick == 0 { &joined[..] } else { &[] };
        let start = Instant::now();
        let report = host.run_tick(tick, 0.05, join, &[], DEFAULT_BUDGET);
        if tick < WARM_UP {
            catching_up += u64::from(report.skipped > 0 || tick == 0);
            continue;
        }
        times.push(start.elapsed());
        skipped += report.skipped;
    }
    times.sort();
    let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "{name:<34} {:>6.2} {:>6.2} {:>6.2} {:>8} {:>8}",
        ms(at(0.5)),
        ms(at(0.99)),
        ms(times[times.len() - 1]),
        catching_up,
        skipped
    );
}

fn main() {
    println!("scripts phase, {TICKS} ticks, {PLAYERS} players, budget {DEFAULT_BUDGET:?}; times in ms");
    println!(
        "{:<34} {:>6} {:>6} {:>6} {:>8} {:>8}",
        "resources", "p50", "p99", "max", "join ticks", "skipped"
    );
    let typical = [TYPICAL; 8];
    run("8 typical", &typical);
    run("8 typical + endless loop", &[&typical[..], &[LOOP]].concat());
    run("8 typical + memory hog", &[&typical[..], &[MEMORY]].concat());
    run("8 typical + 7 MiB stall", &[&typical[..], &[STALL]].concat());
    run(
        "8 typical + loop, hog and stall",
        &[&typical[..], &[LOOP, MEMORY, STALL]].concat(),
    );
    run("8 typical + 3 stalls", &[&typical[..], &[STALL; 3]].concat());
}
