//! Clocks, a deterministic random generator, statistics and process accounting.

use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// splitmix64: small, fast and the same on every machine, so server and load generator build the same world.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [0, n).
    pub fn below(&mut self, n: usize) -> usize {
        (self.f64() * n as f64) as usize % n.max(1)
    }

    /// Standard normal, by Box-Muller.
    pub fn normal(&mut self) -> f64 {
        let u = self.f64().max(1e-12);
        let v = self.f64();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// Wall-clock nanoseconds, comparable between processes on the same machine.
pub fn wall_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// Flushes stdout: when it is a pipe, Rust buffers it in blocks, and the runner reads lines as they come.
pub fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn cpu_ticks(stat: &str) -> Duration {
    // Fields after the command name, which is in parentheses and may contain spaces; utime and stime are
    // fields 14 and 15 of the whole line, 12 and 13 after the name. CLK_TCK is 100 on Linux x86_64.
    let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
        + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    Duration::from_millis(ticks * 10)
}

/// CPU time used by this process so far (all threads, user plus system).
pub fn process_cpu() -> Duration {
    cpu_ticks(&fs::read_to_string("/proc/self/stat").unwrap_or_default())
}

/// Resident memory of this process, in bytes.
pub fn process_rss() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// Cores used by the busiest thread between two calls: a thread near 1.0 is saturated even when the process
/// total looks moderate.
#[derive(Debug, Default)]
pub struct ThreadCpu {
    last: std::collections::HashMap<String, Duration>,
}

impl ThreadCpu {
    fn sample() -> std::collections::HashMap<String, Duration> {
        let mut out = std::collections::HashMap::new();
        if let Ok(dir) = fs::read_dir("/proc/self/task") {
            for entry in dir.flatten() {
                let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
                out.insert(entry.file_name().to_string_lossy().into_owned(), cpu_ticks(&stat));
            }
        }
        out
    }

    pub fn new() -> Self {
        Self { last: Self::sample() }
    }

    pub fn busiest(&mut self, elapsed: Duration) -> f64 {
        let now = Self::sample();
        let max = now
            .iter()
            .map(|(tid, t)| t.saturating_sub(self.last.get(tid).copied().unwrap_or_default()))
            .max()
            .unwrap_or_default();
        self.last = now;
        max.as_secs_f64() / elapsed.as_secs_f64().max(1e-9)
    }
}

/// Percentile of a sorted slice, by nearest rank.
pub fn pct<T: Copy + Default>(sorted: &[T], p: f64) -> T {
    if sorted.is_empty() {
        return T::default();
    }
    let i = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[i.clamp(1, sorted.len()) - 1]
}
