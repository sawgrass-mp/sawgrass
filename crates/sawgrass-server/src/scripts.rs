//! Resources in the tick (task SCRIPT-4): the `scripts` system runs after `net` and gives every resource the
//! players who joined and left this tick, then the tick, within a script budget per tick (see
//! [`sawgrass_script::Host::run_tick`]). Every `report_every` ticks it prints each resource's counters, last error
//! and log.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use sawgrass_script::Host;

use crate::TickInfo;
use crate::net::Roster;

/// The default script budget per tick at 20 Hz: a quarter of the 35 ms p99 the tick must keep
/// (`docs/implementation-plan.md`), leaving the rest to the world. [`budget_for`] gives it for other rates.
pub const DEFAULT_BUDGET: Duration = Duration::from_millis(8);

/// The default script budget at `rate_hz`: a quarter of the p99 the tick must keep (70% of its interval), in
/// whole milliseconds and at least one. 8 ms at 20 Hz, 5 ms at 30 Hz.
#[must_use]
pub fn budget_for(rate_hz: u32) -> Duration {
    let interval_us = 1_000_000 / u64::from(rate_hz.max(1));
    let quarter_ms = interval_us * 7 / 10 / 4 / 1000;
    Duration::from_millis(quarter_ms.max(1))
}

/// The `scripts` tick system.
#[derive(Debug)]
pub struct Scripts {
    host: Host,
    roster: Option<Receiver<Roster>>,
    budget: Duration,
    report_every: u64,
    joined: Vec<u32>,
    left: Vec<u32>,
    worst: Duration,
    skipped: usize,
}

impl Scripts {
    /// Runs `host`'s resources with `budget` per tick, taking joins and leaves from `roster` (none without a
    /// gateway), and reports every `report_every` ticks (0: never).
    #[must_use]
    pub fn new(host: Host, roster: Option<Receiver<Roster>>, budget: Duration, report_every: u64) -> Self {
        Self {
            host,
            roster,
            budget,
            report_every,
            joined: Vec::new(),
            left: Vec::new(),
            worst: Duration::ZERO,
            skipped: 0,
        }
    }

    /// The resources, for tests and reports.
    #[must_use]
    pub fn host(&self) -> &Host {
        &self.host
    }

    /// The tick system.
    pub fn run_tick(&mut self, info: &TickInfo) {
        self.joined.clear();
        self.left.clear();
        if let Some(roster) = &self.roster {
            while let Ok(change) = roster.try_recv() {
                match change {
                    Roster::Joined(id) => self.joined.push(id),
                    Roster::Left(id) => self.left.push(id),
                    // Resources get deaths through the scripting API with a later task.
                    Roster::Died(_) => {}
                }
            }
        }
        let dt = info.dt.as_secs_f64();
        let report = self
            .host
            .run_tick(info.tick, dt, &self.joined, &self.left, self.budget);
        self.worst = self.worst.max(report.spent);
        self.skipped += report.skipped;
        if self.report_every > 0 && info.tick > 0 && info.tick.is_multiple_of(self.report_every) {
            for line in self.report() {
                println!("{line}");
            }
        }
    }

    /// Lines describing the resources since the last report: the slowest tick, then one line per resource and
    /// what each printed.
    pub fn report(&mut self) -> Vec<String> {
        let mut lines = vec![format!(
            "scripts: slowest tick {:.2?} of {:.2?} budget, {} tick events skipped, {} player events dropped",
            self.worst,
            self.budget,
            self.skipped,
            self.host.dropped()
        )];
        self.worst = Duration::ZERO;
        self.skipped = 0;
        for r in self.host.resources_mut() {
            let s = r.stats();
            let mut line = format!(
                "  {}: {} calls, {:.2?} busy, slowest {:.2?}, {} KiB, errors: {} deadline {} memory {} script, \
                 skipped {}",
                r.name(),
                s.calls,
                s.busy,
                s.slowest,
                r.memory_used() / 1024,
                s.deadlines,
                s.memory_errors,
                s.script_errors,
                s.skipped_ticks
            );
            if let Some(e) = r.last_error() {
                line += &format!(", last: {e}");
            }
            lines.push(line);
            let name = r.name().to_owned();
            lines.extend(r.drain_log().into_iter().map(|l| format!("  [{name}] {l}")));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_budget_follows_the_tick_rate() {
        assert_eq!(budget_for(20), DEFAULT_BUDGET);
        assert_eq!(budget_for(30), Duration::from_millis(5));
        assert_eq!(budget_for(1000), Duration::from_millis(1));
        assert_eq!(budget_for(0), budget_for(1));
    }
}
