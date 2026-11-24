//! Scripting engine comparison for roleplay logic (task SCRIPT-1). One engine per build, chosen by a cargo feature;
//! `../run.py` builds each and runs the suites in fresh processes. Prints one JSON object per result line.
//!
//! Usage: scripting-bench verify | speed [--no-limits] | latency | memory | isolation <spin|bomb|recurse|fail|slow_builtin>

mod host;
#[cfg(any(feature = "luau", feature = "lua54", feature = "luajit"))]
mod lua;
#[cfg(feature = "quickjs")]
mod quickjs;
#[cfg(feature = "rhai")]
mod rhai;
#[cfg(feature = "v8")]
mod v8;
#[cfg(feature = "wasm")]
mod wasm;

use host::{Script, Shared};
use std::hint::black_box;
use std::time::{Duration, Instant};

#[cfg(all(feature = "luau", not(feature = "luau-jit")))]
const ENGINE: &str = "luau";
#[cfg(feature = "luau-jit")]
const ENGINE: &str = "luau-jit";
#[cfg(feature = "lua54")]
const ENGINE: &str = "lua54";
#[cfg(all(feature = "luajit", not(feature = "luajit-interp")))]
const ENGINE: &str = "luajit";
#[cfg(feature = "luajit-interp")]
const ENGINE: &str = "luajit-interp";
#[cfg(feature = "quickjs")]
const ENGINE: &str = "quickjs";
#[cfg(feature = "v8")]
const ENGINE: &str = "v8";
#[cfg(feature = "rhai")]
const ENGINE: &str = "rhai";
#[cfg(feature = "wasm")]
const ENGINE: &str = "wasm";

#[cfg(any(feature = "luau", feature = "lua54", feature = "luajit"))]
type E = lua::Engine;
#[cfg(feature = "quickjs")]
type E = quickjs::Engine;
#[cfg(feature = "v8")]
type E = v8::Engine;
#[cfg(feature = "rhai")]
type E = rhai::Engine;
#[cfg(feature = "wasm")]
type E = wasm::Engine;

const LINES: [&str; 5] = [
    "/pay 17 250",
    "/me waves at the crowd",
    "/roll",
    "/pay 4000 999999999",
    "/give 3 water 2",
];
const ECHO: &str = "player 1234 says hello to you!!";
const INSTANCES: usize = 100;

