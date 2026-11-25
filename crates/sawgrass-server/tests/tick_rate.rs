//! The tick loop keeps its rate (SRV-1, SRV-2).
//!
//! The schedule tests run on a simulated clock, so their bounds are exact and a busy CI machine cannot fail
//! them. One test runs on the real clock to check that the loop really sleeps: it must never run fast, and
//! its upper bound only catches a loop that hangs.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sawgrass_server::{BETWEEN, Clock, TickConfig, TickLoop};

#[derive(Debug, Clone, Copy, PartialEq)]
struct Position([f32; 3]);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Velocity([f32; 3]);

/// Players in one world, the architecture target.
const ENTITIES: usize = 4096;

/// A simulated clock: time moves only when the loop sleeps or a test calls [`SimClock::advance`]. Clones
/// share the same time, so systems and hooks can spend simulated time.
#[derive(Debug, Clone)]
struct SimClock {
    origin: Instant,
    elapsed: Arc<Mutex<Duration>>,
}

impl SimClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            elapsed: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    /// Simulated time since the clock was created.
    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().expect("clock lock")
    }

    /// Spends `by` of simulated time, as work would.
    fn advance(&self, by: Duration) {
        *self.elapsed.lock().expect("clock lock") += by;
    }
}

impl Clock for SimClock {
    fn now(&self) -> Instant {
        self.origin + self.elapsed()
    }

    fn sleep_until(&mut self, deadline: Instant) {
        let mut elapsed = self.elapsed.lock().expect("clock lock");
        *elapsed = (*elapsed).max(deadline - self.origin);
    }
}

