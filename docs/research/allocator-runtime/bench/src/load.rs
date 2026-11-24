//! The load generator: `clients` players over quinn on a Tokio runtime, the same for every server variant.
//!
//! Each player connects, sends the hello, then sends a 64-byte state datagram 20 times a second and a 100-byte
//! reliable event once a second, which the server echoes; the round trip of each event is recorded. After every
//! player is connected and 5 s of warm-up, `secs` seconds are measured.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::rustls::pki_types::CertificateDer;
use quinn::{ClientConfig, Endpoint, EndpointConfig, RecvStream, TokioRuntime};

use crate::common::*;
use crate::gw::{CERT_PATH, MAGIC, TickClock, VERSION, transport};

/// Players that share one client socket. Several sockets let `SO_REUSEPORT` spread players over server endpoints
/// by address.
const PER_SOCKET: usize = 16;

fn client_endpoint(server: SocketAddr) -> Endpoint {
    let der = std::fs::read(CERT_PATH).expect("server certificate; start the server first");
    let mut roots = quinn::rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(der)).expect("root");
    let mut config = ClientConfig::with_root_certificates(Arc::new(roots)).expect("client config");
    config.transport_config(transport(false));
    let socket = udp_socket(any_addr(server), false).expect("bind");
    let mut e =
        Endpoint::new(EndpointConfig::default(), None, socket, Arc::new(TokioRuntime)).expect("endpoint");
    e.set_default_client_config(config);
    e
}

async fn read_frame(recv: &mut RecvStream) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.ok()?;
    let mut data = vec![0u8; u32::from_le_bytes(len) as usize];
    recv.read_exact(&mut data).await.ok()?;
    Some(data)
}

#[derive(Default)]
struct Shared {
    measuring: AtomicBool,
    /// Echoes are recorded until 2 s after the window, so events sent at its end still count.
    collecting: AtomicBool,
    connected: AtomicUsize,
    snapshots: AtomicU64,
    events_sent: AtomicU64,
    rtt_ns: Mutex<Vec<u64>>,
}

pub fn load(server: SocketAddr, clients: usize, threads: usize, secs: u64) {
    let started = Instant::now();
    let mut thread_cpu = ThreadCpu::new();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async move {
        let endpoints: Vec<Endpoint> = (0..clients.div_ceil(PER_SOCKET))
            .map(|_| client_endpoint(server))
            .collect();
        let shared: Arc<Shared> = Arc::default();
        for k in 0..clients {
            let endpoint = endpoints[k / PER_SOCKET].clone();
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                // Spread connection attempts so the handshakes do not all land at once.
                tokio::time::sleep(Duration::from_millis(k as u64 * 3)).await;
                let Ok(connecting) = endpoint.connect(server, "localhost") else {
                    return;
                };
                let Ok(conn) = connecting.await else { return };
                let Ok((mut send, mut recv)) = conn.open_bi().await else {
                    return;
                };
                let mut hello = MAGIC.to_vec();
                hello.extend_from_slice(&VERSION.to_le_bytes());
                if send.write_all(&hello).await.is_err() {
                    return;
                }
                let mut welcome = [0u8; 4];
                if recv.read_exact(&mut welcome).await.is_err() {
                    return;
                }
                shared.connected.fetch_add(1, Relaxed);
                {
                    let shared = Arc::clone(&shared);
                    tokio::spawn(async move {
                        while let Some(m) = read_frame(&mut recv).await {
                            if shared.collecting.load(Relaxed)
                                && let Some(at) = sent_at(&m)
                            {
                                let rtt = now_ns().saturating_sub(at);
                                shared.rtt_ns.lock().expect("rtt").push(rtt);
                            }
                        }
                    });
                }
                {
                    let conn = conn.clone();
                    let shared = Arc::clone(&shared);
                    tokio::spawn(async move {
                        while let Ok(d) = conn.read_datagram().await {
                            if shared.measuring.load(Relaxed) && d.first() == Some(&KIND_SNAPSHOT) {
                                shared.snapshots.fetch_add(1, Relaxed);
                            }
                        }
                    });
                }
                let mut clock = TickClock::new();
                let mut n: u64 = 0;
                // Players send their event at different points of the second.
                let event_phase = (k % TICK_HZ as usize) as u64;
                loop {
                    tokio::time::sleep(clock.wait()).await;
                    n += 1;
                    let state = Bytes::from(message(KIND_STATE, k as u32, n, STATE_BYTES));
                    let _ = conn.send_datagram(state);
                    if n % u64::from(TICK_HZ) == event_phase && shared.measuring.load(Relaxed) {
                        let event = frame(&message(KIND_EVENT, k as u32, n, EVENT_BYTES));
                        if send.write_all(&event).await.is_err() {
                            return;
                        }
                        shared.events_sent.fetch_add(1, Relaxed);
                    }
                }
            });
        }
        let connect_deadline = Duration::from_secs(60) + Duration::from_millis(clients as u64 * 3);
        while shared.connected.load(Relaxed) < clients && started.elapsed() < connect_deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!(
            "connected all={} after_s={:.1}",
            shared.connected.load(Relaxed),
            started.elapsed().as_secs_f64()
        );
        flush();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let window_start = Instant::now();
        let window_cpu = process_cpu();
        let _ = thread_cpu.busiest(started.elapsed());
        shared.collecting.store(true, Relaxed);
        shared.measuring.store(true, Relaxed);
        tokio::time::sleep(Duration::from_secs(secs)).await;
        shared.measuring.store(false, Relaxed);
        let window = window_start.elapsed();
        let gen_cpu = (process_cpu() - window_cpu).as_secs_f64() / window.as_secs_f64();
        let gen_busiest = thread_cpu.busiest(window);
        tokio::time::sleep(Duration::from_secs(2)).await;
        shared.collecting.store(false, Relaxed);
        let mut rtt = std::mem::take(&mut *shared.rtt_ns.lock().expect("rtt"));
        rtt.sort_unstable();
        let connected = shared.connected.load(Relaxed);
        let expected_snapshots = connected as f64 * f64::from(TICK_HZ) * window.as_secs_f64();
        let ms = |q: f64| percentile(&rtt, q) as f64 / 1e6;
        println!(
            "load clients={clients} connected={connected} snapshot_delivery_pct={:.2} events_sent={} events_back={} \
             event_p50_ms={:.3} event_p99_ms={:.3} event_p999_ms={:.3} event_max_ms={:.3} gen_cpu_cores={gen_cpu:.2} \
             gen_busiest_thread_cores={gen_busiest:.2}",
            100.0 * shared.snapshots.load(Relaxed) as f64 / expected_snapshots.max(1.0),
            shared.events_sent.load(Relaxed),
            rtt.len(),
            ms(0.5),
            ms(0.99),
            ms(0.999),
            ms(1.0),
        );
        flush();
    });
}
