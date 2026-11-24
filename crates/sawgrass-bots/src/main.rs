//! `sawgrass-bots`: run N load-test bots against a Sawgrass server, or write a test certificate for one.
//!
//! See the crate README for the commands.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use sawgrass_bots::record::Recorder;
use sawgrass_bots::report::{self, Context};
use sawgrass_bots::stats::Counters;
use sawgrass_bots::walker::Area;
use sawgrass_bots::{BotConfig, run};
use sawgrass_gateway::RootCertStore;
use sawgrass_gateway::pki_types::CertificateDer;
use tokio::time::Instant;

/// jemalloc for every allocation in the process (SRV-5; BOT-3 halved the crowd's tick p99 with it).
#[cfg(unix)]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const USAGE: &str = "usage:
  sawgrass-bots gen-cert --out DIR [--name NAME]
  sawgrass-bots run --server ADDR --cert FILE --count N [--duration SECS] [--send-hz N]
                    [--connect-per-sec N] [--area METRES] [--hotspots N] [--seed N] [--threads N]
                    [--report-every SECS] [--name NAME] [--record FILE] [--record-bots N]";

struct RunArgs {
    server: SocketAddr,
    cert: PathBuf,
    name: String,
    count: u64,
    duration: Duration,
    send_hz: u32,
    connect_per_sec: u32,
    area: Area,
    threads: Option<usize>,
    report_every: Duration,
    record: Option<PathBuf>,
    record_bots: u64,
}

fn parse<T: std::str::FromStr>(name: &str, value: Option<String>) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    value
        .ok_or_else(|| format!("{name} needs a value"))?
        .parse()
        .map_err(|e| format!("{name}: {e}"))
}

fn parse_run(mut args: impl Iterator<Item = String>) -> Result<RunArgs, String> {
    let (mut server, mut cert, mut count) = (None, None, None);
    let mut run = RunArgs {
        server: ([127, 0, 0, 1], 4433).into(),
        cert: PathBuf::new(),
        name: "localhost".to_owned(),
        count: 0,
        duration: Duration::from_secs(60),
        send_hz: 20,
        connect_per_sec: 200,
        area: Area::default(),
        threads: None,
        report_every: Duration::from_secs(10),
        record: None,
        record_bots: 32,
    };
    while let Some(flag) = args.next() {
        let value = args.next();
        match flag.as_str() {
            "--server" => server = Some(parse::<SocketAddr>(&flag, value)?),
            "--cert" => cert = Some(PathBuf::from(value.ok_or("--cert needs a value")?)),
            "--name" => run.name = value.ok_or("--name needs a value")?,
            "--count" => count = Some(parse::<u64>(&flag, value)?),
            "--duration" => run.duration = Duration::from_secs(parse(&flag, value)?),
            "--send-hz" => run.send_hz = parse::<u32>(&flag, value)?.clamp(1, 1000),
            "--connect-per-sec" => run.connect_per_sec = parse::<u32>(&flag, value)?.max(1),
            "--area" => run.area.size = parse(&flag, value)?,
            "--hotspots" => run.area.hotspots = parse(&flag, value)?,
            "--seed" => run.area.seed = parse(&flag, value)?,
            "--threads" => run.threads = Some(parse::<usize>(&flag, value)?.max(1)),
            "--report-every" => run.report_every = Duration::from_secs(parse::<u64>(&flag, value)?.max(1)),
            "--record" => run.record = Some(PathBuf::from(value.ok_or("--record needs a value")?)),
            "--record-bots" => run.record_bots = parse(&flag, value)?,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    run.server = server.ok_or("--server is required")?;
    run.cert = cert.ok_or("--cert is required")?;
    run.count = count.ok_or("--count is required")?;
    Ok(run)
}

fn gen_cert(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let (mut out, mut name) = (None, "localhost".to_owned());
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--out" => out = args.next().map(PathBuf::from),
            "--name" => name = args.next().ok_or("--name needs a value")?,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let out = out.ok_or("--out is required")?;
    let cert = rcgen::generate_simple_self_signed(vec![name]).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let write = |file: &str, bytes: &[u8]| {
        let path = out.join(file);
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))
    };
    write("cert.der", cert.cert.der())?;
    write("key.der", &cert.signing_key.serialize_der())?;
    println!(
        "wrote {0}/cert.der and {0}/key.der (self-signed, for tests only)",
        out.display()
    );
    Ok(())
}

async fn run_bots(args: RunArgs, cores: usize) -> Result<(), String> {
    let der = std::fs::read(&args.cert).map_err(|e| format!("{}: {e}", args.cert.display()))?;
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(der))
        .map_err(|e| format!("{}: {e}", args.cert.display()))?;
    let record = match &args.record {
        Some(path) => Some(Arc::new(
            Recorder::create(path, args.record_bots).map_err(|e| format!("{}: {e}", path.display()))?,
        )),
        None => None,
    };
    let config = Arc::new(BotConfig {
        server: args.server,
        server_name: args.name.clone(),
        roots,
        send_hz: args.send_hz,
        area: args.area,
        record: record.clone(),
    });
    let counters = Arc::new(Counters::default());
    let ctx = Context {
        bots: args.count,
        send_hz: args.send_hz,
        cores,
    };
    let start = Instant::now();
    let ramp = Duration::from_secs_f64(args.count as f64 / f64::from(args.connect_per_sec));
    let stop = start + ramp + args.duration;
    println!(
        "{} bots against {} at {} Hz, connecting {}/s ({:.0} s), then {} s of load",
        args.count,
        args.server,
        args.send_hz,
        args.connect_per_sec,
        ramp.as_secs_f64(),
        args.duration.as_secs()
    );

    let spawner = {
        let counters = counters.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let gap = Duration::from_secs(1) / args.connect_per_sec;
            let mut tasks = Vec::with_capacity(args.count as usize);
            for index in 0..args.count {
                tokio::time::sleep_until(start + gap * index as u32).await;
                tasks.push(tokio::spawn(run(
                    config.clone(),
                    index,
                    counters.clone(),
                    index == 0,
                    stop,
                    None,
                )));
            }
            for task in tasks {
                let _ = task.await;
            }
        })
    };

    // The report covers the load phase only: from the end of the ramp to the stop.
    tokio::time::sleep_until(start + ramp).await;
    let load_start = counters.sample(start.elapsed());
    let mut previous = load_start.clone();
    let mut next = Instant::now() + args.report_every;
    while next < stop {
        tokio::time::sleep_until(next).await;
        let now = counters.sample(start.elapsed());
        let server = counters.server.lock().ok().and_then(|s| *s);
        print!("{}", report::render(&previous, &now, server.as_ref(), &ctx));
        println!();
        previous = now;
        next += args.report_every;
    }
    tokio::time::sleep_until(stop).await;
    let end = counters.sample(start.elapsed());
    let server = counters.server.lock().ok().and_then(|s| *s);
    println!("== whole load phase");
    print!("{}", report::render(&load_start, &end, server.as_ref(), &ctx));
    if let Ok(Some(error)) = counters.first_error.lock().map(|e| e.clone()) {
        println!("first connection error: {error}");
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), spawner).await;
    if let (Some(recorder), Some(path)) = (&record, &args.record) {
        recorder.flush().map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next();
    let result = match command.as_deref() {
        Some("gen-cert") => gen_cert(args),
        Some("run") => parse_run(args).and_then(|run_args| {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(run_args.threads.unwrap_or(cores))
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(run_bots(run_args, cores))
        }),
        _ => Err("expected a command".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sawgrass-bots: {e}");
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
