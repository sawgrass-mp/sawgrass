//! The server and client loops for transports that are driven by polling (plain UDP, ENet, renet,
//! GameNetworkingSockets). QUIC through quinn is async and has its own loops in `quic.rs`.

use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use mio::{Events, Interest, Poll, Token};

use crate::common::*;

/// A received message: connection, whether it came on the reliable channel, and the bytes.
pub type Received = (u64, bool, Vec<u8>);

pub trait PollServer {
    /// Sockets to wait on in echo mode (empty if the transport has its own wait).
    fn sockets(&self) -> Vec<&UdpSocket>;
    /// Processes everything that arrived and returns the messages.
    fn receive(&mut self) -> Vec<Received>;
    /// Queues a message to one connection.
    fn send(&mut self, conn: u64, reliable: bool, data: &[u8]);
    /// Connected connections.
    fn connections(&self) -> Vec<u64>;
    /// Sends what is queued.
    fn flush(&mut self);
}

pub trait PollClient: Sized {
    fn connect(server: SocketAddr, id: u32) -> Self;
    fn socket(&self) -> Option<&UdpSocket>;
    fn receive(&mut self) -> Vec<(bool, Vec<u8>)>;
    fn send(&mut self, reliable: bool, data: &[u8]);
    fn connected(&self) -> bool;
    fn flush(&mut self);
}

fn register(poll: &Poll, sockets: &[&UdpSocket]) -> Vec<mio::net::UdpSocket> {
    // Clones share the socket, so readiness on the clone wakes us for the transport's own socket.
    let mut keep = Vec::new();
    for (i, s) in sockets.iter().enumerate() {
        let mut m = mio::net::UdpSocket::from_std(s.try_clone().expect("clone socket"));
        poll.registry()
            .register(&mut m, Token(i), Interest::READABLE)
            .expect("register");
        keep.push(m);
    }
    keep
}

/// Runs a server. In echo mode it wakes on every datagram (for latency tests); in load mode it updates the
/// network at `UPDATE_HZ` and sends snapshots at `TICK_HZ`, as a game server loop would.
pub fn serve<S: PollServer>(mut server: S, load: bool) {
    let mut poll = Poll::new().expect("poll");
    let _keep = if load {
        Vec::new()
    } else {
        register(&poll, &server.sockets())
    };
    // A transport with no socket of ours to wait on (GNS) is polled in a spin loop in echo mode.
    let spin = server.sockets().is_empty();
    let mut events = Events::with_capacity(64);
    let mut reporter = Reporter::new(Duration::from_secs(5));
    let update = Duration::from_secs(1) / UPDATE_HZ;
    let tick = Duration::from_secs(1) / TICK_HZ;
    let mut next_update = Instant::now();
    let mut next_tick = Instant::now() + tick;
    let snapshot = message(KIND_SNAPSHOT, 0, 0, SNAPSHOT_BYTES);
    loop {
        if load {
            next_update += update;
            sleep_until(next_update);
        } else if spin {
            thread::yield_now();
        } else {
            let _ = poll.poll(&mut events, Some(Duration::from_millis(1)));
        }
        for (conn, reliable, data) in server.receive() {
            reporter.received += 1;
            match data.first() {
                Some(&KIND_PING) => {
                    server.send(conn, reliable, &data);
                    reporter.sent += 1;
                }
                Some(&KIND_EVENT) => {
                    server.send(conn, true, &data);
                    reporter.sent += 1;
                }
                _ => {}
            }
        }
        if load && Instant::now() >= next_tick {
            next_tick += tick;
            for conn in server.connections() {
                for _ in 0..SNAPSHOTS_PER_TICK {
                    server.send(conn, false, &snapshot);
                    reporter.sent += 1;
                }
            }
        }
        server.flush();
        reporter.tick(server.connections().len());
    }
}

