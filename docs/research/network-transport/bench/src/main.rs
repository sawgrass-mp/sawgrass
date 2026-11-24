//! Transport benchmark for NET-3. See the README one directory up for what it measures and how to run it.

mod common;
mod enet;
#[cfg(feature = "gns")]
mod gns;
mod poll;
mod quic;
mod relay;
mod renet;
mod udp;

use std::net::SocketAddr;

const USAGE: &str = "usage:
  transport-bench relay  LISTEN TARGET DELAY_MS LOSS_PCT [SEED]
  transport-bench server KIND ADDR echo|load [THREADS]
  transport-bench ping   KIND ADDR unreliable|reliable HZ SECS
  transport-bench load   KIND ADDR CLIENTS THREADS SECS
  transport-bench datagram-size quinn ADDR
KIND: udp, quinn, enet, renet, gns (with --features gns)";

fn arg<T: std::str::FromStr>(args: &[String], i: usize) -> T {
    args.get(i).and_then(|a| a.parse().ok()).unwrap_or_else(|| {
        eprintln!("{USAGE}");
        std::process::exit(2)
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd: String = arg(&args, 1);
    if cmd == "relay" {
        let seed = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(1);
        relay::run(arg(&args, 2), arg(&args, 3), arg(&args, 4), arg(&args, 5), seed);
        return;
    }
    let kind: String = arg(&args, 2);
    let addr: SocketAddr = arg(&args, 3);
    match (cmd.as_str(), kind.as_str()) {
        ("server", _) => {
            let load = arg::<String>(&args, 4) == "load";
            match kind.as_str() {
                "udp" => poll::serve(udp::Server::bind(addr), load),
                "enet" => poll::serve(enet::Server::bind(addr), load),
                "renet" => poll::serve(renet::Server::bind(addr), load),
                #[cfg(feature = "gns")]
                "gns" => poll::serve(gns::Server::bind(addr), load),
                "quinn" => quic::serve(addr, load, args.get(5).and_then(|s| s.parse().ok()).unwrap_or(4)),
                _ => eprintln!("{USAGE}"),
            }
        }
        ("ping", _) => {
            let reliable = arg::<String>(&args, 4) == "reliable";
            let (hz, secs): (u32, u64) = (arg(&args, 5), arg(&args, 6));
            let label = format!("{kind} {}", if reliable { "reliable" } else { "unreliable" });
            match kind.as_str() {
                "udp" => poll::ping::<udp::Client>(addr, reliable, hz, secs, &label),
                "enet" => poll::ping::<enet::Client>(addr, reliable, hz, secs, &label),
                "renet" => poll::ping::<renet::Client>(addr, reliable, hz, secs, &label),
                #[cfg(feature = "gns")]
                "gns" => poll::ping::<gns::Client>(addr, reliable, hz, secs, &label),
                "quinn" => quic::ping(addr, reliable, hz, secs, &label),
                _ => eprintln!("{USAGE}"),
            }
        }
        ("datagram-size", "quinn") => quic::datagram_size(addr),
        ("load", _) => {
            let (clients, threads, secs): (usize, usize, u64) = (arg(&args, 4), arg(&args, 5), arg(&args, 6));
            match kind.as_str() {
                "udp" => poll::load::<udp::Client>(addr, clients, threads, secs),
                "enet" => poll::load::<enet::Client>(addr, clients, threads, secs),
                "renet" => poll::load::<renet::Client>(addr, clients, threads, secs),
                #[cfg(feature = "gns")]
                "gns" => poll::load::<gns::Client>(addr, clients, threads, secs),
                "quinn" => quic::load(addr, clients, threads, secs),
                _ => eprintln!("{USAGE}"),
            }
        }
        _ => eprintln!("{USAGE}"),
    }
}