/// A `between` hook that stops the loop after `ticks` ticks.
fn stop_after(ticks: u64) -> impl FnMut(&mut TickLoop) -> ControlFlow<()> {
    move |l| {
        if l.tick() >= ticks {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// A loop at 50 Hz with `ENTITIES` entities and a movement system.
fn moving_world() -> TickLoop {
    let mut tick_loop = TickLoop::new(TickConfig::new(50).expect("valid rate"));
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
    tick_loop
}

/// Every entity moved exactly one simulated second of fixed steps, whatever the wall time.
fn assert_moved_one_second(tick_loop: &mut TickLoop) {
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
fn keeps_rate_with_4096_moving_entities() {
    let mut tick_loop = moving_world();
    let interval = tick_loop.config().interval();
    let mut clock = SimClock::new();
    let work = clock.clone();
    // Each tick costs a tenth of the interval.
    tick_loop.add_system("work", move |_, _| work.advance(interval / 10));

    let ticks = 50;
    tick_loop
        .run_with_clock(&mut clock, stop_after(ticks))
        .expect("no panic");
    assert_eq!(clock.elapsed(), interval * 50);

    let stats = tick_loop.metrics().stats();
    assert_eq!((stats.ticks, stats.window, stats.overruns), (ticks, 50, 0));
    assert_eq!(stats.p99, interval / 10);
    assert!(stats.within_budget(), "{stats}");
    assert_moved_one_second(&mut tick_loop);
}

#[test]
fn slow_tick_is_counted_and_not_made_up_in_a_burst() {
    let mut tick_loop = TickLoop::new(TickConfig::new(50).expect("valid rate"));
    let interval = tick_loop.config().interval();
    let mut clock = SimClock::new();
    let work = clock.clone();
    let starts = Arc::new(Mutex::new(Vec::new()));
    let log = starts.clone();
    // The first tick takes four intervals; the rest are instant.
    tick_loop.add_system("stall once", move |_, info| {
        log.lock().expect("log lock").push(work.elapsed());
        if info.tick == 0 {
            work.advance(interval * 4);
        }
    });

    tick_loop
        .run_with_clock(&mut clock, stop_after(10))
        .expect("no panic");
    // Without catch-up: 4 intervals for the stall, then 9 more ticks at the normal rate.
    assert_eq!(clock.elapsed(), interval * 13);
    let starts = starts.lock().expect("log lock").clone();
    let expected: Vec<Duration> = std::iter::once(Duration::ZERO)
        .chain((4..13).map(|n| interval * n))
        .collect();
    assert_eq!(starts, expected, "missed ticks were run back to back");
    assert_eq!(tick_loop.metrics().overruns(), 1);
}

#[test]
fn between_hook_time_counts_in_the_tick() {
    let mut tick_loop = TickLoop::new(TickConfig::new(50).expect("valid rate"));
    let interval = tick_loop.config().interval();
    let mut clock = SimClock::new();
    let work = clock.clone();
    tick_loop.add_system("systems", move |_, _| work.advance(interval / 2));

    let hook = clock.clone();
    let mut stop = stop_after(10);
    tick_loop
        .run_with_clock(&mut clock, |l| {
            // Applying input before the tick takes 60% of the interval.
            hook.advance(interval * 3 / 5);
            stop(l)
        })
        .expect("no panic");

    let stats = tick_loop.metrics().stats();
    assert_eq!(stats.p99, interval * 11 / 10, "{stats}");
    assert_eq!(stats.overruns, 10);
    assert!(!stats.within_budget());
}

#[test]
fn real_clock_sleeps_between_ticks() {
    let mut tick_loop = moving_world();
    let interval = tick_loop.config().interval();
    let ticks = 50;
    let start = Instant::now();
    tick_loop.run(stop_after(ticks)).expect("no panic");
    let elapsed = start.elapsed();

    let expected = interval * 50;
    assert!(
        elapsed >= expected,
        "ran fast: {ticks} ticks in {elapsed:?}, expected {expected:?}"
    );
    // Generous on purpose: the simulated-clock tests check the schedule exactly; this one only rules out a
    // loop that never returns.
    assert!(
        elapsed < expected + Duration::from_secs(30),
        "ran far too slow: {ticks} ticks in {elapsed:?}, expected {expected:?}"
    );
    assert_eq!(tick_loop.metrics().ticks(), ticks);
    assert_moved_one_second(&mut tick_loop);
}

#[test]
fn an_overrun_is_kept_with_the_phase_that_took_the_time() {
    let mut tick_loop = TickLoop::new(TickConfig::new(50).expect("valid rate"));
    let interval = tick_loop.config().interval();
    let mut clock = SimClock::new();
    let (fast, slow) = (clock.clone(), clock.clone());
    tick_loop.add_system("input", move |_, _| fast.advance(interval / 10));
    // Tick 3 stalls in "snapshots" for three intervals, tick 6 for two.
    tick_loop.add_system("snapshots", move |_, info| match info.tick {
        3 => slow.advance(interval * 3),
        6 => slow.advance(interval * 2),
        _ => {}
    });
    let hook = clock.clone();
    let mut stop = stop_after(10);
    tick_loop
        .run_with_clock(&mut clock, |l| {
            hook.advance(interval / 20);
            stop(l)
        })
        .expect("no panic");

    let overrun = tick_loop.take_worst_overrun().expect("an overrun");
    assert_eq!(overrun.tick, 3);
    assert_eq!(overrun.duration, interval * 3 + interval / 10 + interval / 20);
    assert_eq!(
        overrun.phases,
        vec![
            (BETWEEN, interval / 20),
            ("input", interval / 10),
            ("snapshots", interval * 3)
        ]
    );
    assert_eq!(overrun.slowest(), Some(("snapshots", interval * 3)));
    assert!(
        overrun
            .to_string()
            .starts_with("tick 3 took 63.000 ms: between 1.000, input 2.000, snapshots 60.000"),
        "{overrun}"
    );
    // Taken: the next report starts empty.
    assert_eq!(tick_loop.take_worst_overrun(), None);
}

#[test]
fn a_panicking_system_stops_the_loop_before_anything_else_runs() {
    let mut tick_loop = TickLoop::new(TickConfig::default());
    let mut clock = SimClock::new();
    let after = Arc::new(Mutex::new(Vec::new()));
    let log = after.clone();
    tick_loop.add_system("physics", |_, info| {
        assert!(info.tick != 2, "contact solver diverged");
    });
    tick_loop.add_system("snapshots", move |_, info| {
        log.lock().expect("log").push(info.tick)
    });
    let mut hooks = 0;
    let result = tick_loop.run_with_clock(&mut clock, |_| {
        hooks += 1;
        ControlFlow::Continue(())
    });

    let panic = result.expect_err("stopped by the panic");
    assert_eq!((panic.system, panic.tick), ("physics", 2));
    assert!(panic.message.contains("contact solver diverged"), "{panic}");
    // The systems after it did not run in that tick, and no tick ran after it.
    assert_eq!(*after.lock().expect("log"), vec![0, 1]);
    assert_eq!(hooks, 3);
    assert_eq!(tick_loop.metrics().ticks(), 2);
}
