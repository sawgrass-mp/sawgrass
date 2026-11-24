//! The load report: what the server measured, what the bots measured, and whether the generator kept up.

use std::fmt::Write as _;
use std::time::Duration;

use sawgrass_protocol::game::ServerStats;

use crate::stats::{Histogram, Sample};

/// Share of the machine's cores above which the generator counts as saturated.
pub const GENERATOR_CPU_LIMIT: f64 = 0.85;

/// What the report needs besides two samples.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// Bots the run started.
    pub bots: u64,
    /// State packets per second each bot sends.
    pub send_hz: u32,
    /// Cores this machine has.
    pub cores: usize,
}

/// Whether the load generator could be trusted over a period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generator {
    /// CPU and send timing were within limits.
    Ok,
    /// The generator used more than [`GENERATOR_CPU_LIMIT`] of its cores, or its sends ran later than one
    /// send period at p99: the figures say more about the generator than about the server.
    Saturated,
    /// Too little was measured to tell.
    Unknown,
}

/// The generator verdict for the period between `from` and `to`.
#[must_use]
pub fn generator(from: &Sample, to: &Sample, ctx: &Context) -> (Generator, Option<f64>, Option<Duration>) {
    let elapsed = (to.at - from.at).as_secs_f64();
    let cores = to
        .cpu
        .zip(from.cpu)
        .map(|(a, b)| a.saturating_sub(b).as_secs_f64() / elapsed.max(1e-9));
    let late_p99 = Histogram::quantile(&from.lateness, &to.lateness, 0.99);
    let period = Duration::from_secs(1) / ctx.send_hz.max(1);
    let verdict = match (cores, late_p99) {
        (Some(c), _) if c > GENERATOR_CPU_LIMIT * ctx.cores as f64 => Generator::Saturated,
        (_, Some(late)) if late > period => Generator::Saturated,
        (_, None) => Generator::Unknown,
        _ => Generator::Ok,
    };
    (verdict, cores, late_p99)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The report for the period between `from` and `to`.
#[must_use]
pub fn render(from: &Sample, to: &Sample, server: Option<&ServerStats>, ctx: &Context) -> String {
    let elapsed = (to.at - from.at).as_secs_f64().max(1e-9);
    let mut out = String::new();
    let connected = to.connected.saturating_sub(to.dropped);
    let _ = writeln!(
        out,
        "bots {connected}/{} connected ({} failed, {} dropped), period {:.0} s ending at {:.0} s",
        ctx.bots,
        to.failed,
        to.dropped,
        elapsed,
        to.at.as_secs_f64()
    );

    match server {
        Some(s) => {
            let us = |v: u32| f64::from(v) / 1000.0;
            let share = f64::from(s.tick_p99_us) / f64::from(s.tick_interval_us.max(1)) * 100.0;
            let cores = |v: u32| {
                if v == u32::MAX {
                    "unknown".to_owned()
                } else {
                    format!("{:.2}", f64::from(v) / 1000.0)
                }
            };
            let _ = writeln!(
                out,
                "server: {} players, tick p50 {:.2} ms, p99 {:.2} ms ({share:.1}% of {:.0} ms, budget 70%), max {:.2} ms, {} overruns",
                s.players,
                us(s.tick_p50_us),
                us(s.tick_p99_us),
                us(s.tick_interval_us),
                us(s.tick_max_us),
                s.overruns
            );
            let per_player = |v: u64| v as f64 / f64::from(s.players.max(1)) / 1000.0;
            let _ = writeln!(
                out,
                "server: cpu {} cores (tick thread {}, network the rest), down {:.2} kB/s and up {:.2} kB/s per player",
                cores(s.cpu_millicores),
                cores(s.tick_thread_millicores),
                per_player(s.sent_bytes_per_sec),
                per_player(s.received_bytes_per_sec)
            );
        }
        None => {
            let _ = writeln!(out, "server: no stats (start it with --stats-to-clients)");
        }
    }

    let n = connected.max(1) as f64;
    let rate = |a: u64, b: u64| a.saturating_sub(b) as f64 / elapsed;
    let datagrams = rate(to.received_datagrams, from.received_datagrams);
    let entities = rate(to.received_entities, from.received_entities);
    let ticks = rate(to.ticks_seen, from.ticks_seen);
    let _ = writeln!(
        out,
        "bots: down {:.2} kB/s per bot in {:.1} datagrams/s, {:.1} ticks/s with a snapshot, {:.1} entities per tick; up {:.2} kB/s per bot",
        rate(to.received_bytes, from.received_bytes) / n / 1000.0,
        datagrams / n,
        ticks / n,
        if ticks > 0.0 { entities / ticks } else { 0.0 },
        rate(to.sent_bytes, from.sent_bytes) / n / 1000.0,
    );

    let (verdict, cores, late_p99) = generator(from, to, ctx);
    let late_p50 = Histogram::quantile(&from.lateness, &to.lateness, 0.5);
    let fmt_late = |d: Option<Duration>| d.map_or_else(|| "n/a".to_owned(), |d| format!("<{:.2} ms", ms(d)));
    let _ = writeln!(
        out,
        "generator: cpu {} of {} cores, send lateness p50 {} p99 {} (period {:.0} ms): {}",
        cores.map_or_else(|| "unknown".to_owned(), |c| format!("{c:.2}")),
        ctx.cores,
        fmt_late(late_p50),
        fmt_late(late_p99),
        1000.0 / f64::from(ctx.send_hz.max(1)),
        match verdict {
            Generator::Ok => "ok",
            Generator::Saturated => "SATURATED, the figures above are not the server's limit",
            Generator::Unknown => "unknown",
        }
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            bots: 10,
            send_hz: 20,
            cores: 4,
        }
    }

    fn sample(at: u64, cpu: u64, late_bucket: usize) -> Sample {
        let mut s = Sample {
            at: Duration::from_secs(at),
            cpu: Some(Duration::from_secs(cpu)),
            connected: 10,
            ..Sample::default()
        };
        if at > 0 {
            s.lateness[late_bucket] = 100;
        }
        s
    }

    #[test]
    fn verdict_follows_cpu_and_lateness() {
        // 1 ms lateness (bucket 10, < 1.024 ms) and 1 core of 4: fine.
        let (v, cores, _) = generator(&sample(0, 0, 0), &sample(10, 10, 10), &ctx());
        assert_eq!(v, Generator::Ok);
        assert_eq!(cores, Some(1.0));
        // 3.6 cores of 4 is over 85%.
        assert_eq!(
            generator(&sample(0, 0, 0), &sample(10, 36, 10), &ctx()).0,
            Generator::Saturated
        );
        // Sends 65 ms late (bucket 17) with a 50 ms period.
        assert_eq!(
            generator(&sample(0, 0, 0), &sample(10, 10, 17), &ctx()).0,
            Generator::Saturated
        );
        assert_eq!(
            generator(&sample(0, 0, 0), &sample(0, 0, 0), &ctx()).0,
            Generator::Unknown
        );
    }

    #[test]
    fn report_names_server_and_generator() {
        let stats = ServerStats {
            tick_p50_us: 1_000,
            tick_p99_us: 3_500,
            tick_max_us: 9_000,
            tick_interval_us: 50_000,
            players: 10,
            cpu_millicores: 1_500,
            tick_thread_millicores: 200,
            sent_bytes_per_sec: 100_000,
            ..ServerStats::default()
        };
        let text = render(&sample(0, 0, 0), &sample(10, 10, 10), Some(&stats), &ctx());
        assert!(text.contains("p99 3.50 ms (7.0% of 50 ms"), "{text}");
        assert!(text.contains("cpu 1.50 cores (tick thread 0.20"), "{text}");
        assert!(text.contains("down 10.00 kB/s"), "{text}");
        assert!(text.contains(": ok"), "{text}");
        assert!(render(&sample(0, 0, 0), &sample(10, 10, 10), None, &ctx()).contains("--stats-to-clients"));
    }
}
