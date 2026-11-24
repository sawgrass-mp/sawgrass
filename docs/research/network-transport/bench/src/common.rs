//! Message layout, clocks, statistics and process accounting shared by every transport.

use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// A ping that the server echoes back on the same channel.
pub const KIND_PING: u8 = 1;
/// A client's 20 Hz state, which the server only counts.
pub const KIND_STATE: u8 = 2;
/// A server snapshot.
pub const KIND_SNAPSHOT: u8 = 3;
/// A reliable event, echoed back reliably.
pub const KIND_EVENT: u8 = 4;

/// Size of a ping, in bytes.
pub const PING_BYTES: usize = 64;
/// Size of a client state message, in bytes (about one `EntityState` plus input).
pub const STATE_BYTES: usize = 64;
/// Size of a reliable event, in bytes.
pub const EVENT_BYTES: usize = 100;
/// Size of one snapshot datagram, in bytes: 1000 unless `SNAPSHOT_BYTES` says otherwise (NET-5 sets it to the
/// compressed sizes it measures).
pub fn snapshot_bytes() -> usize {
    env_usize("SNAPSHOT_BYTES", 1000)
}

/// Snapshot datagrams per client per tick: 2 x 1000 bytes at 20 Hz is the 40 KB/s budget in architecture.md.
/// `SNAPSHOTS_PER_TICK` overrides it.
pub fn snapshots_per_tick() -> usize {
    env_usize("SNAPSHOTS_PER_TICK", 2)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
/// Game tick rate.
pub const TICK_HZ: u32 = 20;
/// Network update rate of poll-driven servers and clients under load.
pub const UPDATE_HZ: u32 = 60;

/// Receive and send buffer requested for every UDP socket.
pub const SOCKET_BUFFER: usize = 8 << 20;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Number of servers on consecutive ports, from `SHARDS` (default 1), for transports whose server has a
/// connection limit below the test's (renet: 1024, ENet: 4095). Clients pick `port + id % SHARDS`.
pub fn shards() -> u16 {
    std::env::var("SHARDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

/// The shard address for client `id`.
pub fn shard_addr(server: SocketAddr, id: u32) -> SocketAddr {
    SocketAddr::new(server.ip(), server.port() + (id % u32::from(shards())) as u16)
}

/// Flushes stdout: when it is a pipe, Rust buffers it in blocks, and the runner reads lines as they come.
pub fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// Nanoseconds since this process first asked.
pub fn now_ns() -> u64 {
    epoch().elapsed().as_nanos() as u64
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

/// Reads kind, client id, sequence and send time back out of a message.
pub fn parse(m: &[u8]) -> Option<(u8, u32, u64, u64)> {
    if m.len() < 21 {
        return None;
    }
    Some((
        m[0],
        u32::from_le_bytes(m[1..5].try_into().ok()?),
        u64::from_le_bytes(m[5..13].try_into().ok()?),
        u64::from_le_bytes(m[13..21].try_into().ok()?),
    ))
}

/// A UDP socket with large buffers, bound to `addr`.
pub fn udp_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let domain = socket2::Domain::for_address(addr);
    let s = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    s.set_recv_buffer_size(SOCKET_BUFFER)?;
    s.set_send_buffer_size(SOCKET_BUFFER)?;
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

/// CPU time used by this process so far (all threads, user plus system).
pub fn process_cpu() -> Duration {
    let stat = fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // Fields after the command name, which is in parentheses and may contain spaces.
    let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15 of the whole line, 12 and 13 after the name.
    let ticks: u64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
        + fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    // CLK_TCK is 100 on Linux x86_64.
    Duration::from_millis(ticks * 10)
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
            let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
            let f: Vec<&str> = rest.split_whitespace().collect();
            let ticks: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
                + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
            out.insert(
                entry.file_name().to_string_lossy().into_owned(),
                Duration::from_millis(ticks * 10),
            );
        }
        out
    }

    /// CPU time the calling thread has used so far.
    pub fn current() -> Duration {
        let stat = fs::read_to_string("/proc/thread-self/stat").unwrap_or_default();
        let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ticks: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
            + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
        Duration::from_millis(ticks * 10)
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

/// Prints a server report line every `every`: connections, CPU, memory and message rates.
#[derive(Debug)]
pub struct Reporter {
    every: Duration,
    last: Instant,
    last_cpu: Duration,
    threads: ThreadCpu,
    /// Messages received since the last report.
    pub received: u64,
    /// Messages sent since the last report.
    pub sent: u64,
}

impl Reporter {
    pub fn new(every: Duration) -> Self {
        Self {
            every,
            last: Instant::now(),
            last_cpu: process_cpu(),
            threads: ThreadCpu::new(),
            received: 0,
            sent: 0,
        }
    }

    /// Prints a line if the interval has passed.
    pub fn tick(&mut self, connections: usize) {
        let elapsed = self.last.elapsed();
        if elapsed < self.every {
            return;
        }
        let cpu = process_cpu();
        let used = (cpu - self.last_cpu).as_secs_f64() / elapsed.as_secs_f64();
        let secs = elapsed.as_secs_f64();
        let busiest = self.threads.busiest(elapsed);
        println!(
            "report conns={connections} cpu_cores={used:.3} busiest_thread_cores={busiest:.3} rss_mb={:.1} recv_per_s={:.0} sent_per_s={:.0}",
            process_rss() as f64 / 1048576.0,
            self.received as f64 / secs,
            self.sent as f64 / secs,
        );
        crate::common::flush();
        self.last = Instant::now();
        self.last_cpu = cpu;
        self.received = 0;
        self.sent = 0;
    }
}

/// Round trips of the pings a client sent, by sequence number.
#[derive(Debug)]
pub struct PingLog {
    sent_at: Vec<u64>,
    rtt: Vec<Option<u64>>,
}

impl PingLog {
    pub fn new() -> Self {
        Self {
            sent_at: Vec::new(),
            rtt: Vec::new(),
        }
    }

    /// Makes the next ping and records when it was sent.
    pub fn next(&mut self, len: usize) -> Vec<u8> {
        let seq = self.sent_at.len() as u64;
        let m = message(KIND_PING, 0, seq, len);
        self.sent_at.push(now_ns());
        self.rtt.push(None);
        m
    }

    /// Records an echo.
    pub fn echo(&mut self, m: &[u8]) {
        if let Some((KIND_PING, _, seq, sent)) = parse(m)
            && let Some(slot) = self.rtt.get_mut(seq as usize)
            && slot.is_none()
        {
            *slot = Some(now_ns().saturating_sub(sent));
        }
    }

    /// Prints sent, received, percentiles and the worst round trip, in milliseconds.
    pub fn report(&self, label: &str) {
        let mut v: Vec<u64> = self.rtt.iter().flatten().copied().collect();
        v.sort_unstable();
        let pct = |p: f64| -> f64 {
            if v.is_empty() {
                return f64::NAN;
            }
            let i = ((v.len() as f64 - 1.0) * p).round() as usize;
            v[i] as f64 / 1e6
        };
        println!(
            "ping {label} sent={} received={} delivered_pct={:.2} p50_ms={:.3} p99_ms={:.3} p999_ms={:.3} max_ms={:.3}",
            self.rtt.len(),
            v.len(),
            100.0 * v.len() as f64 / self.rtt.len().max(1) as f64,
            pct(0.5),
            pct(0.99),
            pct(0.999),
            pct(1.0),
        );
        crate::common::flush();
    }
}

/// Totals a load-generating process prints at the end.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoadTotals {
    pub connected: u64,
    pub snapshots: u64,
    pub events_back: u64,
    pub events_sent: u64,
    /// CPU time of the busiest client thread, read by each thread itself before it exits.
    pub thread_cpu: Duration,
}

impl LoadTotals {
    pub fn add(&mut self, o: Self) {
        self.thread_cpu = self.thread_cpu.max(o.thread_cpu);
        self.connected += o.connected;
        self.snapshots += o.snapshots;
        self.events_back += o.events_back;
        self.events_sent += o.events_sent;
    }

    pub fn report(
        &self,
        clients: usize,
        measured: Duration,
        started: Instant,
        cpu0: Duration,
        threads: &mut ThreadCpu,
    ) {
        // Threads that already exited are gone from /proc, so also take what each one read about itself.
        let busiest = threads
            .busiest(started.elapsed())
            .max(self.thread_cpu.as_secs_f64() / started.elapsed().as_secs_f64().max(1e-9));
        let secs = measured.as_secs_f64();
        let expected = clients as f64 * f64::from(TICK_HZ) * snapshots_per_tick() as f64 * secs;
        let cpu = (process_cpu() - cpu0).as_secs_f64() / started.elapsed().as_secs_f64();
        println!(
            "load clients={clients} connected={} snapshots_per_client_s={:.1} snapshot_delivery_pct={:.1} events_sent={} events_back={} client_cpu_cores={cpu:.2} client_busiest_thread_cores={busiest:.2}",
            self.connected,
            self.snapshots as f64 / clients.max(1) as f64 / secs,
            100.0 * self.snapshots as f64 / expected.max(1.0),
            self.events_sent,
            self.events_back,
        );
        crate::common::flush();
    }
}

/// Sleeps until `deadline` if it is in the future.
pub fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline > now {
        std::thread::sleep(deadline - now);
    }
}
