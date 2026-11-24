//! Proximity voice chat harness for Sawgrass (NET-7). See ../README.md.

mod codec;
mod common;
mod relay;
mod stats;
mod world;

use std::net::SocketAddr;

fn arg<T: std::str::FromStr>(args: &[String], i: usize, default: T) -> T {
    args.get(i).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let addr = |i: usize| -> SocketAddr { arg(&args, i, "127.0.0.1:4433".parse().expect("addr")) };
    match args.get(1).map(String::as_str) {
        Some("codec") => codec::suite(),
        // world PLAYERS SHARE SEED PAYLOAD
        Some("world") => stats::world(
            arg(&args, 2, 4096),
            arg(&args, 3, 0.10),
            arg(&args, 4, 1),
            arg(&args, 5, 60),
        ),
        // serve ADDR PLAYERS SEED BATCH(0|1) FLUSH_MS THREADS
        Some("serve") => relay::serve(
            addr(2),
            arg(&args, 3, 4096),
            arg(&args, 4, 1),
            arg::<u8>(&args, 5, 0) == 1,
            arg(&args, 6, 20),
            arg(&args, 7, 2),
        ),
        // load ADDR PLAYERS SEED SHARE PAYLOAD FRAME_MS THREADS SECS
        Some("load") => relay::load(
            addr(2),
            arg(&args, 3, 4096),
            arg(&args, 4, 1),
            arg(&args, 5, 0.10),
            arg(&args, 6, 60),
            arg(&args, 7, 20),
            arg(&args, 8, 2),
            arg(&args, 9, 30),
        ),
        _ => {
            eprintln!("usage: voice-bench codec | world N SHARE SEED | serve ... | load ...");
            std::process::exit(2);
        }
    }
}
