//! Our own voice relay over QUIC through quinn: clients send Opus frames as datagrams on the connection they
//! already have; the server sends each frame to everyone in hearing range, on the speaker's radio channel or in
//! their phone call, either at once (one datagram per frame and listener) or batched (one datagram per listener
//! every flush interval, holding every frame that listener should hear).
//!
//! Datagrams:
//! - hello: `[3, id u16]`, the client's player index;
//! - frame: `[1, speaker u16, seq u16, flags u8, payload]`, the payload starting with the send time;
//! - batch: `[2, count u8]` then per frame `[speaker u16, seq u16, flags u8, len u8, payload]`.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime};

use crate::common::*;
use crate::world::{Speech, World};

const CERT_PATH: &str = "/tmp/voice-bench-cert.der";
const KIND_FRAME: u8 = 1;
const KIND_BATCH: u8 = 2;
const KIND_HELLO: u8 = 3;
/// Largest batch payload: under the 1,414 bytes quinn sends on this path (NET-5), with room to spare.
const MAX_BATCH: usize = 1_350;
const SOCKET_BUFFER: usize = 8 << 20;

fn transport() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("idle")));
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.datagram_receive_buffer_size(Some(1 << 20));
    t.datagram_send_buffer_size(1 << 20);
    t
}

fn udp_socket(addr: SocketAddr) -> UdpSocket {
    let s = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .expect("socket");
    s.set_recv_buffer_size(SOCKET_BUFFER).expect("rcvbuf");
    s.set_send_buffer_size(SOCKET_BUFFER).expect("sndbuf");
    s.bind(&addr.into()).expect("bind");
    s.into()
}

fn endpoint(addr: SocketAddr, server: Option<ServerConfig>) -> Endpoint {
    Endpoint::new(
        EndpointConfig::default(),
        server,
        udp_socket(addr),
        Arc::new(TokioRuntime),
    )
    .expect("endpoint")
}

fn client_endpoint() -> Endpoint {
    let der = std::fs::read(CERT_PATH).expect("server certificate; start the server first");
    let mut roots = quinn::rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(der)).expect("root");
    let mut config = ClientConfig::with_root_certificates(Arc::new(roots)).expect("client config");
    config.transport_config(Arc::new(transport()));
    let any: SocketAddr = "0.0.0.0:0".parse().expect("addr");
    let mut e = endpoint(any, None);
    e.set_default_client_config(config);
    e
}

#[derive(Default)]
struct Counters {
    frames_in: AtomicU64,
    frames_out: AtomicU64,
    datagrams_out: AtomicU64,
    bytes_out: AtomicU64,
    route_ns: AtomicU64,
}

struct Relay {
    world: World,
    slots: Vec<OnceLock<Connection>>,
    batches: Vec<Mutex<Vec<u8>>>,
    batch: bool,
    counters: Counters,
}

impl Relay {
    fn send(&self, to: u32, data: Bytes) {
        if let Some(c) = self.slots[to as usize].get() {
            let len = data.len() as u64;
            if c.send_datagram(data).is_ok() {
                self.counters.datagrams_out.fetch_add(1, Relaxed);
                self.counters.bytes_out.fetch_add(len, Relaxed);
            }
        }
    }

    fn flush(&self, to: u32, buf: &mut Vec<u8>) {
        if buf.len() > 2 {
            let full = std::mem::replace(buf, vec![KIND_BATCH, 0]);
            self.send(to, Bytes::from(full));
        }
    }

    fn on_frame(&self, speaker: u32, d: Bytes, scratch: &mut Vec<u32>) {
        if d.len() < 7 {
            return;
        }
        self.counters.frames_in.fetch_add(1, Relaxed);
        let t = Instant::now();
        self.world.recipients(speaker, d[5], scratch);
        self.counters
            .route_ns
            .fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        self.counters.frames_out.fetch_add(scratch.len() as u64, Relaxed);
        if !self.batch {
            for &r in scratch.iter() {
                self.send(r, d.clone());
            }
            return;
        }
        let payload = &d[6..];
        for &r in scratch.iter() {
            let mut buf = self.batches[r as usize].lock().expect("batch");
            if buf.len() + 6 + payload.len() > MAX_BATCH || buf[1] == u8::MAX {
                self.flush(r, &mut buf);
            }
            buf[1] += 1;
            buf.extend_from_slice(&d[1..6]);
            buf.push(payload.len() as u8);
            buf.extend_from_slice(payload);
        }
    }
}