/// Player positions on the 4 km square of the zones; most fall in no zone, so most calls check all 64.
fn positions() -> Vec<(f64, f64)> {
    let mut s: i64 = 7;
    let mut next = || {
        s = (s * 16807) % 2147483647;
        (s % 4000 - 2000) as f64
    };
    (0..host::PLAYERS).map(|_| (next(), next())).collect()
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars().take(160) {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn fnv(acc: u64, s: &str) -> u64 {
    s.bytes()
        .fold(acc, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn new_engine(limits: bool) -> (E, Shared) {
    let world = Shared::default();
    match E::new(world.clone(), limits) {
        Ok(e) => (e, world),
        Err(e) => {
            eprintln!("{ENGINE}: cannot create an instance: {e}");
            std::process::exit(2);
        }
    }
}

fn ok<T>(r: Result<T, String>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{ENGINE}: script error: {e}");
            std::process::exit(3);
        }
    }
}

/// Same calls on every engine; run.py checks that the results agree.
fn verify() {
    host::arm(Duration::from_secs(60));
    let (mut e, world) = new_engine(true);
    let noop = ok(e.noop(41));
    let host_loop = ok(e.host_loop(1000));
    let echo = ok(e.echo("hello"));
    let zone: i64 = positions().iter().map(|&(x, y)| ok(e.zone(x, y))).sum();
    let mut chat = 0xcbf29ce484222325u64;
    let mut sample = Vec::new();
    for from in 0..100 {
        for line in LINES {
            let r = ok(e.chat(from, line));
            chat = fnv(chat, &r);
            if from == 3 {
                sample.push(json_str(&r));
            }
        }
    }
    let payday: i64 = (0..3).map(|_| ok(e.payday())).sum();
    let inventory: i64 = (1..=1000).map(|s| ok(e.inventory(s))).sum();
    let checksum = e.checksum(&world);
    println!(
        "{{\"suite\":\"verify\",\"engine\":\"{ENGINE}\",\"noop\":{noop},\"host_loop\":{host_loop},\"echo\":{},\
         \"zone\":{zone},\"chat\":\"{chat:016x}\",\"chat_sample\":[{}],\"payday\":{payday},\"inventory\":{inventory},\
         \"checksum\":{checksum}}}",
        json_str(&echo),
        sample.join(",")
    );
}

/// Median nanoseconds per call of `f` over five rounds of about 200 ms, after calibration.
fn time(mut f: impl FnMut(usize)) -> (f64, usize) {
    let mut n = 1usize;
    loop {
        let t = Instant::now();
        for i in 0..n {
            f(i);
        }
        if t.elapsed() > Duration::from_millis(50) {
            break;
        }
        n *= 2;
    }
    let mut rounds: Vec<f64> = (0..5)
        .map(|_| {
            let t = Instant::now();
            for i in 0..n * 4 {
                f(i);
            }
            t.elapsed().as_nanos() as f64 / (n * 4) as f64
        })
        .collect();
    rounds.sort_by(f64::total_cmp);
    (rounds[2], n * 4)
}

fn speed(limits: bool) {
    host::arm(Duration::from_secs(3600));
    let (mut e, _world) = new_engine(limits);
    let pos = positions();
    let emit = |workload: &str, (ns, ops): (f64, usize)| {
        println!(
            "{{\"suite\":\"speed\",\"engine\":\"{ENGINE}\",\"limits\":{limits},\"workload\":\"{workload}\",\
             \"ns\":{ns:.1},\"calls_per_round\":{ops}}}"
        );
    };
    emit(
        "noop",
        time(|i| {
            black_box(ok(e.noop(i as i64)));
        }),
    );
    emit(
        "host_loop_1000",
        time(|_| {
            black_box(ok(e.host_loop(1000)));
        }),
    );
    emit(
        "echo",
        time(|_| {
            black_box(ok(e.echo(ECHO)));
        }),
    );
    emit(
        "zone",
        time(|i| {
            black_box(ok(e.zone(pos[i % pos.len()].0, pos[i % pos.len()].1)));
        }),
    );
    emit(
        "chat",
        time(|i| {
            black_box(ok(e.chat((i % 4096) as i64, LINES[i % LINES.len()])));
        }),
    );
    emit(
        "payday",
        time(|_| {
            black_box(ok(e.payday()));
        }),
    );
    emit(
        "inventory",
        time(|i| {
            black_box(ok(e.inventory(i as i64 + 1)));
        }),
    );
}

fn percentiles(mut v: Vec<f64>) -> String {
    v.sort_by(f64::total_cmp);
    let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    format!(
        "\"p50\":{:.1},\"p99\":{:.1},\"p999\":{:.1},\"max\":{:.1},\"calls\":{}",
        p(0.5),
        p(0.99),
        p(0.999),
        v[v.len() - 1],
        v.len()
    )
}

/// Per-call times, to see pauses (garbage collection) that a median hides.
fn latency() {
    host::arm(Duration::from_secs(3600));
    let (mut e, _world) = new_engine(true);
    let pos = positions();
    let mut run = |workload: &str, calls: usize, f: &mut dyn FnMut(&mut E, usize)| {
        for i in 0..calls / 10 {
            f(&mut e, i);
        }
        let v: Vec<f64> = (0..calls)
            .map(|i| {
                let t = Instant::now();
                f(&mut e, i);
                t.elapsed().as_nanos() as f64
            })
            .collect();
        println!(
            "{{\"suite\":\"latency\",\"engine\":\"{ENGINE}\",\"workload\":\"{workload}\",{}}}",
            percentiles(v)
        );
    };
    run("zone", 200_000, &mut |e, i| {
        black_box(ok(e.zone(pos[i % pos.len()].0, pos[i % pos.len()].1)));
    });
    run("chat", 100_000, &mut |e, i| {
        black_box(ok(e.chat((i % 4096) as i64, LINES[i % LINES.len()])));
    });
    run("inventory", 100_000, &mut |e, i| {
        black_box(ok(e.inventory(i as i64 + 1)));
    });
    run("payday", 1_000, &mut |e, _| {
        black_box(ok(e.payday()));
    });
}

fn rss() -> usize {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<usize>().ok()))
        .unwrap_or(0)
        * 4096
}

