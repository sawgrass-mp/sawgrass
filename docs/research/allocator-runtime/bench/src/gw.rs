//! The gateway's network path on four runtimes.
//!
//! Every variant runs the same server: QUIC with `sawgrass-gateway`'s transport settings, a handshake on the one
//! bidirectional stream the client opens (hello: 4-byte magic and `u32` version; welcome: `u32` version), then
//! reliable events framed with a `u32` length and echoed back, state datagrams counted, and one snapshot datagram
//! per client per tick, built fresh for each client as a real gateway would.
//!
//! - `tokio-mt`: one Tokio multi-threaded runtime with `threads` workers, as `sawgrass-gateway` uses now, and
//!   `endpoints` quinn endpoints on the same port (`SO_REUSEPORT`; one endpoint is what the gateway does now).
//! - `tokio-tpc`: thread per core: `threads` Tokio current-thread runtimes on their own threads, each with its own
//!   endpoint on the shared port.
//! - `smol`: quinn's smol runtime on smol's global executor (`SMOL_THREADS` threads) and one endpoint.
//! - `compio` (in `cgw.rs`): thread per core on io_uring, each thread with its own `compio-quic` endpoint.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use quinn::{Connection, Endpoint, EndpointConfig, RecvStream, SendStream, ServerConfig, VarInt};

use crate::common::*;

/// Where the server writes its self-signed certificate for the load generator on the same machine.
pub const CERT_PATH: &str = "/tmp/runtime-bench-cert.der";
/// First bytes of a client hello, as in `sawgrass-gateway`.
pub const MAGIC: [u8; 4] = *b"SWGR";
/// Protocol version both sides speak.
pub const VERSION: u32 = 1;
/// Flow-control window per stream and per connection, as in `sawgrass-gateway`.
const RECEIVE_WINDOW: u32 = 256 * 1024;

/// `sawgrass-gateway`'s transport settings: 30 s idle timeout, keep-alive every 5 s, exactly one bidirectional
/// stream from the client and no unidirectional ones, 256 KiB windows, and a datagram receive buffer of 64 KiB on
/// the gateway and 1 MiB on the client.
pub fn transport(gateway: bool) -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    let (peer_bidi, datagram_buffer) = if gateway { (1u32, 64 * 1024) } else { (0, 1 << 20) };
    config
        .max_idle_timeout(Some(quinn::IdleTimeout::from(VarInt::from_u32(30_000))))
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .max_concurrent_bidi_streams(peer_bidi.into())
        .max_concurrent_uni_streams(0u32.into())
        .stream_receive_window(RECEIVE_WINDOW.into())
        .receive_window(RECEIVE_WINDOW.into())
        .datagram_receive_buffer_size(Some(datagram_buffer));
    Arc::new(config)
}

/// A server configuration with a fresh self-signed certificate, which is written to [`CERT_PATH`].
pub fn server_config() -> ServerConfig {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("cert");
    std::fs::write(CERT_PATH, cert.cert.der()).expect("write cert");
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let chain: Vec<CertificateDer<'static>> = vec![cert.cert.der().clone()];
    let mut config = ServerConfig::with_single_cert(chain, key).expect("server config");
    config.transport_config(transport(true));
    config
}

/// Counters shared by every endpoint and thread of one server process.
#[derive(Debug, Default)]
pub struct Counters {
    pub received: AtomicU64,
    pub sent: AtomicU64,
    pub conns: AtomicUsize,
}

impl Counters {
    /// Prints a report line every 5 s, forever, on a plain thread so that no runtime's scheduling delays it.
    pub fn report_forever(self: &Arc<Self>) {
        let counters = Arc::clone(self);
        std::thread::spawn(move || {
            let mut reporter = Reporter::new();
            loop {
                std::thread::sleep(Duration::from_secs(5));
                reporter.report(
                    counters.conns.load(Relaxed),
                    counters.received.swap(0, Relaxed),
                    counters.sent.swap(0, Relaxed),
                );
            }
        });
    }
}

/// The deadlines of a 20 Hz tick that does not drift: tick `n` is due at `start + n / 20 s`; a late tick skips
/// to the next deadline instead of bursting.
pub struct TickClock {
    start: Instant,
    n: u32,
}

impl TickClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            n: 0,
        }
    }

    /// Time to wait before the next tick.
    pub fn wait(&mut self) -> Duration {
        let interval = Duration::from_secs(1) / TICK_HZ;
        self.n += 1;
        let now = Instant::now();
        let mut due = self.start + interval * self.n;
        while due + interval < now {
            self.n += 1;
            due = self.start + interval * self.n;
        }
        due.saturating_duration_since(now)
    }
}

/// What a quinn runtime must provide to run the server.
pub trait Exec: Clone + Send + Sync + 'static {
    fn spawn(&self, f: impl Future<Output = ()> + Send + 'static);
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
    fn quinn(&self) -> Arc<dyn quinn::Runtime>;
}

#[derive(Clone)]
struct Tokio;

impl Exec for Tokio {
    fn spawn(&self, f: impl Future<Output = ()> + Send + 'static) {
        tokio::spawn(f);
    }
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(d))
    }
    fn quinn(&self) -> Arc<dyn quinn::Runtime> {
        Arc::new(quinn::TokioRuntime)
    }
}

