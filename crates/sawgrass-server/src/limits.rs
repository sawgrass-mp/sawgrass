//! Per-connection limits (task NET-12).
//!
//! Each connection gets a [`Bucket`] for client states and one for events. A bucket admits what arrives within its
//! rate and bursts, and drops the rest. A connection that keeps sending more than twice its rate, for
//! [`Limits::strikes`] windows in a row, is told to close: dropping costs little, but a flood still costs the
//! gateway its receive work, and no honest client sends one.

use std::time::{Duration, Instant};

/// The limits every connection is held to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Client states admitted per second; a client sends one per tick (20 at 20 Hz), so this leaves room for 60 Hz.
    pub states_per_sec: f32,
    /// Client states admitted at once after a quiet spell.
    pub state_burst: f32,
    /// Events admitted per second.
    pub events_per_sec: f32,
    /// Events admitted at once after a quiet spell.
    pub event_burst: f32,
    /// The window over which a flood is judged.
    pub window: Duration,
    /// Windows in a row with more than twice the rate after which the connection is closed.
    pub strikes: u32,
    /// Handshakes in progress beyond which unvalidated addresses get a QUIC Retry and the rest are refused.
    pub max_handshakes: usize,
    /// Connections, handshakes included, from one IP address.
    pub max_per_address: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            states_per_sec: 60.0,
            state_burst: 30.0,
            events_per_sec: 20.0,
            event_burst: 40.0,
            window: Duration::from_secs(1),
            strikes: 3,
            max_handshakes: 256,
            max_per_address: 16,
        }
    }
}

/// What to do with something that arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Within the limits.
    Pass,
    /// Over the rate: drop it.
    Drop,
    /// A sustained flood: close the connection.
    Close,
}

/// A token bucket with flood detection.
#[derive(Debug, Clone)]
pub struct Bucket {
    rate: f32,
    burst: f32,
    tokens: f32,
    last: Instant,
    window: Duration,
    window_start: Instant,
    arrivals: u32,
    strikes: u32,
    max_strikes: u32,
}

impl Bucket {
    /// A full bucket admitting `rate` a second in bursts of `burst`, judging floods by `limits`.
    #[must_use]
    pub fn new(rate: f32, burst: f32, limits: &Limits, now: Instant) -> Self {
        Self {
            rate,
            burst,
            tokens: burst,
            last: now,
            window: limits.window,
            window_start: now,
            arrivals: 0,
            strikes: 0,
            max_strikes: limits.strikes,
        }
    }

    /// For client states, by `limits`.
    #[must_use]
    pub fn for_states(limits: &Limits, now: Instant) -> Self {
        Self::new(limits.states_per_sec, limits.state_burst, limits, now)
    }

    /// For events, by `limits`.
    #[must_use]
    pub fn for_events(limits: &Limits, now: Instant) -> Self {
        Self::new(limits.events_per_sec, limits.event_burst, limits, now)
    }

    /// Judges one arrival at `now`.
    pub fn admit(&mut self, now: Instant) -> Admit {
        let since = now.saturating_duration_since(self.window_start);
        if since >= self.window {
            // More than twice the rate over the window is a strike; a calmer window clears them.
            let allowed = 2.0 * self.rate * since.as_secs_f32();
            if self.arrivals as f32 > allowed {
                self.strikes += 1;
            } else {
                self.strikes = 0;
            }
            self.window_start = now;
            self.arrivals = 0;
            if self.strikes >= self.max_strikes {
                return Admit::Close;
            }
        }
        self.arrivals = self.arrivals.saturating_add(1);
        let elapsed = now.saturating_duration_since(self.last).as_secs_f32();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Admit::Pass
        } else {
            Admit::Drop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steady_client_passes_and_a_burst_is_cut() {
        let limits = Limits::default();
        let start = Instant::now();
        let mut bucket = Bucket::for_states(&limits, start);
        // 20 Hz for ten seconds: everything passes.
        for i in 0..200u32 {
            let now = start + Duration::from_millis(50 * u64::from(i));
            assert_eq!(bucket.admit(now), Admit::Pass, "state {i}");
        }
        // 100 at once: the burst passes, the rest is dropped.
        let now = start + Duration::from_secs(20);
        let passed = (0..100).filter(|_| bucket.admit(now) == Admit::Pass).count();
        assert_eq!(passed, 30);
    }

    #[test]
    fn a_sustained_flood_closes_and_a_short_one_does_not() {
        let limits = Limits::default();
        let start = Instant::now();
        let mut bucket = Bucket::for_states(&limits, start);
        let at = |ms: u64| start + Duration::from_millis(ms);
        // Two seconds at 1,000 a second, then a calm second: no close.
        let mut ms = 0;
        for _ in 0..2000 {
            assert_ne!(bucket.admit(at(ms)), Admit::Close);
            ms += 1;
        }
        for _ in 0..20 {
            assert_ne!(bucket.admit(at(ms)), Admit::Close);
            ms += 50;
        }
        // Then three seconds at 1,000 a second: closed at the third window's end.
        let mut closed = None;
        for i in 0..4000 {
            if bucket.admit(at(ms)) == Admit::Close {
                closed = Some(i);
                break;
            }
            ms += 1;
        }
        let closed = closed.expect("closed");
        assert!((2900..=3100).contains(&closed), "closed after {closed} states");
    }
}
