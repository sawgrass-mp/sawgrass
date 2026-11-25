//! The Sawgrass world server.
//!
//! Runs the tick loop and prints tick metrics every five seconds. With `--listen` it also accepts players
//! through an in-process gateway (see [`sawgrass_server::net`]).
//!
//! ```text
//! sawgrass-server [--tick-hz N] [--ticks N] [--listen ADDR --cert FILE --key FILE] [--net-threads N]
//!                 [--max-players N] [--max-per-address N] [--max-handshakes N] [--tick-threads N]
//!                 [--stats-to-clients] [--resources DIR [--script-budget-ms N]]
//!                 [--vehicles N] [--vehicle-area M]
//! ```
//!
//! - `--tick-hz N`: ticks per second, 1 to 1000 (default 20).
//! - `--ticks N`: stop after N ticks and print the final metrics (default: run until killed).
//! - `--listen ADDR`: accept players on this UDP address, for example `0.0.0.0:4433`.
//! - `--cert FILE`, `--key FILE`: the gateway's certificate (DER) and PKCS#8 private key (DER).
//!   `sawgrass-bots gen-cert` writes a self-signed pair for tests.
//! - `--net-threads N`: threads for the gateway runtime (default: a quarter of the cores, at least 1).
//! - `--max-players N`: refuse connections beyond N (default 4096).
//! - `--max-per-address N`: refuse connections beyond N from one IP address (default 16); load tests that run
//!   every bot from one machine raise it.
//! - `--max-handshakes N`: refuse new handshakes while N are in progress (default 1024).
//! - `--tick-threads N`: threads that build players' snapshots in parallel during a tick (default: the cores the
//!   gateway does not get; 1 builds them on the tick thread).
//!
//! The two pools share the cores the process may run on and do not overlap (task SRV-11): the tick thread waits
//! while the snapshot pool works, so gateway and tick threads together are one per core. Giving only one of the
//! options gives the other the rest of the cores.
//! - `--stats-to-clients`: answer clients' stats requests, for load tests. Do not use it on a public server.
//! - `--vehicles N`: spawn N parked vehicles at start, spread over a square centred on the origin, for load tests
//!   (default 0). Their model is `AssetId(1)`, a placeholder.
//! - `--vehicle-area M`: the edge of that square, in metres (default 1500, the bots' crowded area).
//! - `--resources DIR`: run every resource in `DIR` (a folder with a `resource.toml`) in the tick. A resource that
//!   fails to load is reported and left out.
//! - `--script-budget-ms N`: the time all resources together may take in one tick (default 8).

use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
use sawgrass_protocol::game::{ClientState, ServerStats};
use sawgrass_protocol::vehicle::VehicleBlock;
use sawgrass_protocol::{Position, Rotation, Velocity};
use sawgrass_server::limits::Limits;
use sawgrass_server::net::{self, NetId};
use sawgrass_server::scripts::{self, Scripts};
use sawgrass_server::{TickConfig, TickLoop, cpu};

/// jemalloc for every allocation in the process (SRV-5; BOT-3 halved the crowd's tick p99 with it).
#[cfg(unix)]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const REPORT_EVERY_SECS: u64 = 5;