#[derive(Clone)]
struct Smol;

impl Exec for Smol {
    fn spawn(&self, f: impl Future<Output = ()> + Send + 'static) {
        smol::spawn(f).detach();
    }
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            smol::Timer::after(d).await;
        })
    }
    fn quinn(&self) -> Arc<dyn quinn::Runtime> {
        Arc::new(quinn::SmolRuntime)
    }
}

async fn read_frame(recv: &mut RecvStream) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.ok()?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_EVENT_LEN {
        return None;
    }
    let mut data = vec![0u8; len];
    recv.read_exact(&mut data).await.ok()?;
    Some(data)
}

/// Reads the client's hello and answers it; `None` if it is not a valid hello.
async fn handshake(send: &mut SendStream, recv: &mut RecvStream) -> Option<()> {
    let mut hello = [0u8; 8];
    recv.read_exact(&mut hello).await.ok()?;
    if hello[..4] != MAGIC || u32::from_le_bytes(hello[4..].try_into().ok()?) != VERSION {
        return None;
    }
    send.write_all(&VERSION.to_le_bytes()).await.ok()
}

fn endpoint<E: Exec>(ex: &E, addr: SocketAddr, reuse_port: bool, config: ServerConfig) -> Endpoint {
    let socket = udp_socket(addr, reuse_port).expect("bind");
    Endpoint::new(EndpointConfig::default(), Some(config), socket, ex.quinn()).expect("endpoint")
}

/// Accepts connections on `endpoint` and serves them, with a snapshot task for the endpoint's own clients.
async fn serve<E: Exec>(ex: E, endpoint: Endpoint, counters: Arc<Counters>) {
    let conns: Arc<Mutex<Vec<Connection>>> = Arc::default();
    {
        let (ex2, conns, counters) = (ex.clone(), Arc::clone(&conns), Arc::clone(&counters));
        ex.spawn(async move {
            let mut clock = TickClock::new();
            let mut tick = 0u64;
            loop {
                ex2.sleep(clock.wait()).await;
                tick += 1;
                let list = conns.lock().expect("conns").clone();
                for (i, c) in list.iter().enumerate() {
                    // A fresh buffer per client and tick, as a gateway encoding each player's snapshot would make.
                    let snapshot = Bytes::from(message(KIND_SNAPSHOT, i as u32, tick, SNAPSHOT_BYTES));
                    if c.send_datagram(snapshot).is_ok() {
                        counters.sent.fetch_add(1, Relaxed);
                    }
                }
            }
        });
    }
    while let Some(incoming) = endpoint.accept().await {
        let (ex2, conns, counters) = (ex.clone(), Arc::clone(&conns), Arc::clone(&counters));
        ex.spawn(async move {
            let Ok(conn) = incoming.await else { return };
            let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                return;
            };
            if handshake(&mut send, &mut recv).await.is_none() {
                conn.close(VarInt::from_u32(1), b"bad hello");
                return;
            }
            conns.lock().expect("conns").push(conn.clone());
            counters.conns.fetch_add(1, Relaxed);
            {
                let counters = Arc::clone(&counters);
                ex2.spawn(async move {
                    while let Some(m) = read_frame(&mut recv).await {
                        counters.received.fetch_add(1, Relaxed);
                        if send.write_all(&frame(&m)).await.is_ok() {
                            counters.sent.fetch_add(1, Relaxed);
                        }
                    }
                });
            }
            while conn.read_datagram().await.is_ok() {
                counters.received.fetch_add(1, Relaxed);
            }
            conns
                .lock()
                .expect("conns")
                .retain(|c| c.stable_id() != conn.stable_id());
            counters.conns.fetch_sub(1, Relaxed);
        });
    }
}

/// Runs the server until killed.
pub fn server(runtime: &str, addr: SocketAddr, threads: usize, endpoints: usize) {
    let config = server_config();
    let counters: Arc<Counters> = Arc::default();
    println!(
        "server runtime={runtime} allocator={} threads={threads} endpoints={endpoints}",
        allocator()
    );
    flush();
    counters.report_forever();
    match runtime {
        "tokio-mt" => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(threads)
                .enable_all()
                .build()
                .expect("rt");
            rt.block_on(async move {
                for _ in 1..endpoints {
                    let e = endpoint(&Tokio, addr, true, config.clone());
                    tokio::spawn(serve(Tokio, e, Arc::clone(&counters)));
                }
                serve(Tokio, endpoint(&Tokio, addr, endpoints > 1, config), counters).await;
            });
        }
        "tokio-tpc" => {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let (config, counters) = (config.clone(), Arc::clone(&counters));
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("rt");
                        rt.block_on(async move {
                            serve(Tokio, endpoint(&Tokio, addr, true, config), counters).await;
                        });
                    })
                })
                .collect();
            for h in handles {
                let _ = h.join();
            }
        }
        "smol" => {
            // The accept loop runs on the global executor too, so all work is on its `SMOL_THREADS` threads;
            // this thread only waits.
            let e = endpoint(&Smol, addr, false, config);
            Smol.spawn(serve(Smol, e, counters));
            smol::block_on(smol::future::pending::<()>());
        }
        "compio" => crate::cgw::server(addr, threads, config, counters),
        other => panic!("unknown runtime {other}"),
    }
}