/// Sends pings at `hz` for `secs` seconds on one channel and prints the round trips.
pub fn ping<C: PollClient>(server: SocketAddr, reliable: bool, hz: u32, secs: u64, label: &str) {
    // A fresh id per run, so a server still holding an earlier run's connection accepts this one.
    let mut client = C::connect(server, std::process::id());
    let mut poll = Poll::new().expect("poll");
    let _keep = client.socket().map(|s| register(&poll, &[s]));
    let spin = client.socket().is_none();
    let mut events = Events::with_capacity(64);
    let start = Instant::now();
    while !client.connected() {
        let _ = poll.poll(&mut events, Some(Duration::from_millis(1)));
        client.receive();
        client.flush();
        assert!(start.elapsed() < Duration::from_secs(10), "could not connect");
    }
    let mut log = PingLog::new();
    let interval = Duration::from_secs(1) / hz;
    let begin = Instant::now();
    let end = begin + Duration::from_secs(secs);
    let mut next = begin;
    let drain = end + Duration::from_secs(3);
    while Instant::now() < drain {
        let now = Instant::now();
        if now >= next && now < end {
            let m = log.next(PING_BYTES);
            client.send(reliable, &m);
            next += interval;
        }
        for (_, m) in client.receive() {
            log.echo(&m);
        }
        client.flush();
        if spin {
            thread::yield_now();
        } else {
            let wait = next
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(1));
            let _ = poll.poll(&mut events, Some(wait));
        }
    }
    log.report(label);
}

/// Runs `clients` clients on `threads` threads. Each sends its state at 20 Hz and one reliable event per
/// second, and counts the snapshots it receives. Counting starts once every client has connected.
pub fn load<C: PollClient>(server: SocketAddr, clients: usize, threads: usize, secs: u64) {
    let started = Instant::now();
    let cpu0 = process_cpu();
    let measure_from = std::sync::Arc::new(std::sync::OnceLock::<Instant>::new());
    let connected = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for t in 0..threads {
        let ids: Vec<usize> = (0..clients).filter(|i| i % threads == t).collect();
        let measure_from = std::sync::Arc::clone(&measure_from);
        let connected = std::sync::Arc::clone(&connected);
        handles.push(thread::spawn(move || {
            let mut cs: Vec<(C, bool)> = Vec::new();
            for &i in &ids {
                cs.push((C::connect(server, i as u32 + 1), false));
                // Spread the handshakes out a little.
                if cs.len().is_multiple_of(64) {
                    for (c, _) in &mut cs {
                        c.receive();
                        c.flush();
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
            let update = Duration::from_secs(1) / UPDATE_HZ;
            let mut next = Instant::now();
            let mut n: u64 = 0;
            let mut totals = LoadTotals::default();
            let mut end: Option<Instant> = None;
            loop {
                next += update;
                sleep_until(next);
                n += 1;
                let measuring = measure_from.get().is_some_and(|t| Instant::now() >= *t);
                for (k, (c, was)) in cs.iter_mut().enumerate() {
                    for (_, m) in c.receive() {
                        if measuring {
                            match m.first() {
                                Some(&KIND_SNAPSHOT) => totals.snapshots += 1,
                                Some(&KIND_EVENT) => totals.events_back += 1,
                                _ => {}
                            }
                        }
                    }
                    if c.connected() && !*was {
                        *was = true;
                        connected.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if *was {
                        // 20 Hz state, staggered across clients; one event per second.
                        let phase = (n + k as u64) % u64::from(UPDATE_HZ / TICK_HZ);
                        if phase == 0 {
                            c.send(false, &message(KIND_STATE, k as u32, n, STATE_BYTES));
                        }
                        if (n + k as u64).is_multiple_of(u64::from(UPDATE_HZ)) && measuring {
                            c.send(true, &message(KIND_EVENT, k as u32, n, EVENT_BYTES));
                            totals.events_sent += 1;
                        }
                    }
                    c.flush();
                }
                if measuring && end.is_none() {
                    end = Some(*measure_from.get().expect("set") + Duration::from_secs(secs));
                }
                if end.is_some_and(|e| Instant::now() >= e) {
                    totals.connected = cs.iter().filter(|(c, _)| c.connected()).count() as u64;
                    return totals;
                }
                if measure_from.get().is_none()
                    && connected.load(std::sync::atomic::Ordering::Relaxed) == clients
                {
                    // Every client is in; measure after five seconds of warm-up.
                    if measure_from.set(Instant::now() + Duration::from_secs(5)).is_ok() {
                        println!(
                            "connected all={clients} after_s={:.1}",
                            started.elapsed().as_secs_f64()
                        );
                        crate::common::flush();
                    }
                }
                if measure_from.get().is_none() && started.elapsed() > Duration::from_secs(120) {
                    let _ = measure_from.set(Instant::now());
                    eprintln!(
                        "only {} of {clients} connected after 120 s; measuring anyway",
                        connected.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    crate::common::flush();
                }
            }
        }));
    }
    let mut totals = LoadTotals::default();
    for h in handles {
        totals.add(h.join().expect("client thread"));
    }
    totals.report(clients, Duration::from_secs(secs), started, cpu0);
}