pub fn serve(addr: SocketAddr, players: usize, seed: u64, batch: bool, flush_ms: u64, threads: usize) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async move {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("cert");
        std::fs::write(CERT_PATH, cert.cert.der()).expect("write cert");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let mut config = ServerConfig::with_single_cert(vec![cert.cert.der().clone()], key).expect("server config");
        config.transport_config(Arc::new(transport()));
        let endpoint = endpoint(addr, Some(config));
        let relay = Arc::new(Relay {
            world: World::new(players, seed),
            slots: (0..players).map(|_| OnceLock::new()).collect(),
            batches: (0..players).map(|_| Mutex::new(vec![KIND_BATCH, 0])).collect(),
            batch,
            counters: Counters::default(),
        });
        println!("ready players={players}");
        flush();

        {
            let relay = Arc::clone(&relay);
            tokio::spawn(async move {
                let mut last = Instant::now();
                let mut last_cpu = process_cpu();
                let mut thread_cpu = ThreadCpu::new();
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let secs = last.elapsed().as_secs_f64();
                    let cpu = process_cpu();
                    let c = &relay.counters;
                    let conns = relay.slots.iter().filter(|s| s.get().is_some_and(|c| c.close_reason().is_none())).count();
                    let frames_in = c.frames_in.swap(0, Relaxed) as f64;
                    println!(
                        "report conns={conns} cpu_cores={:.3} busiest_thread_cores={:.3} rss_mb={:.1} frames_in_per_s={:.0} frames_out_per_s={:.0} datagrams_out_per_s={:.0} payload_out_bytes_per_s={:.0} route_ns_per_frame={:.0}",
                        (cpu - last_cpu).as_secs_f64() / secs,
                        thread_cpu.busiest(last.elapsed()),
                        process_rss() as f64 / 1_048_576.0,
                        frames_in / secs,
                        c.frames_out.swap(0, Relaxed) as f64 / secs,
                        c.datagrams_out.swap(0, Relaxed) as f64 / secs,
                        c.bytes_out.swap(0, Relaxed) as f64 / secs,
                        c.route_ns.swap(0, Relaxed) as f64 / frames_in.max(1.0),
                    );
                    flush();
                    last = Instant::now();
                    last_cpu = cpu;
                }
            });
        }

        if batch {
            let relay = Arc::clone(&relay);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(flush_ms));
                loop {
                    tick.tick().await;
                    for (i, b) in relay.batches.iter().enumerate() {
                        let mut buf = b.lock().expect("batch");
                        relay.flush(i as u32, &mut buf);
                    }
                }
            });
        }

        while let Some(incoming) = endpoint.accept().await {
            let relay = Arc::clone(&relay);
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                let mut id: Option<u32> = None;
                let mut scratch = Vec::with_capacity(256);
                while let Ok(d) = conn.read_datagram().await {
                    match (d.first(), id) {
                        (Some(&KIND_HELLO), None) if d.len() >= 3 => {
                            let i = u32::from(u16::from_le_bytes([d[1], d[2]]));
                            if (i as usize) < relay.slots.len() && relay.slots[i as usize].set(conn.clone()).is_ok() {
                                id = Some(i);
                            }
                        }
                        (Some(&KIND_FRAME), Some(i)) => relay.on_frame(i, d, &mut scratch),
                        _ => {}
                    }
                }
            });
        }
    });
}

/// Delivery latency, 10 µs buckets up to 1 s.
struct Histogram(Vec<AtomicU32>);

impl Histogram {
    const BUCKET_NS: u64 = 10_000;

    fn new() -> Self {
        Self((0..100_000).map(|_| AtomicU32::new(0)).collect())
    }

    fn record(&self, ns: u64) {
        let i = ((ns / Self::BUCKET_NS) as usize).min(self.0.len() - 1);
        self.0[i].fetch_add(1, Relaxed);
    }

    fn percentile_ms(&self, p: f64) -> f64 {
        let counts: Vec<u64> = self.0.iter().map(|c| u64::from(c.load(Relaxed))).collect();
        let total: u64 = counts.iter().sum();
        let target = ((p / 100.0) * total as f64).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, c) in counts.iter().enumerate() {
            seen += c;
            if seen >= target {
                return (i as u64 * Self::BUCKET_NS + Self::BUCKET_NS / 2) as f64 / 1e6;
            }
        }
        0.0
    }
}

struct Load {
    measuring: AtomicBool,
    received: AtomicU64,
    expected: AtomicU64,
    sent: AtomicU64,
    datagrams: AtomicU64,
    latency: Histogram,
}

