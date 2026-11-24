//! Tick model benchmark (SRV-3): runs one model of the world tick at a fixed rate and reports tick durations.
//!
//!     tick-bench run <model> <scenario> <players> <rate_hz> <threads> <seconds> [unpaced]
//!     tick-bench check <scenario> <players> <rate_hz> <threads> <ticks>
//!
//! `run` prints one JSON line. `check` runs every model on the same inputs and fails unless all of them end in
//! the same world (same checksum). Models: current, soa, soa-rayon, zones, hecs-rayon, bevy, bevy-st.

mod ecs_bevy;
mod ecs_hecs;
mod logic;
mod sim;
mod soa;
mod zones;

use std::time::{Duration, Instant};

use logic::{Input, Params, PlayerState};
use sim::{Scenario, Sim};

/// One way of running the tick. Every model does the same work and must end in the same world.
pub trait Model {
    /// Hands the model this tick's inputs, outside the timed tick (models that keep their own inbox copy it).
    fn prepare(&mut self, _inputs: &[Input]) {}
    /// Runs one tick.
    fn tick(&mut self, inputs: &[Input], tick: u64);
    /// A hash of every player's state, in player order.
    fn checksum(&self) -> u64;
    /// Transfers applied so far.
    fn applied(&self) -> u64;
    /// Time per phase (ingest, grid, visibility, gameplay, transfers, snapshot), summed, if the model keeps it.
    fn phases(&self) -> Option<[Duration; 6]> {
        None
    }
    /// Mean visible-set and snapshot sizes at the end of the run, if the model reports them.
    fn sizes(&self) -> Option<(f64, f64)> {
        None
    }
}

const MODELS: [&str; 7] = [
    "current",
    "soa",
    "soa-rayon",
    "zones",
    "hecs-rayon",
    "bevy",
    "bevy-st",
];
const WARMUP_SECS: u32 = 3;

fn build(name: &str, scenario: Scenario, players: u32, rate: u32, threads: usize) -> (Sim, Box<dyn Model>) {
    let p = Params::new(rate, scenario.area());
    let mut sim = Sim::new(scenario, players, rate);
    let mut inputs = Vec::new();
    sim.inputs(&mut inputs);
    sim.advance();
    let states: Vec<PlayerState> = inputs
        .iter()
        .enumerate()
        .map(|(i, &inp)| PlayerState::new(i as u32, inp))
        .collect();
    let model: Box<dyn Model> = match name {
        "current" => Box::new(ecs_hecs::Current::new(p, rate, states)),
        "soa" => Box::new(soa::Soa::new(p, states, None)),
        "soa-rayon" => Box::new(soa::Soa::new(p, states, Some(threads))),
        "zones" => Box::new(zones::Zones::new(p, states, threads)),
        "hecs-rayon" => Box::new(ecs_hecs::HecsRayon::new(p, states, threads)),
        "bevy" => Box::new(ecs_bevy::Bevy::new(p, states, threads, false)),
        "bevy-st" => Box::new(ecs_bevy::Bevy::new(p, states, threads, true)),
        other => fail(&format!("unknown model {other}")),
    };
    (sim, model)
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2)
}

/// User plus system CPU time of the whole process (all threads), from /proc.
fn cpu_time() -> Duration {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // Fields after the command name, which is in parentheses and may contain spaces.
    let rest = stat.rsplit_once(") ").map(|(_, r)| r).unwrap_or("");
    let f: Vec<u64> = rest.split(' ').map(|x| x.parse().unwrap_or(0)).collect();
    // utime and stime are fields 14 and 15 of the line, 12 and 13 after the name; clock ticks of 10 ms.
    let ticks = f.get(11).copied().unwrap_or(0) + f.get(12).copied().unwrap_or(0);
    Duration::from_millis(ticks * 10)
}

