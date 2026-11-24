//! QUIC through `quinn`: unreliable datagrams for state and snapshots, one bidirectional stream per connection
//! for reliable, ordered events (length-prefixed), as architecture.md describes.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use quinn::{
    ClientConfig, Connection, Endpoint, EndpointConfig, RecvStream, SendStream, ServerConfig, TokioRuntime,
};

use crate::common::*;

/// Where the server writes its self-signed certificate for clients on the same machine.
const CERT_PATH: &str = "/tmp/transport-bench-cert.der";

fn transport() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("idle")));
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.datagram_receive_buffer_size(Some(1 << 20));
    t
}

fn endpoint(addr: SocketAddr, server: Option<ServerConfig>) -> Endpoint {
    let socket = udp_socket(addr).expect("bind");
    Endpoint::new(EndpointConfig::default(), server, socket, Arc::new(TokioRuntime)).expect("endpoint")
}

fn client_endpoint(server: SocketAddr) -> Endpoint {
    let der = std::fs::read(CERT_PATH).expect("server certificate; start the server first");
    let mut roots = quinn::rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(der)).expect("root");
    let mut config = ClientConfig::with_root_certificates(Arc::new(roots)).expect("client config");
    config.transport_config(Arc::new(transport()));
    let mut e = endpoint(any_addr(server), None);
    e.set_default_client_config(config);
    e
}

async fn write_frame(send: &mut SendStream, data: &[u8]) -> bool {
    let mut f = Vec::with_capacity(2 + data.len());
    f.extend_from_slice(&(data.len() as u16).to_le_bytes());
    f.extend_from_slice(data);
    send.write_all(&f).await.is_ok()
}

async fn read_frame(recv: &mut RecvStream) -> Option<Vec<u8>> {
    let mut len = [0u8; 2];
    recv.read_exact(&mut len).await.ok()?;
    let mut data = vec![0u8; u16::from_le_bytes(len) as usize];
    recv.read_exact(&mut data).await.ok()?;
    Some(data)
}

#[derive(Default)]
struct Counters {
    received: AtomicU64,
    sent: AtomicU64,
}

pub fn serve(addr: SocketAddr, load: bool, threads: usize) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async move {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("cert");
        std::fs::write(CERT_PATH, cert.cert.der()).expect("write cert");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let mut config =
            ServerConfig::with_single_cert(vec![cert.cert.der().clone()], key).expect("server config");
        config.transport_config(Arc::new(transport()));
        let endpoint = endpoint(addr, Some(config));
        let conns: Arc<Mutex<Vec<Connection>>> = Arc::default();
        let counters: Arc<Counters> = Arc::default();

        // Report every 5 s.
        {
            let conns = Arc::clone(&conns);
            let counters = Arc::clone(&counters);
            tokio::spawn(async move {
                let mut last = Instant::now();
                let mut last_cpu = process_cpu();
                let mut thread_cpu = ThreadCpu::new();
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let secs = last.elapsed().as_secs_f64();
                    let cpu = process_cpu();
                    let n = {
                        let mut c = conns.lock().expect("conns");
                        c.retain(|c| c.close_reason().is_none());
                        c.len()
                    };
                    println!(
                        "report conns={n} cpu_cores={:.3} busiest_thread_cores={:.3} rss_mb={:.1} recv_per_s={:.0} sent_per_s={:.0}",
                        (cpu - last_cpu).as_secs_f64() / secs,
                        thread_cpu.busiest(last.elapsed()),
                        process_rss() as f64 / 1048576.0,
                        counters.received.swap(0, Relaxed) as f64 / secs,
                        counters.sent.swap(0, Relaxed) as f64 / secs,
                    );
                    crate::common::flush();
                    last = Instant::now();
                    last_cpu = cpu;
                }
            });
        }

        // Snapshots at 20 Hz.
        if load {
            let conns = Arc::clone(&conns);
            let counters = Arc::clone(&counters);
            tokio::spawn(async move {
                let snapshot = Bytes::from(message(KIND_SNAPSHOT, 0, 0, snapshot_bytes()));
                let per_tick = snapshots_per_tick();
                let mut tick = tokio::time::interval(Duration::from_secs(1) / TICK_HZ);
                loop {
                    tick.tick().await;
                    let list = conns.lock().expect("conns").clone();
                    for c in &list {
                        for _ in 0..per_tick {
                            if c.send_datagram(snapshot.clone()).is_ok() {
                                counters.sent.fetch_add(1, Relaxed);
                            }
                        }
                    }
                }
            });
        }

        while let Some(incoming) = endpoint.accept().await {
            let conns = Arc::clone(&conns);
            let counters = Arc::clone(&counters);
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                conns.lock().expect("conns").push(conn.clone());
                // Reliable events and reliable pings: echoed on the stream the client opened.
                {
                    let conn = conn.clone();
                    let counters = Arc::clone(&counters);
                    tokio::spawn(async move {
                        let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                            return;
                        };
                        while let Some(m) = read_frame(&mut recv).await {
                            counters.received.fetch_add(1, Relaxed);
                            if write_frame(&mut send, &m).await {
                                counters.sent.fetch_add(1, Relaxed);
                            }
                        }
                    });
                }
                while let Ok(d) = conn.read_datagram().await {
                    counters.received.fetch_add(1, Relaxed);
                    if d.first() == Some(&KIND_PING) && conn.send_datagram(d).is_ok() {
                        counters.sent.fetch_add(1, Relaxed);
                    }
                }
            });
        }
    });
}

