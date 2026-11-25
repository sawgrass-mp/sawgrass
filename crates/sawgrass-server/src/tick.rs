//! The fixed-rate tick loop.
//!
//! [`TickLoop`] owns the ECS world and an ordered list of systems. Each tick runs every system once, in the
//! order they were added, and records how long that took in [`TickMetrics`]. [`TickLoop::run`] schedules
//! ticks on a fixed grid (tick `n` starts at `start + n * interval`), so the rate does not drift with tick
//! duration. When a tick overruns and the loop falls more than one interval behind, the grid restarts from
//! the current time instead of running the missed ticks back to back.
//!
//! A tick's recorded duration runs from the start of its slot, when [`TickLoop::run`] calls `between`, to the
//! end of its last system: everything that delays the schedule counts against the budget. The loop reads
//! time through a [`Clock`], so tests can drive the schedule with a simulated clock instead of sleeping.
//!
//! Every phase of a tick (the `between` hook, then each system by name) is timed, and the longest overrun since
//! [`TickLoop::take_worst_overrun`] is kept with its phases, so a report can say where an overrun's time went. A
//! system that panics stops the loop: [`TickLoop::run`] returns a [`SystemPanic`] naming it, and no later system
//! runs on the world it may have left half-written.

use std::ops::ControlFlow;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use hecs::World;

use crate::metrics::TickMetrics;

/// Tick rate and metrics window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickConfig {
    rate_hz: u32,
    window_secs: u32,
}

impl TickConfig {
    /// The default tick rate, from the 20–30 Hz range in `docs/architecture.md`.
    pub const DEFAULT_RATE_HZ: u32 = 20;
    /// The highest accepted tick rate.
    pub const MAX_RATE_HZ: u32 = 1000;
    /// Seconds of ticks the metrics window covers by default.
    pub const DEFAULT_WINDOW_SECS: u32 = 60;
    /// The longest accepted metrics window: one hour, at most 3.6 million ticks at [`Self::MAX_RATE_HZ`].
    pub const MAX_WINDOW_SECS: u32 = 3600;

    /// A configuration ticking at `rate_hz` (1 to [`Self::MAX_RATE_HZ`]).
    pub fn new(rate_hz: u32) -> Result<Self, TickConfigError> {
        if rate_hz == 0 || rate_hz > Self::MAX_RATE_HZ {
            return Err(TickConfigError { rate_hz });
        }
        Ok(Self {
            rate_hz,
            window_secs: Self::DEFAULT_WINDOW_SECS,
        })
    }

    /// Sets how many seconds of ticks the metrics window covers, clamped to 1 to [`Self::MAX_WINDOW_SECS`].
    #[must_use]
    pub fn with_window_secs(mut self, secs: u32) -> Self {
        self.window_secs = secs.clamp(1, Self::MAX_WINDOW_SECS);
        self
    }

    /// Ticks per second.
    #[must_use]
    pub fn rate_hz(&self) -> u32 {
        self.rate_hz
    }

    /// Time between the starts of two ticks.
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(1) / self.rate_hz
    }

    /// Ticks kept in the metrics window.
    #[must_use]
    pub fn window_ticks(&self) -> usize {
        usize::try_from(u64::from(self.rate_hz) * u64::from(self.window_secs)).unwrap_or(usize::MAX)
    }
}

impl Default for TickConfig {
    fn default() -> Self {
        Self {
            rate_hz: Self::DEFAULT_RATE_HZ,
            window_secs: Self::DEFAULT_WINDOW_SECS,
        }
    }
}

/// A tick rate outside 1 to [`TickConfig::MAX_RATE_HZ`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickConfigError {
    /// The rejected rate.
    pub rate_hz: u32,
}

impl std::fmt::Display for TickConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tick rate {} Hz is outside 1..={} Hz",
            self.rate_hz,
            TickConfig::MAX_RATE_HZ
        )
    }
}

impl std::error::Error for TickConfigError {}

/// What a system knows about the tick it runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickInfo {
    /// Number of this tick, from 0.
    pub tick: u64,
    /// Simulated time this tick advances: always the tick interval, so simulation is deterministic.
    pub dt: Duration,
}

/// The time source of [`TickLoop::run_with_clock`].
pub trait Clock {
    /// The current time.
    fn now(&self) -> Instant;
    /// Blocks until `deadline`; returns at once if it has passed.
    fn sleep_until(&mut self, deadline: Instant);
}