fn percentile(sorted: &[Duration], q: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

#[allow(clippy::too_many_arguments)]
fn run(
    model_name: &str,
    scenario: Scenario,
    players: u32,
    rate: u32,
    threads: usize,
    secs: u32,
    paced: bool,
) {
    let (mut sim, mut model) = build(model_name, scenario, players, rate, threads);
    let interval = Duration::from_secs(1) / rate;
    let warmup = (WARMUP_SECS * rate) as u64;
    let total = warmup + u64::from(secs * rate);
    let mut inputs = Vec::new();
    let mut samples = Vec::with_capacity((total - warmup) as usize);
    let (mut gen_time, mut cpu0, mut wall0) = (Duration::ZERO, Duration::ZERO, Instant::now());
    let (mut late, mut dropped) = (0u64, 0u64);
    let mut phases0 = None;
    let mut next = Instant::now();
    for t in 0..total {
        if t == warmup {
            phases0 = model.phases();
            cpu0 = cpu_time();
            wall0 = Instant::now();
            gen_time = Duration::ZERO;
        }
        let g = Instant::now();
        sim.inputs(&mut inputs);
        model.prepare(&inputs);
        gen_time += g.elapsed();
        if paced {
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            }
        }
        let start = Instant::now();
        model.tick(&inputs, t);
        let d = start.elapsed();
        let g = Instant::now();
        sim.advance();
        gen_time += g.elapsed();
        if t >= warmup {
            samples.push(d);
            // A model far over budget would run for minutes: stop once it has used three times the planned time.
            if wall0.elapsed() > Duration::from_secs(u64::from(secs) * 3) && samples.len() >= 100 {
                break;
            }
            if d > interval {
                late += 1;
            }
        }
        next += interval;
        let now = Instant::now();
        if now > next + interval {
            // Same policy as TickLoop::run: more than a whole tick behind, skip the missed slots.
            if t >= warmup {
                dropped += ((now - next).as_nanos() / interval.as_nanos()) as u64;
            }
            next = now;
        }
    }
    let wall = wall0.elapsed();
    let cpu = cpu_time().saturating_sub(cpu0).saturating_sub(gen_time);
    let n = samples.len();
    let mean = samples.iter().sum::<Duration>() / n as u32;
    samples.sort_unstable();
    let budget = interval.mul_f64(0.7);
    let over = samples.iter().filter(|&&d| d > budget).count();
    let phases = match (phases0, model.phases()) {
        (Some(a), Some(b)) => {
            let v: Vec<String> = (0..6)
                .map(|i| format!("{:.1}", us(b[i] - a[i]) / n as f64))
                .collect();
            format!("[{}]", v.join(","))
        }
        _ => "null".into(),
    };
    let sizes = model
        .sizes()
        .map_or("null".into(), |(v, s)| format!("[{v:.1},{s:.1}]"));
    println!(
        "{{\"model\":\"{model_name}\",\"scenario\":\"{}\",\"players\":{players},\"rate_hz\":{rate},\"threads\":{threads},\"paced\":{paced},\
\"ticks\":{n},\"p50_us\":{:.1},\"p90_us\":{:.1},\"p99_us\":{:.1},\"max_us\":{:.1},\"mean_us\":{:.1},\"budget_us\":{:.1},\
\"over_budget\":{over},\"late\":{late},\"dropped_slots\":{dropped},\"cpu_ms_per_tick\":{:.3},\"wall_s\":{:.2},\
\"gen_us_per_tick\":{:.1},\"phases_us\":{phases},\"visible_snapshot\":{sizes},\"transfers\":{},\"checksum\":\"{:016x}\"}}",
        scenario.name(),
        us(percentile(&samples, 0.5)),
        us(percentile(&samples, 0.9)),
        us(percentile(&samples, 0.99)),
        us(samples[n - 1]),
        us(mean),
        us(budget),
        cpu.as_secs_f64() * 1e3 / n as f64,
        wall.as_secs_f64(),
        us(gen_time) / n as f64,
        model.applied(),
        model.checksum(),
    );
}

fn check(scenario: Scenario, players: u32, rate: u32, threads: usize, ticks: u64) {
    let mut sums = Vec::new();
    for name in MODELS {
        let (mut sim, mut model) = build(name, scenario, players, rate, threads);
        let mut inputs = Vec::new();
        for t in 0..ticks {
            sim.inputs(&mut inputs);
            model.prepare(&inputs);
            model.tick(&inputs, t);
            sim.advance();
        }
        let sum = model.checksum();
        println!(
            "{{\"check\":\"{name}\",\"scenario\":\"{}\",\"players\":{players},\"rate_hz\":{rate},\"threads\":{threads},\
\"ticks\":{ticks},\"transfers\":{},\"checksum\":\"{sum:016x}\"}}",
            scenario.name(),
            model.applied()
        );
        sums.push(sum);
    }
    if sums.windows(2).any(|w| w[0] != w[1]) {
        fail("models disagree");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |i: usize| -> u32 {
        args.get(i)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| fail("bad or missing number"))
    };
    let scenario = |i: usize| {
        args.get(i)
            .and_then(|s| Scenario::parse(s))
            .unwrap_or_else(|| fail("scenario: spread, city or crowd"))
    };
    match args.first().map(String::as_str) {
        Some("run") => {
            let model = args.get(1).cloned().unwrap_or_default();
            let paced = args.get(7).map(String::as_str) != Some("unpaced");
            run(
                &model,
                scenario(2),
                num(3),
                num(4),
                num(5) as usize,
                num(6),
                paced,
            );
        }
        Some("check") => check(scenario(1), num(2), num(3), num(4) as usize, u64::from(num(5))),
        _ => fail("usage: tick-bench run|check ..."),
    }
}
