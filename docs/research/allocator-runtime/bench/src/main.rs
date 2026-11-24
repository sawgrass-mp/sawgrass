//! Allocators and async runtimes on Sawgrass's hot paths (PERF-1). See `../README.md`.
//!
//! ```text
//! runtime-bench server <tokio-mt|tokio-tpc|smol|compio> <addr> <threads> [endpoints]
//! runtime-bench load <addr> <clients> <threads> <seconds>
//! runtime-bench tick <players> <warmup ticks> <ticks>
//! runtime-bench ledger <tokio|compio> <dir> <callers> <closed|RATE> <seconds>
//! ```
//!
//! The allocator is chosen when building: `--features mimalloc`, `--features jemalloc`, or neither for the
//! system allocator.

mod cgw;
mod common;
mod gw;
mod ledger;
mod load;
mod tick;

#[cfg(all(feature = "mimalloc", feature = "jemalloc"))]
compile_error!("choose one allocator feature");

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn arg<T: std::str::FromStr>(args: &[String], i: usize) -> T {
    args.get(i)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| usage())
}

fn usage() -> ! {
    eprintln!(
        "usage: runtime-bench server RUNTIME ADDR THREADS [ENDPOINTS] | load ADDR CLIENTS THREADS SECS | \
         tick PLAYERS WARMUP TICKS | ledger RUNTIME DIR CALLERS closed|RATE SECS"
    );
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => gw::server(
            &args.get(2).cloned().unwrap_or_else(|| usage()),
            arg(&args, 3),
            arg(&args, 4),
            args.get(5).and_then(|v| v.parse().ok()).unwrap_or(1),
        ),
        Some("load") => load::load(arg(&args, 2), arg(&args, 3), arg(&args, 4), arg(&args, 5)),
        Some("tick") => tick::tick(arg(&args, 2), arg(&args, 3), arg(&args, 4)),
        Some("ledger") => {
            let mode: String = arg(&args, 5);
            ledger::ledger(
                &args.get(2).cloned().unwrap_or_else(|| usage()),
                std::path::Path::new(&args.get(3).cloned().unwrap_or_else(|| usage())),
                arg(&args, 4),
                if mode == "closed" {
                    None
                } else {
                    Some(arg(&args, 5))
                },
                arg(&args, 6),
            );
        }
        _ => usage(),
    }
}