/// The real clock: [`Instant::now`] and [`std::thread::sleep`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&mut self, deadline: Instant) {
        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);
        }
    }
}

type System = Box<dyn FnMut(&mut World, &TickInfo) + Send>;

/// The name [`Overrun::phases`] gives the `between` hook of [`TickLoop::run`].
pub const BETWEEN: &str = "between";

/// A tick that took longer than the interval, and where its time went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overrun {
    /// The tick's number.
    pub tick: u64,
    /// Its recorded duration.
    pub duration: Duration,
    /// Each phase in the order it ran: [`BETWEEN`] (under [`TickLoop::run`]), then the systems by name.
    pub phases: Vec<(&'static str, Duration)>,
}

impl Overrun {
    /// The phase that took the longest.
    #[must_use]
    pub fn slowest(&self) -> Option<(&'static str, Duration)> {
        self.phases.iter().copied().max_by_key(|&(_, d)| d)
    }
}

impl std::fmt::Display for Overrun {
    /// `tick 812 took 61.204 ms: between 0.051, net 61.153`
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tick {} took {:.3} ms:", self.tick, ms(self.duration))?;
        for (i, (name, d)) in self.phases.iter().enumerate() {
            let sep = if i == 0 { "" } else { "," };
            write!(f, "{sep} {name} {:.3}", ms(*d))?;
        }
        Ok(())
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// A system panicked; the loop stopped before running anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPanic {
    /// The system's name.
    pub system: &'static str,
    /// The tick it panicked in.
    pub tick: u64,
    /// The panic message, when it was a string.
    pub message: String,
}

impl std::fmt::Display for SystemPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "system {:?} panicked in tick {}: {}",
            self.system, self.tick, self.message
        )
    }
}

impl std::error::Error for SystemPanic {}

/// The world, its systems and the fixed-rate loop that runs them.
pub struct TickLoop {
    config: TickConfig,
    world: World,
    systems: Vec<(&'static str, System)>,
    metrics: TickMetrics,
    tick: u64,
    /// The phases of the tick being run, reused across ticks.
    phases: Vec<(&'static str, Duration)>,
    worst: Option<Overrun>,
}

impl std::fmt::Debug for TickLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TickLoop")
            .field("config", &self.config)
            .field("entities", &self.world.len())
            .field(
                "systems",
                &self.systems.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            )
            .field("tick", &self.tick)
            .finish_non_exhaustive()
    }
}

impl TickLoop {
    /// An empty world with no systems.
    #[must_use]
    pub fn new(config: TickConfig) -> Self {
        Self {
            metrics: TickMetrics::new(config.interval(), config.window_ticks()),
            config,
            world: World::new(),
            systems: Vec::new(),
            tick: 0,
            phases: Vec::new(),
            worst: None,
        }
    }

    /// Adds a system that runs once per tick, after the systems added before it.
    pub fn add_system(
        &mut self,
        name: &'static str,
        system: impl FnMut(&mut World, &TickInfo) + Send + 'static,
    ) -> &mut Self {
        self.systems.push((name, Box::new(system)));
        self
    }

    /// The configuration the loop was built with.
    #[must_use]
    pub fn config(&self) -> &TickConfig {
        &self.config
    }

    /// The ECS world.
    #[must_use]
    pub fn world(&self) -> &World {
        &self.world
    }

    /// The ECS world, for spawning and despawning between ticks.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Tick duration metrics.
    #[must_use]
    pub fn metrics(&self) -> &TickMetrics {
        &self.metrics
    }

    /// Ticks run so far; also the number of the next tick.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The longest overrun since the last call, with its phases; `None` if no tick overran.
    pub fn take_worst_overrun(&mut self) -> Option<Overrun> {
        self.worst.take()
    }

    /// Runs every system once, records the duration and returns it. Does not wait.
    ///
    /// # Panics
    ///
    /// If a system panics, with its [`SystemPanic`] as the message; the systems after it do not run.
    pub fn run_tick(&mut self) -> Duration {
        let clock = SystemClock;
        let start = clock.now();
        self.phases.clear();
        match self.finish_tick(&clock, start, start) {
            Ok(duration) => duration,
            Err(panic) => panic!("{panic}"),
        }
    }

