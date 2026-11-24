//! The Sawgrass world server.
//!
//! Runs the tick loop and prints tick metrics every five seconds. With `--listen` it also accepts players
//! through an in-process gateway (see [`sawgrass_server::net`]).
//!
//! ```text
//! sawgrass-server [--tick-hz N] [--ticks N] [--listen ADDR --cert FILE --key FILE] [--net-threads N]
//!                 [--stats-to-clients]
//! ```
//!
//! - `--tick-hz N`: ticks per second, 1 to 1000 (default 20).
//! - `--ticks N`: stop after N ticks and print the final metrics (default: run until killed).
//! - `--listen ADDR`: accept players on this UDP address, for example `0.0.0.0:4433`.
//! - `--cert FILE`, `--key FILE`: the gateway's certificate (DER) and PKCS#8 private key (DER).
//!   `sawgrass-bots gen-cert` writes a self-signed pair for tests.
//! - `--net-threads N`: threads for the gateway runtime (default: all cores but the tick thread's).
//! - `--stats-to-clients`: answer clients' stats requests, for load tests. Do not use it on a public server.

use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use sawgrass_protocol::game::ServerStats;
use sawgrass_server::net::{self, NetId};
use sawgrass_server::{TickConfig, TickLoop, cpu};

const REPORT_EVERY_SECS: u64 = 5;

struct Args {
    tick_hz: u32,
    ticks: Option<u64>,
    listen: Option<SocketAddr>,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    net_threads: Option<usize>,
    stats_to_clients: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        tick_hz: TickConfig::DEFAULT_RATE_HZ,
        ticks: None,
        listen: None,
        cert: None,
        key: None,
        net_threads: None,
        stats_to_clients: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = |name: &str| iter.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "--tick-hz" => {
                args.tick_hz = value("--tick-hz")?
                    .parse()
                    .map_err(|e| format!("--tick-hz: {e}"))?;
            }
            "--ticks" => {
                args.ticks = Some(value("--ticks")?.parse().map_err(|e| format!("--ticks: {e}"))?);
            }
            "--listen" => {
                args.listen = Some(value("--listen")?.parse().map_err(|e| format!("--listen: {e}"))?);
            }
            "--cert" => args.cert = Some(value("--cert")?.into()),
            "--key" => args.key = Some(value("--key")?.into()),
            "--net-threads" => {
                let n: usize = value("--net-threads")?
                    .parse()
                    .map_err(|e| format!("--net-threads: {e}"))?;
                args.net_threads = Some(n.max(1));
            }
            "--stats-to-clients" => args.stats_to_clients = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if args.listen.is_some() && (args.cert.is_none() || args.key.is_none()) {
        return Err("--listen needs --cert and --key".to_owned());
    }
    Ok(args)
}

/// Turns CPU time spent over `elapsed` into thousandths of a core.
fn millicores(spent: Option<Duration>, elapsed: Duration) -> u32 {
    spent.map_or(u32::MAX, |spent| {
        (spent.as_secs_f64() / elapsed.as_secs_f64().max(1e-9) * 1000.0) as u32
    })
}

fn main() -> ExitCode {
    let (args, config) = match parse_args().and_then(|args| {
        let config = TickConfig::new(args.tick_hz).map_err(|e| e.to_string())?;
        Ok((args, config))
    }) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("sawgrass-server: {err}");
            eprintln!(
                "usage: sawgrass-server [--tick-hz N] [--ticks N] [--listen ADDR --cert FILE --key FILE] \
                 [--net-threads N] [--stats-to-clients]"
            );
            return ExitCode::from(2);
        }
    };

    println!(
        "sawgrass-server {} (protocol {}), {} Hz",
        env!("CARGO_PKG_VERSION"),
        sawgrass_protocol::PROTOCOL_VERSION,
        config.rate_hz()
    );

    let mut tick_loop = TickLoop::new(config);

    // The gateway runtime lives as long as main; the tick loop runs on this thread.
    let mut runtime = None;
    let mut network = None;
    if let (Some(listen), Some(cert), Some(key)) = (args.listen, &args.cert, &args.key) {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let threads = args.net_threads.unwrap_or(cores.saturating_sub(1).max(1));
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("gateway")
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("sawgrass-server: runtime: {e}");
                return ExitCode::FAILURE;
            }
        };
        let players = {
            let _guard = rt.enter();
            match net::bind_gateway(listen, cert, key) {
                Ok(gateway) => {
                    println!("listening on {listen} with {threads} gateway threads");
                    net::listen(gateway, args.stats_to_clients)
                }
                Err(e) => {
                    eprintln!("sawgrass-server: gateway: {e}");
                    return ExitCode::FAILURE;
                }
            }
        };
        network = Some((players.traffic(), players.stats_slot()));
        let mut players = players;
        tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
        runtime = Some(rt);
    }

    let report_every = u64::from(config.rate_hz()) * REPORT_EVERY_SECS;
    let main_tid = std::process::id();
    let mut last = (
        Instant::now(),
        cpu::process_time(),
        cpu::thread_time(main_tid),
        0u64,
        0u64,
    );
    tick_loop.run(|tick_loop| {
        let tick = tick_loop.tick();
        if args.ticks.is_some_and(|limit| tick >= limit) {
            return ControlFlow::Break(());
        }
        if tick > 0 && tick % report_every == 0 {
            let stats = tick_loop.metrics().stats();
            let now = (Instant::now(), cpu::process_time(), cpu::thread_time(main_tid));
            let elapsed = now.0 - last.0;
            let process = millicores(now.1.zip(last.1).map(|(a, b)| a.saturating_sub(b)), elapsed);
            let tick_thread = millicores(now.2.zip(last.2).map(|(a, b)| a.saturating_sub(b)), elapsed);
            let players = tick_loop.world().query::<&NetId>().iter().count();
            let mut line =
                format!("tick {stats}, players {players}, cpu {process} mcores (tick thread {tick_thread})");
            if let Some((traffic, slot)) = &network {
                let sent = traffic.sent_bytes.load(Ordering::Relaxed);
                let received = traffic.received_bytes.load(Ordering::Relaxed);
                let per_sec = |bytes: u64| (bytes as f64 / elapsed.as_secs_f64()) as u64;
                let report = ServerStats {
                    tick_p50_us: stats.p50.as_micros() as u32,
                    tick_p99_us: stats.p99.as_micros() as u32,
                    tick_max_us: stats.max.as_micros() as u32,
                    tick_interval_us: stats.interval.as_micros() as u32,
                    overruns: stats.overruns as u32,
                    players: players as u32,
                    cpu_millicores: process,
                    tick_thread_millicores: tick_thread,
                    sent_bytes_per_sec: per_sec(sent - last.3),
                    received_bytes_per_sec: per_sec(received - last.4),
                };
                line += &format!(
                    ", sent {} kB/s, received {} kB/s, send errors {}",
                    report.sent_bytes_per_sec / 1000,
                    report.received_bytes_per_sec / 1000,
                    traffic.send_errors.load(Ordering::Relaxed)
                );
                if let Ok(mut slot) = slot.lock() {
                    *slot = Some(report);
                }
                last.3 = sent;
                last.4 = received;
            }
            println!("{line}");
            last = (now.0, now.1, now.2, last.3, last.4);
        }
        ControlFlow::Continue(())
    });
    println!("tick {}", tick_loop.metrics().stats());
    drop(runtime);
    ExitCode::SUCCESS
}
