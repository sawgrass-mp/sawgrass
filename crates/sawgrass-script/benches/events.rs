//! What the runtime costs per event, and how long the built-ins Luau does not interrupt can block at the default
//! memory cap. Run with `cargo bench -p sawgrass-script`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use sawgrass_script::{Event, Limits, Resource, SORT_UNGUARDED};

fn resource(source: &str, limits: Limits) -> Resource {
    match Resource::from_sources("bench", &[("main.luau".into(), source.into())], limits) {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    }
}

/// Median time of one event, over `rounds` rounds of `n` events.
fn per_event(r: &mut Resource, event: Event, n: u32) -> Duration {
    let mut rounds: Vec<Duration> = (0..21)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..n {
                black_box(r.dispatch(black_box(event))).ok();
            }
            start.elapsed() / n
        })
        .collect();
    rounds.sort();
    rounds[rounds.len() / 2]
}

fn main() {
    let limits = Limits::default();
    let n = 100_000;
    let tick = Event::Tick { tick: 1, dt: 0.05 };
    println!("per event, median of 21 rounds of {n}:");
    let mut none = resource("", limits);
    println!(
        "  no handler                      {:>8.0?}",
        per_event(&mut none, tick, n)
    );
    let mut empty = resource("sawgrass.on('tick', function() end)", limits);
    println!(
        "  empty tick handler              {:>8.0?}",
        per_event(&mut empty, tick, n)
    );
    let mut join = resource(
        "local online = {} sawgrass.on('playerJoin', function(p) online[p] = true end)",
        limits,
    );
    println!(
        "  playerJoin, one table write     {:>8.0?}",
        per_event(&mut join, Event::PlayerJoin(7), n)
    );

    let cap = limits.memory;
    let slow = Limits {
        deadline: Duration::from_secs(30),
        ..limits
    };
    println!(
        "\nbuilt-ins the interrupt does not reach, at the {} MiB cap:",
        cap >> 20
    );
    let sort_input = |n: usize| {
        format!(
            "local t = {{}} for i = 1, {n} do t[i] = (i * 7919) % {} end",
            n + 7
        )
    };
    let cases = [
        (
            "string.rep, 48 MiB",
            String::new(),
            "string.rep('x', 48 * 1024 * 1024)",
        ),
        (
            "string.rep, 8 MiB",
            String::new(),
            "string.rep('x', 8 * 1024 * 1024)",
        ),
        (
            "a .. b, 2 x 24 MiB",
            "local a = string.rep('x', 24 * 1024 * 1024)".to_owned(),
            "a .. a",
        ),
        (
            "table.concat, 1M pieces",
            "local t = table.create(1000000, 'abcdefgh')".to_owned(),
            "table.concat(t)",
        ),
        (
            "table.concat, 2 pieces of 12 MiB",
            "local a = string.rep('x', 12 * 1024 * 1024) local t = {a, a}".to_owned(),
            "table.concat(t)",
        ),
        (
            "buffer.create, 48 MiB",
            String::new(),
            "buffer.create(48 * 1024 * 1024)",
        ),
        (
            "table.sort, largest unguarded",
            sort_input(SORT_UNGUARDED),
            "table.sort(t)",
        ),
        (
            "table.sort, 1M with the guard",
            sort_input(1_000_000),
            "table.sort(t)",
        ),
    ];
    for (name, setup, op) in cases {
        let mut r = resource(
            &format!(
                "sawgrass.on('tick', function() {setup} local start = os.clock() local _ = {op} print(os.clock() - start) end)"
            ),
            slow,
        );
        let result = r.dispatch(tick);
        let secs: f64 = r
            .drain_log()
            .first()
            .and_then(|l| l.parse().ok())
            .unwrap_or(f64::NAN);
        println!("  {name:<33} {:>8.2} ms  {result:?}", secs * 1000.0);
    }
}
