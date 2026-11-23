//! The fixed-rate tick loop.
//!
//! [`TickLoop`] owns the ECS world and an ordered list of systems. Each tick runs every system once, in the
//! order they were added, and records how long that took in [`TickMetrics`]. [`TickLoop::run`] schedules
//! ticks on a fixed grid (tick `n` starts at `start + n * interval`), so the rate does not drift with tick
//! duration. When a tick overruns and the loop falls more than one interval behind, the grid restarts from
//! the current time instead of running the missed ticks back to back.

use std::ops::ControlFlow;
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

    /// Sets how many seconds of ticks the metrics window covers (at least one tick).
    #[must_use]
    pub fn with_window_secs(mut self, secs: u32) -> Self {
        self.window_secs = secs;
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

type System = Box<dyn FnMut(&mut World, &TickInfo) + Send>;

/// The world, its systems and the fixed-rate loop that runs them.
pub struct TickLoop {
    config: TickConfig,
    world: World,
    systems: Vec<(&'static str, System)>,
    metrics: TickMetrics,
    tick: u64,
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

    /// Runs every system once, records the duration and returns it. Does not wait.
    pub fn run_tick(&mut self) -> Duration {
        let info = TickInfo {
            tick: self.tick,
            dt: self.config.interval(),
        };
        let start = Instant::now();
        for (_, system) in &mut self.systems {
            system(&mut self.world, &info);
        }
        let duration = start.elapsed();
        self.metrics.record(duration);
        self.tick += 1;
        duration
    }

    /// Runs ticks at the configured rate until `between` returns [`ControlFlow::Break`].
    ///
    /// `between` is called before every tick, outside the measured tick time. It is the place for work that
    /// is not simulation: reporting metrics, applying commands, deciding to stop.
    pub fn run(&mut self, mut between: impl FnMut(&mut Self) -> ControlFlow<()>) {
        let interval = self.config.interval();
        let mut next = Instant::now();
        loop {
            if between(self).is_break() {
                return;
            }
            self.run_tick();
            next += interval;
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
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
