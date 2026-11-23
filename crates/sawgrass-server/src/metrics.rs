//! Tick duration metrics.
//!
//! [`TickMetrics`] keeps the duration of the most recent ticks in a fixed-size window (one minute of ticks
//! by default) and counts every tick and every overrun since the server started. [`TickMetrics::stats`]
//! turns the window into a [`TickStats`] snapshot: p50, p99 and maximum tick time, and p99 as a fraction of
//! the tick interval. The architecture budget is met while that fraction stays under
//! [`TickStats::P99_BUDGET_FRACTION`].
//!
//! A tick's duration is the time its systems take to run, not the time between ticks: the sleep until the
//! next tick is excluded. An overrun is a tick that took longer than the tick interval.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

/// Rolling record of tick durations.
#[derive(Debug, Clone)]
pub struct TickMetrics {
    interval: Duration,
    window: VecDeque<Duration>,
    capacity: usize,
    ticks: u64,
    overruns: u64,
}

impl TickMetrics {
    /// Metrics for ticks of `interval`, keeping the last `capacity` durations (at least one).
    #[must_use]
    pub fn new(interval: Duration, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            interval,
            window: VecDeque::with_capacity(capacity),
            capacity,
            ticks: 0,
            overruns: 0,
        }
    }

    /// Records one tick that took `duration`.
    pub fn record(&mut self, duration: Duration) {
        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(duration);
        self.ticks += 1;
        if duration > self.interval {
            self.overruns += 1;
        }
    }

    /// Ticks recorded since creation.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Ticks since creation that took longer than the tick interval.
    #[must_use]
    pub fn overruns(&self) -> u64 {
        self.overruns
    }

    /// A snapshot of the current window. Sorts a copy of the window, so call it for reports, not every tick.
    #[must_use]
    pub fn stats(&self) -> TickStats {
        let mut sorted: Vec<Duration> = self.window.iter().copied().collect();
        sorted.sort_unstable();
        TickStats {
            ticks: self.ticks,
            overruns: self.overruns,
            window: sorted.len(),
            interval: self.interval,
            p50: percentile(&sorted, 50),
            p99: percentile(&sorted, 99),
            max: sorted.last().copied().unwrap_or_default(),
        }
    }
}

/// Nearest-rank percentile of an ascending slice; zero for an empty slice.
fn percentile(sorted: &[Duration], pct: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (pct * sorted.len()).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

/// Tick duration statistics over the metrics window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickStats {
    /// Ticks recorded since the server started.
    pub ticks: u64,
    /// Ticks since the server started that took longer than the tick interval.
    pub overruns: u64,
    /// How many recent ticks the percentiles cover.
    pub window: usize,
    /// The tick interval (1 / tick rate).
    pub interval: Duration,
    /// Median tick duration in the window.
    pub p50: Duration,
    /// 99th percentile tick duration in the window.
    pub p99: Duration,
    /// Longest tick in the window.
    pub max: Duration,
}

impl TickStats {
    /// The architecture budget: p99 tick time under 70% of the tick interval.
    pub const P99_BUDGET_FRACTION: f64 = 0.7;

    /// p99 as a fraction of the tick interval.
    #[must_use]
    pub fn p99_fraction(&self) -> f64 {
        self.p99.as_secs_f64() / self.interval.as_secs_f64()
    }

    /// Whether p99 is within [`Self::P99_BUDGET_FRACTION`] of the tick interval.
    #[must_use]
    pub fn within_budget(&self) -> bool {
        self.p99_fraction() < Self::P99_BUDGET_FRACTION
    }
}

impl fmt::Display for TickStats {
    /// One log line, for example:
    /// `ticks=1200 window=1200 p50=0.412ms p99=1.873ms max=2.950ms interval=50.000ms p99_budget=3.7% overruns=0`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        write!(
            f,
            "ticks={} window={} p50={:.3}ms p99={:.3}ms max={:.3}ms interval={:.3}ms p99_budget={:.1}% overruns={}",
            self.ticks,
            self.window,
            ms(self.p50),
            ms(self.p99),
            ms(self.max),
            ms(self.interval),
            self.p99_fraction() * 100.0,
            self.overruns,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn empty_window_reports_zero() {
        let stats = TickMetrics::new(50 * MS, 10).stats();
        assert_eq!(
            (stats.ticks, stats.window, stats.p50, stats.p99, stats.max),
            (0, 0, Duration::ZERO, Duration::ZERO, Duration::ZERO)
        );
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut metrics = TickMetrics::new(500 * MS, 100);
        for i in (1..=100).rev() {
            metrics.record(i * MS);
        }
        let stats = metrics.stats();
        assert_eq!(stats.p50, 50 * MS);
        assert_eq!(stats.p99, 99 * MS);
        assert_eq!(stats.max, 100 * MS);
    }

    #[test]
    fn window_keeps_only_recent_ticks() {
        let mut metrics = TickMetrics::new(50 * MS, 3);
        for d in [40, 1, 2, 3] {
            metrics.record(d * MS);
        }
        let stats = metrics.stats();
        assert_eq!((stats.ticks, stats.window, stats.max), (4, 3, 3 * MS));
    }

    #[test]
    fn overruns_count_ticks_longer_than_the_interval() {
        let mut metrics = TickMetrics::new(50 * MS, 10);
        for d in [10, 50, 51, 80] {
            metrics.record(d * MS);
        }
        assert_eq!(metrics.overruns(), 2);
    }

    #[test]
    fn budget_is_seventy_percent_of_the_interval() {
        let mut metrics = TickMetrics::new(50 * MS, 1);
        metrics.record(34 * MS);
        assert!(metrics.stats().within_budget());
        metrics.record(35 * MS);
        assert!(!metrics.stats().within_budget());
    }
}