    /// Runs every system once, timing each from `systems_start`, and records the time from `slot_start` to the
    /// end of the last system. Phases already in `self.phases` (the `between` hook) count as this tick's.
    fn finish_tick(
        &mut self,
        clock: &impl Clock,
        slot_start: Instant,
        systems_start: Instant,
    ) -> Result<Duration, SystemPanic> {
        let info = TickInfo {
            tick: self.tick,
            dt: self.config.interval(),
        };
        let mut phase_start = systems_start;
        for (name, system) in &mut self.systems {
            let world = &mut self.world;
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| system(world, &info))) {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_owned());
                return Err(SystemPanic {
                    system: name,
                    tick: self.tick,
                    message,
                });
            }
            let now = clock.now();
            self.phases
                .push((name, now.saturating_duration_since(phase_start)));
            phase_start = now;
        }
        let duration = phase_start.saturating_duration_since(slot_start);
        self.metrics.record(duration);
        if duration > self.config.interval() && self.worst.as_ref().is_none_or(|w| duration > w.duration) {
            self.worst = Some(Overrun {
                tick: self.tick,
                duration,
                phases: self.phases.clone(),
            });
        }
        self.tick += 1;
        Ok(duration)
    }

    /// Runs ticks at the configured rate on the real clock until `between` returns [`ControlFlow::Break`], or
    /// until a system panics, which returns its [`SystemPanic`].
    ///
    /// `between` is called at the start of every tick slot, before the systems. It is the place for work
    /// that is not simulation: reporting metrics, applying commands, deciding to stop. Its time counts in the
    /// recorded tick duration, because it delays the schedule like any system.
    pub fn run(&mut self, between: impl FnMut(&mut Self) -> ControlFlow<()>) -> Result<(), SystemPanic> {
        self.run_with_clock(&mut SystemClock, between)
    }

    /// [`Self::run`] with time read from `clock`.
    pub fn run_with_clock(
        &mut self,
        clock: &mut impl Clock,
        mut between: impl FnMut(&mut Self) -> ControlFlow<()>,
    ) -> Result<(), SystemPanic> {
        let interval = self.config.interval();
        let mut next = clock.now();
        loop {
            let slot_start = clock.now();
            if between(self).is_break() {
                return Ok(());
            }
            let systems_start = clock.now();
            self.phases.clear();
            self.phases
                .push((BETWEEN, systems_start.saturating_duration_since(slot_start)));
            self.finish_tick(clock, slot_start, systems_start)?;
            next += interval;
            let now = clock.now();
            if now < next {
                clock.sleep_until(next);
            } else if now - next > interval {
                // More than a whole tick behind: drop the missed slots instead of bursting to catch up.
                next = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rate_is_twenty_hz() {
        let config = TickConfig::default();
        assert_eq!(config.rate_hz(), 20);
        assert_eq!(config.interval(), Duration::from_millis(50));
        assert_eq!(config.window_ticks(), 1200);
    }

    #[test]
    fn window_is_clamped_to_one_hour() {
        let config = TickConfig::new(TickConfig::MAX_RATE_HZ).expect("valid rate");
        assert_eq!(config.with_window_secs(u32::MAX).window_ticks(), 3_600_000);
        assert_eq!(config.with_window_secs(0).window_ticks(), 1000);
        // Building the loop with the largest window must not try to allocate it all up front.
        let tick_loop = TickLoop::new(config.with_window_secs(u32::MAX));
        assert_eq!(tick_loop.metrics().stats().window, 0);
    }

    #[test]
    fn rate_must_be_in_range() {
        assert_eq!(TickConfig::new(0), Err(TickConfigError { rate_hz: 0 }));
        assert_eq!(TickConfig::new(1001), Err(TickConfigError { rate_hz: 1001 }));
        assert_eq!(
            TickConfig::new(30).map(|c| c.interval()),
            Ok(Duration::from_secs(1) / 30)
        );
    }

    #[test]
    fn systems_run_in_order_with_tick_info() {
        let mut tick_loop = TickLoop::new(TickConfig::default());
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        for name in ["first", "second"] {
            let log = log.clone();
            tick_loop.add_system(name, move |_, info| {
                if let Ok(mut log) = log.lock() {
                    log.push((name, info.tick, info.dt));
                }
            });
        }
        tick_loop.run_tick();
        tick_loop.run_tick();
        let dt = Duration::from_millis(50);
        let log = log.lock().map(|l| l.clone()).unwrap_or_default();
        assert_eq!(
            log,
            vec![
                ("first", 0, dt),
                ("second", 0, dt),
                ("first", 1, dt),
                ("second", 1, dt)
            ]
        );
        assert_eq!(tick_loop.metrics().ticks(), 2);
    }
}