struct Args {
    tick_hz: u32,
    ticks: Option<u64>,
    listen: Option<SocketAddr>,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    net_threads: Option<usize>,
    max_players: usize,
    max_per_address: usize,
    max_handshakes: usize,
    tick_threads: Option<usize>,
    stats_to_clients: bool,
    vehicles: u32,
    vehicle_area: f32,
    resources: Option<PathBuf>,
    script_budget: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        tick_hz: TickConfig::DEFAULT_RATE_HZ,
        ticks: None,
        listen: None,
        cert: None,
        key: None,
        net_threads: None,
        max_players: net::ListenConfig::default().max_players,
        max_per_address: net::ListenConfig::default().limits.max_per_address,
        max_handshakes: net::ListenConfig::default().limits.max_handshakes,
        tick_threads: None,
        stats_to_clients: false,
        vehicles: 0,
        vehicle_area: 1500.0,
        resources: None,
        script_budget: scripts::DEFAULT_BUDGET,
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
            "--max-players" => {
                args.max_players = value("--max-players")?
                    .parse()
                    .map_err(|e| format!("--max-players: {e}"))?;
            }
            "--max-per-address" => {
                args.max_per_address = value("--max-per-address")?
                    .parse()
                    .map_err(|e| format!("--max-per-address: {e}"))?;
            }
            "--max-handshakes" => {
                args.max_handshakes = value("--max-handshakes")?
                    .parse()
                    .map_err(|e| format!("--max-handshakes: {e}"))?;
            }
            "--tick-threads" => {
                let n: usize = value("--tick-threads")?
                    .parse()
                    .map_err(|e| format!("--tick-threads: {e}"))?;
                args.tick_threads = Some(n.max(1));
            }
            "--stats-to-clients" => args.stats_to_clients = true,
            "--vehicles" => {
                args.vehicles = value("--vehicles")?
                    .parse()
                    .map_err(|e| format!("--vehicles: {e}"))?;
            }
            "--vehicle-area" => {
                args.vehicle_area = value("--vehicle-area")?
                    .parse()
                    .map_err(|e| format!("--vehicle-area: {e}"))?;
            }
            "--resources" => args.resources = Some(value("--resources")?.into()),
            "--script-budget-ms" => {
                let ms: u64 = value("--script-budget-ms")?
                    .parse()
                    .map_err(|e| format!("--script-budget-ms: {e}"))?;
                args.script_budget = Duration::from_millis(ms);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if args.listen.is_some() && (args.cert.is_none() || args.key.is_none()) {
        return Err("--listen needs --cert and --key".to_owned());
    }
    Ok(args)
}

/// Spawns `count` parked vehicles at even spacing over a square of edge `area` centred on the origin, and returns
/// how many the server took.
fn spawn_vehicles(players: &mut net::Players, count: u32, area: f32) -> u32 {
    let side = f64::from(count).sqrt().ceil().max(1.0) as u32;
    let step = area / side as f32;
    let mut spawned = 0;
    for i in 0..count {
        let (x, y) = (
            -area / 2.0 + step * ((i % side) as f32 + 0.5),
            -area / 2.0 + step * ((i / side) as f32 + 0.5),
        );
        let (Ok(position), Ok(rotation)) = (
            Position::from_metres([x, y, 30.0]),
            Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]),
        ) else {
            continue;
        };
        let state = ClientState {
            position,
            velocity: Velocity { cm_per_s: [0; 3] },
            rotation,
        };
        let info = Info::with_fields(EntityKind::Vehicle, AssetId(1), &VehicleBlock::INTACT.to_fields());
        if let Ok(info) = info
            && players.spawn(info, state).is_some()
        {
            spawned += 1;
        }
    }
    spawned
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
                 [--net-threads N] [--max-players N] [--max-per-address N] [--max-handshakes N] \
                 [--tick-threads N] \
                 [--stats-to-clients] [--resources DIR [--script-budget-ms N]]"
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
    let mut shutdown = None;
    let mut roster = None;
    if let (Some(listen), Some(cert), Some(key)) = (args.listen, &args.cert, &args.key) {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let (threads, tick_threads) = split_cores(cores, args.net_threads, args.tick_threads);
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
                    println!(
                        "listening on {listen} with {threads} gateway threads and {tick_threads} tick threads"
                    );
                    net::listen(
                        gateway,
                        net::ListenConfig {
                            stats_to_clients: args.stats_to_clients,
                            max_players: args.max_players,
                            tick_threads,
                            limits: Limits {
                                max_per_address: args.max_per_address,
                                max_handshakes: args.max_handshakes,
                                ..Limits::default()
                            },
                            ..net::ListenConfig::default()
                        },
                    )
                }
                Err(e) => {
                    eprintln!("sawgrass-server: gateway: {e}");
                    return ExitCode::FAILURE;
                }
            }
        };
        network = Some((players.traffic(), players.stats_slot()));
        shutdown = Some(players.shutdown());
        let mut players = players;
        let spawned = spawn_vehicles(&mut players, args.vehicles, args.vehicle_area);
        if args.vehicles > 0 {
            println!("spawned {spawned} of {} vehicles", args.vehicles);
        }
        roster = Some(players.roster());
        tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
        runtime = Some(rt);
    }

    let report_every = u64::from(config.rate_hz()) * REPORT_EVERY_SECS;
    if let Some(dir) = &args.resources {
        let (host, errors) = sawgrass_script::Host::load_dir(dir, sawgrass_script::Limits::default());
        for e in &errors {
            eprintln!("sawgrass-server: {e}");
        }
        let names: Vec<&str> = host.resources().iter().map(|r| r.name()).collect();
        println!(
            "resources: {} running ({}), {} failed, script budget {:?} per tick",
            names.len(),
            names.join(", "),
            errors.len(),
            args.script_budget
        );
        let mut scripts = Scripts::new(host, roster.take(), args.script_budget, report_every);
        tick_loop.add_system("scripts", move |_, info| scripts.run_tick(info));
    }

    let main_tid = std::process::id();
    let mut last = (
        Instant::now(),
        cpu::process_time(),
        cpu::thread_time(main_tid),
        0u64,
        0u64,
    );
    let result = tick_loop.run(|tick_loop| {
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
                    ", sent {} kB/s, received {} kB/s, send errors {}, dropped ticks {}, \
                     refused states {} bounds {} speed {} teleport",
                    report.sent_bytes_per_sec / 1000,
                    report.received_bytes_per_sec / 1000,
                    traffic.send_errors.load(Ordering::Relaxed),
                    traffic.dropped_ticks.load(Ordering::Relaxed),
                    traffic.refused_bounds.load(Ordering::Relaxed),
                    traffic.refused_speed.load(Ordering::Relaxed),
                    traffic.refused_teleport.load(Ordering::Relaxed)
                );
                let count = |counter: &std::sync::atomic::AtomicU64| counter.load(Ordering::Relaxed);
                line += &format!(
                    ", limits: dropped {} states {} events, closed {} floods {} bad events, \
                     handshakes retried {} refused {}, refused per address {}",
                    count(&traffic.dropped_states),
                    count(&traffic.dropped_events),
                    count(&traffic.closed_floods),
                    count(&traffic.closed_bad_events),
                    count(&traffic.retried_handshakes),
                    count(&traffic.refused_handshakes),
                    count(&traffic.refused_per_address)
                );
                line += &format!(
                    ", owned states refused {} stale {} movement {} far, owner events lost {}",
                    count(&traffic.refused_owned_stale),
                    count(&traffic.refused_owned_movement),
                    count(&traffic.refused_owned_far),
                    count(&traffic.lost_owner_events)
                );
                if let Ok(mut slot) = slot.lock() {
                    *slot = Some(report);
                }
                last.3 = sent;
                last.4 = received;
            }
            if let Some(overrun) = tick_loop.take_worst_overrun() {
                line += &format!(", worst overrun {overrun}");
            }
            println!("{line}");
            last = (now.0, now.1, now.2, last.3, last.4);
        }
        ControlFlow::Continue(())
    });
    println!("tick {}", tick_loop.metrics().stats());
    if let Err(panic) = result {
        // The world may be half-written: close every connection and stop rather than tick on.
        eprintln!("sawgrass-server: {panic}; stopping");
        if let (Some(shutdown), Some(rt)) = (&shutdown, &runtime) {
            shutdown.close();
            rt.block_on(tokio::time::sleep(Duration::from_millis(200)));
        }
        drop(runtime);
        return ExitCode::FAILURE;
    }
    drop(runtime);
    ExitCode::SUCCESS
}