fn warm(e: &mut E) {
    ok(e.noop(1));
    ok(e.host_loop(10));
    ok(e.echo(ECHO));
    ok(e.zone(0.0, 0.0));
    ok(e.chat(1, LINES[0]));
    ok(e.payday());
    ok(e.inventory(1));
}

/// Time to create an instance with the resource loaded, and memory per instance after one call of each workload.
fn memory() {
    host::arm(Duration::from_secs(3600));
    let r0 = rss();
    let t = Instant::now();
    let (mut first, first_world) = new_engine(true);
    let first_ms = t.elapsed().as_secs_f64() * 1e3;
    warm(&mut first);
    let r1 = rss();
    let mut times = Vec::new();
    let mut all = Vec::new();
    for _ in 0..INSTANCES {
        let t = Instant::now();
        let (mut e, w) = new_engine(true);
        times.push(t.elapsed().as_secs_f64() * 1e3);
        warm(&mut e);
        all.push((e, w));
    }
    let r2 = rss();
    times.sort_by(f64::total_cmp);
    #[cfg(feature = "wasm")]
    let compile = wasm::compile_ms().map_or("null".to_string(), |v| format!("{v:.2}"));
    #[cfg(not(feature = "wasm"))]
    let compile = "null";
    println!(
        "{{\"suite\":\"memory\",\"engine\":\"{ENGINE}\",\"first_ms\":{first_ms:.2},\"first_rss\":{},\
         \"create_ms_p50\":{:.3},\"create_ms_max\":{:.3},\"rss_per_instance\":{},\"instances\":{INSTANCES},\
         \"module_compile_ms\":{compile}}}",
        r1.saturating_sub(r0),
        times[times.len() / 2],
        times[times.len() - 1],
        r2.saturating_sub(r1) / INSTANCES
    );
    // Dropped newest first: V8 requires isolates to be dropped in the reverse order of their creation.
    while let Some(instance) = all.pop() {
        drop(black_box(instance));
    }
    drop(first);
    drop(first_world);
}

fn peak_rss() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<usize>().ok()))
        })
        .unwrap_or(0)
        * 1024
}

/// One misbehaving call. If the process dies or hangs, run.py records that instead.
fn isolation(case: &str) {
    host::arm(Duration::from_secs(10));
    let (mut e, _world) = new_engine(true);
    // The endless loop and the long library call get a 10 ms deadline; the others are stopped by their limits.
    let budget = if case == "spin" || case == "slow_builtin" {
        Duration::from_millis(10)
    } else {
        Duration::from_secs(10)
    };
    host::arm(budget);
    let t = Instant::now();
    let r = e.misbehave(case);
    let ms = t.elapsed().as_secs_f64() * 1e3;
    e.recover();
    host::arm(Duration::from_secs(10));
    let usable = e.noop(1) == Ok(2);
    let (outcome, message) = match &r {
        Ok(()) => ("returned", String::new()),
        Err(m) => ("error", m.clone()),
    };
    println!(
        "{{\"suite\":\"isolation\",\"engine\":\"{ENGINE}\",\"case\":\"{case}\",\"outcome\":\"{outcome}\",\
         \"message\":{},\"ms\":{ms:.2},\"usable_after\":{usable},\"memory_capped\":{},\"peak_rss\":{}}}",
        json_str(&message),
        e.memory_capped(),
        peak_rss()
    );
}

fn main() {
    host::start_watchdog();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let limits = !args.iter().any(|a| a == "--no-limits");
    match args.first().map(String::as_str) {
        Some("verify") => verify(),
        Some("speed") => speed(limits),
        Some("latency") => latency(),
        Some("memory") => memory(),
        Some("isolation") => isolation(args.get(1).map_or("spin", String::as_str)),
        _ => {
            eprintln!(
                "usage: scripting-bench verify | speed [--no-limits] | latency | memory | isolation <case>"
            );
            std::process::exit(1);
        }
    }
}
