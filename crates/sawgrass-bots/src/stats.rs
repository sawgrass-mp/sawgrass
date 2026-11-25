//! Counters every bot adds to, and what the run reads from them.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sawgrass_protocol::game::ServerStats;

/// Lateness histogram buckets: bucket `i` holds values below `2^i` microseconds, the last one everything above.
const BUCKETS: usize = 24;

/// A histogram of durations with power-of-two buckets, safe to fill from many tasks.
#[derive(Debug)]
pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Histogram {
    /// Records one value.
    pub fn record(&self, value: Duration) {
        let micros = u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        let bucket = (u64::BITS - micros.leading_zeros()) as usize;
        self.buckets[bucket.min(BUCKETS - 1)].fetch_add(1, Ordering::Relaxed);
    }

    /// The counts so far.
    #[must_use]
    pub fn counts(&self) -> [u64; BUCKETS] {
        std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed))
    }

    /// The upper bound of the bucket that holds quantile `q` (0 to 1) of the values counted between `before`
    /// and `after`; `None` if there are none. The answer is within a factor of two.
    #[must_use]
    pub fn quantile(before: &[u64; BUCKETS], after: &[u64; BUCKETS], q: f64) -> Option<Duration> {
        let counts: Vec<u64> = after.iter().zip(before).map(|(a, b)| a - b).collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return None;
        }
        let rank = ((total as f64 * q).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, count) in counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return Some(Duration::from_micros(1u64 << i));
            }
        }
        None
    }
}

/// Totals over all bots of a run.
#[derive(Debug, Default)]
pub struct Counters {
    /// Bots that finished the handshake.
    pub connected: AtomicU64,
    /// Bots that could not connect before the run's stop, retries included.
    pub failed: AtomicU64,
    /// Connection attempts that failed and were tried again, as a game client does.
    pub connect_retries: AtomicU64,
    /// Bots whose connection ended before the run did.
    pub dropped: AtomicU64,
    /// State datagrams sent.
    pub sent_datagrams: AtomicU64,
    /// Bytes of state datagrams sent (headers included, QUIC overhead not).
    pub sent_bytes: AtomicU64,
    /// Snapshot datagrams received.
    pub received_datagrams: AtomicU64,
    /// Bytes of snapshot datagrams received.
    pub received_bytes: AtomicU64,
    /// Entity states received in snapshots.
    pub received_entities: AtomicU64,
    /// Distinct server ticks each bot saw a snapshot for, summed over bots.
    pub ticks_seen: AtomicU64,
    /// Of [`Counters::ticks_seen`], the ticks whose snapshot a bot decoded, so entities per tick need no guess.
    pub decoded_ticks: AtomicU64,
    /// How late each state send ran after its scheduled time. A generator that runs out of CPU sends late.
    pub send_lateness: Histogram,
    /// Payments sent to other bots (BOT-7).
    pub payments_sent: AtomicU64,
    /// Payments the server answered as done.
    pub payments_done: AtomicU64,
    /// Payments refused because the payer had too little money.
    pub payments_short: AtomicU64,
    /// Payments answered with any other refusal or failure.
    pub payments_refused: AtomicU64,
    /// Time from sending a payment to its answer, which leaves the server once the payment is on disk.
    pub payment_answer: Histogram,
    /// The latest figures the server reported, if it was started with `--stats-to-clients`.
    pub server: Mutex<Option<ServerStats>>,
    /// The first connection error, to show why bots failed.
    pub first_error: Mutex<Option<String>>,
}

impl Counters {
    /// Adds `n` to a counter.
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    /// Remembers `error` if it is the first one.
    pub fn error(&self, error: String) {
        if let Ok(mut first) = self.first_error.lock()
            && first.is_none()
        {
            *first = Some(error);
        }
    }
}

/// A copy of the counters at one moment, to compute rates between two of them.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// When it was taken, since the run started.
    pub at: Duration,
    /// [`Counters::connected`].
    pub connected: u64,
    /// [`Counters::failed`].
    pub failed: u64,
    /// [`Counters::connect_retries`].
    pub connect_retries: u64,
    /// [`Counters::dropped`].
    pub dropped: u64,
    /// [`Counters::sent_datagrams`].
    pub sent_datagrams: u64,
    /// [`Counters::sent_bytes`].
    pub sent_bytes: u64,
    /// [`Counters::received_datagrams`].
    pub received_datagrams: u64,
    /// [`Counters::received_bytes`].
    pub received_bytes: u64,
    /// [`Counters::received_entities`].
    pub received_entities: u64,
    /// [`Counters::ticks_seen`].
    pub ticks_seen: u64,
    /// [`Counters::decoded_ticks`].
    pub decoded_ticks: u64,
    /// [`Counters::send_lateness`].
    pub lateness: [u64; BUCKETS],
    /// [`Counters::payments_sent`].
    pub payments_sent: u64,
    /// [`Counters::payments_done`].
    pub payments_done: u64,
    /// [`Counters::payments_short`].
    pub payments_short: u64,
    /// [`Counters::payments_refused`].
    pub payments_refused: u64,
    /// [`Counters::payment_answer`].
    pub payment_answer: [u64; BUCKETS],
    /// CPU time of this process so far.
    pub cpu: Option<Duration>,
}

impl Counters {
    /// The counters now.
    #[must_use]
    pub fn sample(&self, at: Duration) -> Sample {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Sample {
            at,
            connected: load(&self.connected),
            failed: load(&self.failed),
            connect_retries: load(&self.connect_retries),
            dropped: load(&self.dropped),
            sent_datagrams: load(&self.sent_datagrams),
            sent_bytes: load(&self.sent_bytes),
            received_datagrams: load(&self.received_datagrams),
            received_bytes: load(&self.received_bytes),
            received_entities: load(&self.received_entities),
            ticks_seen: load(&self.ticks_seen),
            decoded_ticks: load(&self.decoded_ticks),
            lateness: self.send_lateness.counts(),
            payments_sent: load(&self.payments_sent),
            payments_done: load(&self.payments_done),
            payments_short: load(&self.payments_short),
            payments_refused: load(&self.payments_refused),
            payment_answer: self.payment_answer.counts(),
            cpu: crate::cpu::process_time(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_fall_in_power_of_two_buckets() {
        let h = Histogram::default();
        let empty = h.counts();
        for _ in 0..98 {
            h.record(Duration::from_micros(100));
        }
        h.record(Duration::from_millis(30));
        h.record(Duration::from_secs(1000));
        let counts = h.counts();
        assert_eq!(
            Histogram::quantile(&empty, &counts, 0.5),
            Some(Duration::from_micros(128))
        );
        assert_eq!(
            Histogram::quantile(&empty, &counts, 0.99),
            Some(Duration::from_micros(32_768))
        );
        assert_eq!(Histogram::quantile(&counts, &counts, 0.5), None);
    }
}
