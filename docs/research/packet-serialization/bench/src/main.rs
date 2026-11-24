//! Serialization benchmark for NET-4. See the README one directory up for what it measures and how to run it.
//!
//! Commands:
//!   sizes            encoded size of every message in every format
//!   speed            encode, decode and zero-copy read time
//!   fuzz FORMAT N    decode N random and N mutated inputs; count errors, accepted inputs and panics

mod flat;
mod formats;
mod handwritten;
mod model;
#[allow(clippy::all, unsafe_op_in_unsafe_fn, mismatched_lifetime_syntaxes, unused_imports, dead_code, non_camel_case_types)]
#[rustfmt::skip]
mod packets_generated;

use std::hint::black_box;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use formats::all;
use model::*;

/// Entities in a snapshot: about what the 40 KB/s budget allows at 20 Hz (architecture.md says 100 to 150).
const VISIBLE: usize = 100;

fn sizes() {
    for (msg, packet) in messages(1, VISIBLE) {
        for mut f in all() {
            let n = f.encode(&packet).len();
            let bytes = f.encode(&packet).to_vec();
            assert_eq!(
                f.decode(&bytes).as_ref(),
                Some(&packet),
                "{} round trip",
                f.name()
            );
            println!(
                "{{\"suite\":\"size\",\"message\":\"{msg}\",\"format\":\"{}\",\"bytes\":{n}}}",
                f.name()
            );
        }
    }
    // Bytes per entity, from the difference between snapshots of 100 and 150 entities.
    for mut f in all() {
        let a = f.encode(&messages(1, 100)[0].1).len();
        let b = f.encode(&messages(1, 150)[0].1).len();
        println!(
            "{{\"suite\":\"size\",\"message\":\"per_entity\",\"format\":\"{}\",\"bytes\":{}}}",
            f.name(),
            (b - a) as f64 / 50.0
        );
    }
}

/// Median nanoseconds per call over five rounds of at least 200 ms each.
fn time(mut op: impl FnMut()) -> f64 {
    for _ in 0..1000 {
        op();
    }
    let mut rounds = Vec::new();
    for _ in 0..5 {
        let mut n: u64 = 0;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            for _ in 0..100 {
                op();
            }
            n += 100;
        }
        rounds.push(start.elapsed().as_nanos() as f64 / n as f64);
    }
    rounds.sort_by(f64::total_cmp);
    rounds[2]
}

fn speed() {
    for (msg, packet) in messages(1, VISIBLE) {
        for mut f in all() {
            let bytes = f.encode(&packet).to_vec();
            let encode = time(|| {
                black_box(f.encode(black_box(&packet)));
            });
            let decode = time(|| {
                black_box(f.decode(black_box(&bytes)));
            });
            let read = time(|| {
                black_box(f.read(black_box(&bytes)));
            });
            assert_eq!(f.read(&bytes), Some(checksum(&packet)), "{} read", f.name());
            println!(
                "{{\"suite\":\"speed\",\"message\":\"{msg}\",\"format\":\"{}\",\"encode_ns\":{encode:.1},\"decode_ns\":{decode:.1},\"read_ns\":{read:.1},\"zero_copy_read\":{}}}",
                f.name(),
                f.zero_copy()
            );
        }
    }
}

/// Decodes random bytes and corrupted valid packets. Prints how many were rejected, how many were accepted
/// (decoded into some packet) and how many panicked, and the slowest single decode.
fn fuzz(name: &str, n: u64) {
    let mut f = all().into_iter().find(|f| f.name() == name).expect("format name");
    std::panic::set_hook(Box::new(|_| {}));
    let valid: Vec<Vec<u8>> = messages(1, VISIBLE)
        .iter()
        .map(|(_, p)| f.encode(p).to_vec())
        .collect();
    let mut rng = Rng(0x5EED);
    let (mut rejected, mut accepted, mut panicked) = (0u64, 0u64, 0u64);
    let mut slowest = Duration::ZERO;
    let mut input = Vec::new();
    for i in 0..2 * n {
        input.clear();
        if i < n {
            // Random bytes, 0 to 4 KB.
            let len = (rng.next() % 4096) as usize;
            input.extend((0..len).map(|_| rng.next() as u8));
        } else {
            // A valid packet with 1 to 4 bytes changed, cut short, or with bytes appended.
            input.extend_from_slice(&valid[(rng.next() % valid.len() as u64) as usize]);
            match rng.next() % 3 {
                0 => {
                    for _ in 0..1 + rng.next() % 4 {
                        let at = (rng.next() % input.len() as u64) as usize;
                        input[at] = rng.next() as u8;
                    }
                }
                1 => input.truncate((rng.next() % input.len() as u64) as usize),
                _ => input.extend((0..1 + rng.next() % 16).map(|_| rng.next() as u8)),
            }
        }
        let start = Instant::now();
        match catch_unwind(AssertUnwindSafe(|| f.decode(&input))) {
            Ok(Some(_)) => accepted += 1,
            Ok(None) => rejected += 1,
            Err(_) => panicked += 1,
        }
        slowest = slowest.max(start.elapsed());
    }
    println!(
        "{{\"suite\":\"fuzz\",\"format\":\"{name}\",\"inputs\":{},\"rejected\":{rejected},\"accepted\":{accepted},\"panicked\":{panicked},\"slowest_us\":{:.1}}}",
        2 * n,
        slowest.as_secs_f64() * 1e6
    );
}

/// A sum over every field, so that a read that skips fields cannot look faster than it is.
pub fn checksum(p: &Packet) -> u64 {
    let e = |e: &Entity| -> u64 {
        u64::from(e.id)
            .wrapping_add(e.pos.iter().map(|&v| v as u64).fold(0, u64::wrapping_add))
            .wrapping_add(e.vel.iter().map(|&v| v as u64).fold(0, u64::wrapping_add))
            .wrapping_add(u64::from(e.rot))
    };
    match p {
        Packet::Snapshot(s) => {
            u64::from(s.seq) + u64::from(s.tick) + s.entities.iter().map(e).fold(0, u64::wrapping_add)
        }
        Packet::ClientState(c) => {
            u64::from(c.seq)
                + u64::from(c.tick)
                + e(&c.me)
                + u64::from(c.buttons)
                + c.aim[0] as u64
                + c.aim[1] as u64
        }
        Packet::Chat(c) => {
            u64::from(c.seq)
                + u64::from(c.tick)
                + u64::from(c.from)
                + u64::from(c.channel)
                + c.text.len() as u64
        }
        Packet::ScriptEvent(s) => {
            u64::from(s.seq)
                + u64::from(s.tick)
                + s.name.len() as u64
                + s.args
                    .iter()
                    .map(|a| match a {
                        Arg::Bool(b) => u64::from(*b),
                        Arg::Int(i) => *i as u64,
                        Arg::Float(f) => u64::from(f.to_bits()),
                        Arg::Str(s) => s.len() as u64,
                    })
                    .fold(0, u64::wrapping_add)
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("sizes") => sizes(),
        Some("speed") => speed(),
        Some("fuzz") => fuzz(
            &args[2],
            args.get(3).and_then(|n| n.parse().ok()).unwrap_or(100_000),
        ),
        _ => eprintln!("usage: serialization-bench sizes | speed | fuzz FORMAT [N]"),
    }
}