/// Connects to a server and prints the largest datagram quinn will send, at once and after path MTU discovery
/// has had time to probe.
pub fn datagram_size(server: SocketAddr) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async move {
        let endpoint = client_endpoint(server);
        let conn = endpoint
            .connect(server, "localhost")
            .expect("connect")
            .await
            .expect("handshake");
        let started = Instant::now();
        for wait in [0u64, 1, 3, 6] {
            tokio::time::sleep_until((started + Duration::from_secs(wait)).into()).await;
            println!(
                "datagram after_s={wait} max_datagram_bytes={}",
                conn.max_datagram_size().unwrap_or(0)
            );
            flush();
        }
        conn.close(0u32.into(), b"done");
        endpoint.wait_idle().await;
    });
}

pub fn ping(server: SocketAddr, reliable: bool, hz: u32, secs: u64, label: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async move {
        let endpoint = client_endpoint(server);
        let conn = endpoint
            .connect(server, "localhost")
            .expect("connect")
            .await
            .expect("handshake");
        let (mut send, mut recv) = conn.open_bi().await.expect("stream");
        // The server only sees the stream once something is written on it.
        let log = Arc::new(Mutex::new(PingLog::new()));
        {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                while let Some(m) = read_frame(&mut recv).await {
                    log.lock().expect("log").echo(&m);
                }
            });
        }
        {
            let log = Arc::clone(&log);
            let conn = conn.clone();
            tokio::spawn(async move {
                while let Ok(d) = conn.read_datagram().await {
                    log.lock().expect("log").echo(&d);
                }
            });
        }
        let mut interval = tokio::time::interval(Duration::from_secs(1) / hz);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            interval.tick().await;
            let m = log.lock().expect("log").next(PING_BYTES);
            if reliable {
                write_frame(&mut send, &m).await;
            } else {
                let _ = conn.send_datagram(Bytes::from(m));
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        log.lock().expect("log").report(label);
    });
}

pub fn load(server: SocketAddr, clients: usize, threads: usize, secs: u64) {
    let started = Instant::now();
    let cpu0 = process_cpu();
    let mut thread_cpu = ThreadCpu::new();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async move {
        // 64 connections share one client socket, like many players behind few addresses would not; it keeps
        // the load generator's own cost down.
        let endpoints: Vec<Endpoint> = (0..clients.div_ceil(64))
            .map(|_| client_endpoint(server))
            .collect();
        let measuring = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicUsize::new(0));
        let snapshots = Arc::new(AtomicU64::new(0));
        let events_back = Arc::new(AtomicU64::new(0));
        let events_sent = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for k in 0..clients {
            let endpoint = endpoints[k / 64].clone();
            let (measuring, connected, snapshots, events_back, events_sent) = (
                Arc::clone(&measuring),
                Arc::clone(&connected),
                Arc::clone(&snapshots),
                Arc::clone(&events_back),
                Arc::clone(&events_sent),
            );
            tasks.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(k as u64)).await;
                let Ok(connecting) = endpoint.connect(server, "localhost") else {
                    return;
                };
                let Ok(conn) = connecting.await else { return };
                let Ok((mut send, mut recv)) = conn.open_bi().await else {
                    return;
                };
                connected.fetch_add(1, Relaxed);
                {
                    let measuring = Arc::clone(&measuring);
                    tokio::spawn(async move {
                        while read_frame(&mut recv).await.is_some() {
                            if measuring.load(Relaxed) {
                                events_back.fetch_add(1, Relaxed);
                            }
                        }
                    });
                }
                {
                    let conn = conn.clone();
                    let measuring = Arc::clone(&measuring);
                    tokio::spawn(async move {
                        while let Ok(d) = conn.read_datagram().await {
                            if measuring.load(Relaxed) && d.first() == Some(&KIND_SNAPSHOT) {
                                snapshots.fetch_add(1, Relaxed);
                            }
                        }
                    });
                }
                let mut tick = tokio::time::interval(Duration::from_secs(1) / TICK_HZ);
                let mut n: u64 = 0;
                loop {
                    tick.tick().await;
                    n += 1;
                    let _ = conn.send_datagram(Bytes::from(message(KIND_STATE, k as u32, n, STATE_BYTES)));
                    if n.is_multiple_of(u64::from(TICK_HZ)) && measuring.load(Relaxed) {
                        write_frame(&mut send, &message(KIND_EVENT, k as u32, n, EVENT_BYTES)).await;
                        events_sent.fetch_add(1, Relaxed);
                    }
                }
            }));
        }
        while connected.load(Relaxed) < clients && started.elapsed() < Duration::from_secs(120) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if connected.load(Relaxed) < clients {
            eprintln!(
                "only {} of {clients} connected after 120 s; measuring anyway",
                connected.load(Relaxed)
            );
            crate::common::flush();
        }
        println!(
            "connected all={} after_s={:.1}",
            connected.load(Relaxed),
            started.elapsed().as_secs_f64()
        );
        crate::common::flush();
        tokio::time::sleep(Duration::from_secs(5)).await;
        measuring.store(true, Relaxed);
        tokio::time::sleep(Duration::from_secs(secs)).await;
        measuring.store(false, Relaxed);
        let open = endpoints.iter().map(|e| e.open_connections()).sum::<usize>();
        let totals = LoadTotals {
            connected: open as u64,
            snapshots: snapshots.load(Relaxed),
            events_back: events_back.load(Relaxed),
            events_sent: events_sent.load(Relaxed),
            thread_cpu: Duration::ZERO,
        };
        totals.report(clients, Duration::from_secs(secs), started, cpu0, &mut thread_cpu);
    });
}