fn on_receive(load: &Load, d: &[u8]) {
    if !load.measuring.load(Relaxed) {
        return;
    }
    load.datagrams.fetch_add(1, Relaxed);
    let now = wall_ns();
    let stamp = |p: &[u8]| {
        if p.len() >= 8 {
            let ts = u64::from_le_bytes(p[..8].try_into().unwrap_or_default());
            load.latency.record(now.saturating_sub(ts));
        }
        load.received.fetch_add(1, Relaxed);
    };
    match d.first() {
        Some(&KIND_FRAME) if d.len() >= 6 => stamp(&d[6..]),
        Some(&KIND_BATCH) if d.len() >= 2 => {
            let mut at = 2;
            for _ in 0..d[1] {
                if at + 6 > d.len() {
                    break;
                }
                let len = usize::from(d[at + 5]);
                let end = (at + 6 + len).min(d.len());
                stamp(&d[at + 6..end]);
                at = end;
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
pub fn load(
    server: SocketAddr,
    players: usize,
    seed: u64,
    share: f64,
    payload: usize,
    frame_ms: u64,
    threads: usize,
    secs: u64,
) {
    let started = Instant::now();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async move {
        let world = Arc::new(World::new(players, seed));
        // How many listeners each speaker reaches with each set of flags; positions do not change.
        let mut scratch = Vec::new();
        let reach: Arc<Vec<[u32; 16]>> = Arc::new(
            (0..players as u32)
                .map(|s| {
                    let mut row = [0u32; 16];
                    for (f, r) in row.iter_mut().enumerate() {
                        world.recipients(s, f as u8, &mut scratch);
                        *r = scratch.len() as u32;
                    }
                    row
                })
                .collect(),
        );
        let load = Arc::new(Load {
            measuring: AtomicBool::new(false),
            received: AtomicU64::new(0),
            expected: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            datagrams: AtomicU64::new(0),
            latency: Histogram::new(),
        });
        let connected = Arc::new(AtomicUsize::new(0));
        let group = 64;
        for g in 0..players.div_ceil(group) {
            let range = g * group..((g + 1) * group).min(players);
            let (world, reach, load, connected) =
                (Arc::clone(&world), Arc::clone(&reach), Arc::clone(&load), Arc::clone(&connected));
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(g as u64 * 40)).await;
                let endpoint = client_endpoint();
                let mut conns = Vec::new();
                for k in range.clone() {
                    let Ok(connecting) = endpoint.connect(server, "localhost") else { continue };
                    let Ok(conn) = connecting.await else { continue };
                    let id = (k as u16).to_le_bytes();
                    let _ = conn.send_datagram(Bytes::from(vec![KIND_HELLO, id[0], id[1]]));
                    let rx = conn.clone();
                    let load = Arc::clone(&load);
                    tokio::spawn(async move {
                        while let Ok(d) = rx.read_datagram().await {
                            on_receive(&load, &d);
                        }
                    });
                    conns.push((k, conn));
                    connected.fetch_add(1, Relaxed);
                }
                let mut speech = Speech::new(&world, share, seed ^ (g as u64 + 1));
                let mut tick = tokio::time::interval(Duration::from_millis(frame_ms));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
                let mut seq: u16 = 0;
                let filler: Vec<u8> = (0..payload).map(|i| (i * 31 + 7) as u8).collect();
                loop {
                    tick.tick().await;
                    // The speech model runs in 20 ms steps whatever the frame length.
                    for _ in 0..(frame_ms / 20).max(1) {
                        speech.step(&world, range.clone());
                    }
                    seq = seq.wrapping_add(1);
                    let measuring = load.measuring.load(Relaxed);
                    for (k, conn) in &conns {
                        let Some(flags) = speech.state[*k] else { continue };
                        let id = (*k as u16).to_le_bytes();
                        let s = seq.to_le_bytes();
                        let mut m = Vec::with_capacity(6 + payload);
                        m.extend_from_slice(&[KIND_FRAME, id[0], id[1], s[0], s[1], flags]);
                        m.extend_from_slice(&wall_ns().to_le_bytes());
                        m.extend_from_slice(&filler[8.min(payload)..]);
                        if conn.send_datagram(Bytes::from(m)).is_ok() && measuring {
                            load.sent.fetch_add(1, Relaxed);
                            load.expected.fetch_add(u64::from(reach[*k][usize::from(flags & 0x0f)]), Relaxed);
                        }
                    }
                }
            });
        }
        while connected.load(Relaxed) < players && started.elapsed() < Duration::from_secs(180) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!("connected all={} after_s={:.1}", connected.load(Relaxed), started.elapsed().as_secs_f64());
        flush();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let cpu0 = process_cpu();
        let mut thread_cpu = ThreadCpu::new();
        let t0 = Instant::now();
        load.measuring.store(true, Relaxed);
        println!("measuring start");
        flush();
        tokio::time::sleep(Duration::from_secs(secs)).await;
        load.measuring.store(false, Relaxed);
        println!("measuring stop");
        flush();
        let elapsed = t0.elapsed();
        // Frames still in flight when measuring stopped count once they arrive.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let s = elapsed.as_secs_f64();
        let sent = load.sent.load(Relaxed) as f64;
        let expected = load.expected.load(Relaxed) as f64;
        let received = load.received.load(Relaxed) as f64;
        let frames_per_speaker = (1000 / frame_ms) as f64;
        println!(
            "result players={players} share={share} frame_ms={frame_ms} payload={payload} speaking_mean={:.0} frames_sent_per_s={:.0} frames_expected_per_s={:.0} frames_received_per_s={:.0} datagrams_received_per_s={:.0} delivered={:.4} lat_p50_ms={:.2} lat_p99_ms={:.2} lat_p999_ms={:.2} gen_cpu_cores={:.3} gen_busiest_thread={:.3}",
            sent / s / frames_per_speaker,
            sent / s,
            expected / s,
            received / s,
            load.datagrams.load(Relaxed) as f64 / s,
            received / expected.max(1.0),
            load.latency.percentile_ms(50.0),
            load.latency.percentile_ms(99.0),
            load.latency.percentile_ms(99.9),
            (process_cpu() - cpu0).as_secs_f64() / (s + 0.5),
            thread_cpu.busiest(t0.elapsed()),
        );
        flush();
    });
}
