//! Which server tick it is on the client, and the render tick remote entities are shown at.
//!
//! The server stamps every snapshot with its tick and runs at a fixed rate, so `arrival - tick / rate` is the
//! one-way delay plus the offset between the two clocks, plus jitter. Jitter only ever delays a datagram, so the
//! clock follows the lower edge of those samples: an early arrival pulls the estimate down at once, and when every
//! sample of the last second was late the estimate rises by the smallest lateness, which follows a server that
//! runs slow or drops tick slots under load within a second. A jump of more than [`ClockConfig::resync_secs`] (a
//! server restart, a stalled client) resets it.
//!
//! The render tick trails the estimate by a delay long enough that the snapshot after the render tick has
//! normally arrived: one tick for the snapshot interval, [`ClockConfig::extra_ticks`] for a lost or late snapshot,
//! and a multiple of the measured jitter. The render tick runs with local time and only speeds up or slows down
//! by a few percent to follow that target, so remote entities never visibly jump and never go backwards.

/// How the clock and the render delay adapt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockConfig {
    /// Ticks of delay on top of one snapshot interval, so one lost snapshot does not leave the interpolation
    /// without a next sample.
    pub extra_ticks: f64,
    /// Measured jitter is multiplied by this and added to the delay.
    pub jitter_factor: f64,
    /// Longest render delay, in ticks, whatever the jitter.
    pub max_delay_ticks: f64,
    /// How much faster or slower than real time the render tick may run to follow its target: 0.05 is 5%.
    pub delay_rate: f64,
    /// A sample this many seconds away from the estimate resets the clock.
    pub resync_secs: f64,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            extra_ticks: 1.0,
            jitter_factor: 2.0,
            max_delay_ticks: 8.0,
            delay_rate: 0.05,
            resync_secs: 1.0,
        }
    }
}

/// Weight of a sample earlier than the estimate: the estimate falls fast.
const FALL: f64 = 0.5;
/// Weight of a sample in the running jitter.
const JITTER_WEIGHT: f64 = 0.1;

/// The server tick estimated from snapshot arrivals.
#[derive(Debug, Clone)]
pub struct ServerClock {
    config: ClockConfig,
    tick_hz: f64,
    /// Local time of tick 0 on the fastest path, in seconds.
    offset: Option<f64>,
    /// Running mean of how far samples land from `offset`, in seconds.
    jitter: f64,
    /// The smallest lateness in the current window of samples, and how many samples it holds.
    window_min: f64,
    window_len: u32,
    /// How far the last render tick trailed the server tick, in ticks.
    delay: f64,
    /// The local time and value of the last render tick returned.
    last: Option<(f64, f64)>,
}

impl ServerClock {
    /// A clock for a server running at `tick_hz` (from the join event), with nothing observed yet.
    #[must_use]
    pub fn new(tick_hz: u16, config: ClockConfig) -> Self {
        let tick_hz = f64::from(tick_hz.max(1));
        Self {
            config,
            tick_hz,
            offset: None,
            jitter: 0.0,
            window_min: f64::INFINITY,
            window_len: 0,
            delay: 1.0 + config.extra_ticks,
            last: None,
        }
    }

    /// Server ticks per second.
    #[must_use]
    pub fn tick_hz(&self) -> f64 {
        self.tick_hz
    }

    /// Takes the arrival of the snapshot for `tick` at local time `now`, in seconds.
    pub fn observe(&mut self, tick: u32, now: f64) {
        let sample = now - f64::from(tick) / self.tick_hz;
        let Some(offset) = self.offset else {
            self.offset = Some(sample);
            return;
        };
        let d = sample - offset;
        if d.abs() > self.config.resync_secs {
            self.offset = Some(sample);
            self.jitter = 0.0;
            self.window_min = f64::INFINITY;
            self.window_len = 0;
            self.last = None;
            return;
        }
        self.jitter += (d.abs() - self.jitter) * JITTER_WEIGHT;
        if d < 0.0 {
            self.offset = Some(offset + d * FALL);
            self.window_min = f64::INFINITY;
            self.window_len = 0;
            return;
        }
        self.window_min = self.window_min.min(d);
        self.window_len += 1;
        if f64::from(self.window_len) >= self.tick_hz {
            // A whole second of late samples: the fastest path itself is later now.
            self.offset = Some(offset + self.window_min);
            self.window_min = f64::INFINITY;
            self.window_len = 0;
        }
    }

    /// Whether a snapshot has been observed.
    #[must_use]
    pub fn synced(&self) -> bool {
        self.offset.is_some()
    }

    /// The server tick at local time `now`, fractional: the newest tick a snapshot on the fastest path would
    /// carry. `None` before the first snapshot.
    #[must_use]
    pub fn server_tick(&self, now: f64) -> Option<f64> {
        self.offset.map(|o| (now - o) * self.tick_hz)
    }

    /// Measured jitter in seconds.
    #[must_use]
    pub fn jitter_secs(&self) -> f64 {
        self.jitter
    }

    /// How far the last render tick trailed the server tick, in ticks.
    #[must_use]
    pub fn delay_ticks(&self) -> f64 {
        self.delay
    }

