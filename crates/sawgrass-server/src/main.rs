//! The Sawgrass world server.
//!
//! Runs the tick loop and prints tick metrics every five seconds. Until the gateway connection (NET-2) lands
//! the world is empty.
//!
//! ```text
//! sawgrass-server [--tick-hz N] [--ticks N]
//! ```
//!
//! - `--tick-hz N`: ticks per second, 1 to 1000 (default 20).
//! - `--ticks N`: stop after N ticks and print the final metrics (default: run until killed).

use std::ops::ControlFlow;
use std::process::ExitCode;

use sawgrass_server::{TickConfig, TickLoop};

const REPORT_EVERY_SECS: u64 = 5;

struct Args {
    tick_hz: u32,
    ticks: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        tick_hz: TickConfig::DEFAULT_RATE_HZ,
        ticks: None,
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
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let (args, config) = match parse_args().and_then(|args| {
        let config = TickConfig::new(args.tick_hz).map_err(|e| e.to_string())?;
        Ok((args, config))
    }) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("sawgrass-server: {err}");
            eprintln!("usage: sawgrass-server [--tick-hz N] [--ticks N]");
            return ExitCode::from(2);
        }
    };

    println!(
        "sawgrass-server {} (protocol {}), {} Hz",
        env!("CARGO_PKG_VERSION"),
        sawgrass_protocol::PROTOCOL_VERSION,
        config.rate_hz()
    );

    let report_every = u64::from(config.rate_hz()) * REPORT_EVERY_SECS;
    let mut tick_loop = TickLoop::new(config);
    tick_loop.run(|tick_loop| {
        let tick = tick_loop.tick();
        if args.ticks.is_some_and(|limit| tick >= limit) {
            return ControlFlow::Break(());
        }
        if tick > 0 && tick % report_every == 0 {
            println!("tick {}", tick_loop.metrics().stats());
        }
        ControlFlow::Continue(())
    });
    println!("tick {}", tick_loop.metrics().stats());
    ExitCode::SUCCESS
}
