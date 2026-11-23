//! The tick loop keeps its rate under light load (SRV-1).
//!
//! These tests sleep in real time. Bounds allow for a busy CI machine: the loop must never run fast, and may
//! run at most a quarter slower than its rate.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use sawgrass_server::{TickConfig, TickLoop};

#[derive(Debug, Clone, Copy, PartialEq)]
struct Position([f32; 3]);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Velocity([f32; 3]);

/// Players in one world, the architecture target.
const ENTITIES: usize = 4096;

/// Runs `tick_loop` for `ticks` ticks and returns the wall time it took.
fn run_ticks(tick_loop: &mut TickLoop, ticks: u64) -> Duration {
    let start = Instant::now();
    tick_loop.run(|l| {
        if l.tick() >= ticks {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    start.elapsed()
}

#[test]
fn keeps_rate_with_4096_moving_entities() {
    let config = TickConfig::new(50).expect("valid rate");
    let interval = config.interval();
    let mut tick_loop = TickLoop::new(config);
    for i in 0..ENTITIES {
        let x = i as f32;
        tick_loop
            .world_mut()
            .spawn((Position([x, 0.0, 0.0]), Velocity([1.0, 2.0, 0.0])));
    }
    tick_loop.add_system("movement", |world, info| {
        let dt = info.dt.as_secs_f32();
        for (pos, vel) in world.query_mut::<(&mut Position, &Velocity)>() {
            for axis in 0..3 {
                pos.0[axis] += vel.0[axis] * dt;
            }
        }
    });

    let ticks = 50;
    let elapsed = run_ticks(&mut tick_loop, ticks);
    let expected = interval * u32::try_from(ticks).expect("small");
    assert!(
        elapsed >= expected,
        "ran fast: {ticks} ticks in {elapsed:?}, expected {expected:?}"
    );
    assert!(
        elapsed <= expected * 5 / 4,
        "ran slow: {ticks} ticks in {elapsed:?}, expected {expected:?}"
    );

    let stats = tick_loop.metrics().stats();
    assert_eq!(stats.ticks, ticks);
    assert_eq!(stats.window, 50);
    assert!(stats.p99 < interval, "light load overran the tick: {stats}");

    // Every entity moved exactly `ticks` fixed steps, whatever the wall time.
    let mut checked = 0;
    for (pos, _) in tick_loop.world_mut().query_mut::<(&Position, &Velocity)>() {
        assert!(
            (pos.0[1] - 2.0).abs() < 1e-3,
            "y = {} after one simulated second",
            pos.0[1]
        );
        checked += 1;
    }
    assert_eq!(checked, ENTITIES);
}

#[test]
fn slow_tick_is_counted_and_not_made_up_in_a_burst() {
    let config = TickConfig::new(50).expect("valid rate");
    let interval = config.interval();
    let mut tick_loop = TickLoop::new(config);
    // The first tick takes four intervals; the rest are instant.
    tick_loop.add_system("stall once", move |_, info| {
        if info.tick == 0 {
            std::thread::sleep(interval * 4);
        }
    });

    let ticks = 10;
    let elapsed = run_ticks(&mut tick_loop, ticks);
    // Without catch-up: 4 intervals for the stall, then 9 more ticks at the normal rate.
    let expected = interval * 13;
    assert!(
        elapsed >= expected,
        "missed ticks were run back to back: {elapsed:?}, expected {expected:?}"
    );
    assert!(
        elapsed <= expected * 5 / 4,
        "ran slow: {elapsed:?}, expected {expected:?}"
    );
    assert_eq!(tick_loop.metrics().overruns(), 1);
}