/// Splits `cores` between the gateway runtime and the snapshot pool, honouring either count that was given. By
/// default the gateway gets a quarter (at least one) and the pool the rest (at least one).
fn split_cores(cores: usize, net: Option<usize>, tick: Option<usize>) -> (usize, usize) {
    let cores = cores.max(1);
    let rest = |n: usize| cores.saturating_sub(n).max(1);
    match (net, tick) {
        (Some(net), Some(tick)) => (net, tick),
        (Some(net), None) => (net, rest(net)),
        (None, Some(tick)) => (rest(tick), tick),
        (None, None) => {
            let net = (cores / 4).max(1);
            (net, rest(net))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::split_cores;

    #[test]
    fn the_gateway_and_the_tick_pool_share_the_cores_without_overlap() {
        assert_eq!(split_cores(1, None, None), (1, 1));
        assert_eq!(split_cores(2, None, None), (1, 1));
        assert_eq!(split_cores(4, None, None), (1, 3));
        assert_eq!(split_cores(16, None, None), (4, 12));
        assert_eq!(split_cores(4, Some(2), None), (2, 2));
        assert_eq!(split_cores(4, None, Some(1)), (3, 1));
        assert_eq!(split_cores(2, Some(1), Some(4)), (1, 4));
        assert_eq!(split_cores(2, Some(3), None), (3, 1));
    }
}
