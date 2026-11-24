//! Messages, framing, sockets, statistics and process accounting shared by every part of the harness.
//!
//! Process accounting (`process_cpu`, `process_rss`, `ThreadCpu`) is the NET-3 harness's
//! (`../../network-transport/bench/src/common.rs`), so CPU figures are read the same way.

use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// A client's 20 Hz state, which the server only counts.
pub const KIND_STATE: u8 = 2;
/// A server snapshot.
pub const KIND_SNAPSHOT: u8 = 3;
/// A reliable event, echoed back reliably.
pub const KIND_EVENT: u8 = 4;

/// Size of a client state datagram, in bytes (about one `EntityState` plus input).
pub const STATE_BYTES: usize = 64;
/// Size of a reliable event, in bytes.
pub const EVENT_BYTES: usize = 100;
/// Size of a snapshot datagram: a delta-encoded snapshot of 100 entities, which NET-5 measured at 793 bytes and
/// sends as one datagram a tick.
pub const SNAPSHOT_BYTES: usize = 800;
/// Game tick rate.
pub const TICK_HZ: u32 = 20;
/// Receive and send buffer requested for every UDP socket.
pub const SOCKET_BUFFER: usize = 8 << 20;
/// Largest reliable event either side accepts, as in `sawgrass-gateway`.
pub const MAX_EVENT_LEN: usize = 64 * 1024;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Nanoseconds since this process first asked.
pub fn now_ns() -> u64 {
    epoch().elapsed().as_nanos() as u64
}

/// Flushes stdout: when it is a pipe, Rust buffers it in blocks, and the runner reads lines as they come.
pub fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// Builds a message: kind, client id, sequence and send time, padded with a pattern to `len` bytes.
pub fn message(kind: u8, client: u32, seq: u64, len: usize) -> Vec<u8> {
    let mut m = vec![0u8; len];
    m[0] = kind;
    m[1..5].copy_from_slice(&client.to_le_bytes());
    m[5..13].copy_from_slice(&seq.to_le_bytes());
    m[13..21].copy_from_slice(&now_ns().to_le_bytes());
    for (i, b) in m[21..].iter_mut().enumerate() {
        *b = (i * 31 + 7) as u8;
    }
    m
}

/// The send time written into a message by [`message`].
pub fn sent_at(m: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(m.get(13..21)?.try_into().ok()?))
}

/// A message framed as on the gateway's event stream: `u32` little-endian length, then the bytes.
pub fn frame(m: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(4 + m.len());
    f.extend_from_slice(&(m.len() as u32).to_le_bytes());
    f.extend_from_slice(m);
    f
}

/// A UDP socket with large buffers, bound to `addr`; with `reuse_port` several sockets can share the port and
/// the kernel spreads peers over them by address.
pub fn udp_socket(addr: SocketAddr, reuse_port: bool) -> std::io::Result<UdpSocket> {
    let domain = socket2::Domain::for_address(addr);
    let s = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    s.set_recv_buffer_size(SOCKET_BUFFER)?;
    s.set_send_buffer_size(SOCKET_BUFFER)?;
    if reuse_port {
        s.set_reuse_port(true)?;
    }
    s.bind(&addr.into())?;
    Ok(s.into())
}

/// The address of `0.0.0.0:0` or `[::]:0` matching `peer`.
pub fn any_addr(peer: SocketAddr) -> SocketAddr {
    if peer.is_ipv4() {
        "0.0.0.0:0".parse().expect("valid")
    } else {
        "[::]:0".parse().expect("valid")
    }
}

fn stat_ticks(stat: &str) -> Duration {
    // Fields after the command name, which is in parentheses and may contain spaces.
    let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15 of the whole line, 12 and 13 after the name.
    let ticks: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
        + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    // CLK_TCK is 100 on Linux x86_64.
    Duration::from_millis(ticks * 10)
}

/// CPU time used by this process so far (all threads, user plus system).
pub fn process_cpu() -> Duration {
    stat_ticks(&fs::read_to_string("/proc/self/stat").unwrap_or_default())
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

/// CPU time of each thread of this process, to find the busiest one: a thread near 1.0 core is saturated even
/// when the process total looks moderate.
#[derive(Debug, Default)]
pub struct ThreadCpu {
    last: std::collections::HashMap<String, Duration>,
}

impl ThreadCpu {
    fn sample() -> std::collections::HashMap<String, Duration> {
        let mut out = std::collections::HashMap::new();
        let Ok(dir) = fs::read_dir("/proc/self/task") else {
            return out;
        };
        for entry in dir.flatten() {
            let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
            out.insert(
                entry.file_name().to_string_lossy().into_owned(),
                stat_ticks(&stat),
            );
        }
        out
    }

    pub fn new() -> Self {
        Self { last: Self::sample() }
    }

    /// Cores used by the busiest thread since the last call, over `elapsed`.
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

/// Prints a server report line each time it is called: connections, CPU, memory and message rates.
#[derive(Debug)]
pub struct Reporter {
    last: Instant,
    last_cpu: Duration,
    threads: ThreadCpu,
}

impl Reporter {
    pub fn new() -> Self {
        Self {
            last: Instant::now(),
            last_cpu: process_cpu(),
            threads: ThreadCpu::new(),
        }
    }

    pub fn report(&mut self, conns: usize, received: u64, sent: u64) {
        let elapsed = self.last.elapsed();
        let secs = elapsed.as_secs_f64();
        let cpu = process_cpu();
        println!(
            "report conns={conns} cpu_cores={:.3} busiest_thread_cores={:.3} rss_mb={:.1} recv_per_s={:.0} sent_per_s={:.0}",
            (cpu - self.last_cpu).as_secs_f64() / secs,
            self.threads.busiest(elapsed),
            process_rss() as f64 / 1048576.0,
            received as f64 / secs,
            sent as f64 / secs,
        );
        flush();
        self.last = Instant::now();
        self.last_cpu = cpu;
    }
}

/// Percentile of sorted samples, nearest rank.
pub fn percentile<T: Copy + Default>(sorted: &[T], q: f64) -> T {
    if sorted.is_empty() {
        return T::default();
    }
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

/// The allocator this binary was built with.
pub fn allocator() -> &'static str {
    if cfg!(feature = "mimalloc") {
        "mimalloc"
    } else if cfg!(feature = "jemalloc") {
        "jemalloc"
    } else {
        "system"
    }
}