    /// The tick remote entities are shown at, at local time `now`: the server tick minus the render delay. `None`
    /// before the first snapshot. Call it once per frame.
    ///
    /// The render tick does not follow every move of the estimate: it advances with local time at a rate within
    /// [`ClockConfig::delay_rate`] of real time towards its target, so a jittery estimate or a growing delay never
    /// makes remote entities jump or run backwards. Only a gap larger than [`ClockConfig::max_delay_ticks`] (a
    /// resync) is crossed at once.
    pub fn render_tick(&mut self, now: f64) -> Option<f64> {
        let server = self.server_tick(now)?;
        let target_delay =
            (1.0 + self.config.extra_ticks + self.config.jitter_factor * self.jitter * self.tick_hz)
                .min(self.config.max_delay_ticks);
        let target = server - target_delay;
        let render = match self.last {
            Some((at, last)) => {
                let elapsed = ((now - at) * self.tick_hz).max(0.0);
                let predicted = last + elapsed;
                let error = target - predicted;
                if error.abs() > self.config.max_delay_ticks {
                    target
                } else {
                    let step = self.config.delay_rate * elapsed;
                    predicted + error.clamp(-step, step)
                }
            }
            None => target,
        };
        self.last = Some((now, render));
        self.delay = server - render;
        Some(render)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_fastest_arrivals() {
        let mut clock = ServerClock::new(20, ClockConfig::default());
        assert_eq!(clock.render_tick(0.0), None);
        // 40 ms one way; every third snapshot 30 ms late.
        for tick in 0..200u32 {
            let late = if tick % 3 == 0 { 0.030 } else { 0.0 };
            clock.observe(tick, f64::from(tick) / 20.0 + 0.040 + late);
        }
        let now = 200.0 / 20.0 + 0.040;
        let server = clock.server_tick(now).expect("synced");
        assert!((server - 200.0).abs() < 0.15, "server tick {server}");
        assert!(clock.jitter_secs() > 0.003 && clock.jitter_secs() < 0.03);
    }

    #[test]
    fn render_tick_trails_and_never_goes_back() {
        let mut clock = ServerClock::new(20, ClockConfig::default());
        let mut last = f64::NEG_INFINITY;
        for tick in 0..400u32 {
            let now = f64::from(tick) / 20.0 + 0.05;
            clock.observe(tick, now);
            let render = clock.render_tick(now).expect("synced");
            assert!(render >= last);
            last = render;
        }
        // No jitter: one interval plus one extra tick.
        assert!((clock.delay_ticks() - 2.0).abs() < 1e-9);
        assert!((last - 397.0).abs() < 0.05, "render {last}");
    }

    #[test]
    fn delay_grows_with_jitter_while_the_render_tick_runs_smoothly() {
        let mut clock = ServerClock::new(20, ClockConfig::default());
        let mut last: Option<(f64, f64)> = None;
        for frame in 0..6000u32 {
            let now = f64::from(frame) / 60.0;
            // Snapshots with 0 to 60 ms of jitter in a fixed pattern, delivered by the frame they arrive in.
            for tick in 0..=(now * 20.0) as u32 {
                let arrival = f64::from(tick) / 20.0 + f64::from((tick * 7) % 13) * 0.005;
                if arrival <= now && arrival > now - 1.0 / 60.0 {
                    clock.observe(tick, arrival);
                }
            }
            let Some(render) = clock.render_tick(now) else {
                continue;
            };
            if let Some((at, previous)) = last {
                // Never faster or slower than real time by more than 5%.
                let rate = (render - previous) / ((now - at) * 20.0);
                assert!((0.95 - 1e-9..=1.05 + 1e-9).contains(&rate), "rate {rate}");
            }
            last = Some((now, render));
        }
        let delay = clock.delay_ticks();
        assert!(delay > 2.5 && delay < 8.0, "delay {delay}");
    }

    #[test]
    fn follows_a_server_that_runs_slow() {
        let mut clock = ServerClock::new(20, ClockConfig::default());
        // Ticks 52 ms apart instead of 50, then one dropped slot of 100 ms.
        let mut at = 0.0;
        for tick in 0..400u32 {
            at += if tick == 300 { 0.152 } else { 0.052 };
            clock.observe(tick, at + 0.030);
        }
        let server = clock.server_tick(at + 0.030).expect("synced");
        // Within a second's worth of drift (0.8 ticks) and the dropped slot.
        assert!((399.0 - server).abs() < 3.0, "server tick {server}");
        // The drift itself reads as up to 40 ms of lateness a second, and the delay grows to cover it.
        assert!(clock.jitter_secs() < 0.08, "jitter {}", clock.jitter_secs());
    }

    #[test]
    fn resyncs_after_a_jump() {
        let mut clock = ServerClock::new(30, ClockConfig::default());
        for tick in 0..30u32 {
            clock.observe(tick, f64::from(tick) / 30.0);
        }
        // The server restarted: ticks begin again from 0, five seconds later.
        clock.observe(0, 6.0);
        let server = clock.server_tick(6.0).expect("synced");
        assert!(server.abs() < 1e-9, "{server}");
        assert!(clock.render_tick(6.0).expect("synced") < 0.0);
    }
}
