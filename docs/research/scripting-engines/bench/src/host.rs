//! What every engine shares: the world the scripts read and change through host functions, the engine interface,
//! and the watchdog that stops a script running past its deadline.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const PLAYERS: i64 = 4096;
/// Memory cap per instance where the engine supports one.
#[cfg_attr(feature = "rhai", allow(dead_code))]
pub const MEMORY_LIMIT: usize = 64 << 20;

/// Roleplay state behind the host functions: a wallet and a job per player.
#[derive(Debug)]
pub struct World {
    money: Vec<i64>,
    job: Vec<i64>,
}

impl Default for World {
    fn default() -> Self {
        World {
            money: vec![1000; PLAYERS as usize],
            job: (0..PLAYERS).map(|i| i % 8).collect(),
        }
    }
}

impl World {
    pub fn get_money(&self, id: i64) -> i64 {
        usize::try_from(id)
            .ok()
            .and_then(|i| self.money.get(i))
            .copied()
            .unwrap_or(-1)
    }

    /// Adds `delta` to a wallet; refuses (false) an unknown player or a balance below zero.
    pub fn add_money(&mut self, id: i64, delta: i64) -> bool {
        let Some(m) = usize::try_from(id).ok().and_then(|i| self.money.get_mut(i)) else {
            return false;
        };
        match m.checked_add(delta) {
            Some(v) if v >= 0 => {
                *m = v;
                true
            }
            _ => false,
        }
    }

    pub fn player_count(&self) -> i64 {
        PLAYERS
    }

    pub fn player_job(&self, id: i64) -> i64 {
        usize::try_from(id)
            .ok()
            .and_then(|i| self.job.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// A checksum of every wallet, to compare engines after the same calls.
    pub fn checksum(&self) -> i64 {
        self.money.iter().enumerate().fold(0i64, |acc, (i, m)| {
            acc.wrapping_mul(31).wrapping_add(m ^ i as i64)
        })
    }
}

pub fn host_add(x: i64) -> i64 {
    x + 1
}

pub type Shared = Rc<RefCell<World>>;

/// One engine instance with the roleplay resource loaded. Every call returns the script's error as text.
pub trait Script: Sized {
    /// Creates an instance and loads the resource; `limits` installs the deadline check and the memory cap.
    fn new(world: Shared, limits: bool) -> Result<Self, String>;
    fn noop(&mut self, x: i64) -> Result<i64, String>;
    /// The script calls `host_add` n times.
    fn host_loop(&mut self, n: i64) -> Result<i64, String>;
    fn echo(&mut self, s: &str) -> Result<String, String>;
    fn zone(&mut self, x: f64, y: f64) -> Result<i64, String>;
    fn chat(&mut self, from: i64, line: &str) -> Result<String, String>;
    fn payday(&mut self) -> Result<i64, String>;
    fn inventory(&mut self, seed: i64) -> Result<i64, String>;
    /// Runs one of the misbehaving functions: spin, bomb, recurse, fail.
    fn misbehave(&mut self, case: &str) -> Result<(), String>;
    /// Clears what a stopped call leaves behind, so the instance can be called again.
    fn recover(&mut self) {}
    /// Whether this build enforces a memory cap per instance.
    fn memory_capped(&self) -> bool;
    /// Checksum of the wallets after the calls made so far.
    fn checksum(&self, world: &Shared) -> i64 {
        world.borrow().checksum()
    }
}

// The watchdog: a deadline in nanoseconds since START, and a flag set when it passes. Scripts are stopped by
// engine hooks that read the flag (Lua, Luau, QuickJS, Rhai) or by actions run when it is set (V8 terminates the
// isolate, wasmtime advances the epoch).
static START: OnceLock<Instant> = OnceLock::new();
static DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);
static STOP: AtomicBool = AtomicBool::new(false);
type Kick = Box<dyn Fn() + Send>;
static KICKS: Mutex<Vec<Kick>> = Mutex::new(Vec::new());

fn now_ns() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Starts the watchdog thread; it checks the deadline every 250 µs.
pub fn start_watchdog() {
    now_ns();
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_micros(250));
            if !STOP.load(Ordering::Relaxed) && now_ns() > DEADLINE.load(Ordering::Relaxed) {
                STOP.store(true, Ordering::Relaxed);
                if let Ok(kicks) = KICKS.lock() {
                    for k in kicks.iter() {
                        k();
                    }
                }
            }
        }
    });
}

/// Runs `f` on the watchdog thread whenever a deadline passes.
#[cfg_attr(not(any(feature = "v8", feature = "wasm")), allow(dead_code))]
pub fn on_expire(f: impl Fn() + Send + 'static) {
    if let Ok(mut kicks) = KICKS.lock() {
        kicks.push(Box::new(f));
    }
}

/// Allows the scripts `budget` from now. The server would arm one budget per tick for all script work.
pub fn arm(budget: Duration) {
    STOP.store(false, Ordering::Relaxed);
    DEADLINE.store(now_ns() + budget.as_nanos() as u64, Ordering::Relaxed);
}

#[cfg_attr(any(feature = "v8", feature = "wasm"), allow(dead_code))]
pub fn stopped() -> bool {
    STOP.load(Ordering::Relaxed)
}

#[cfg_attr(any(feature = "v8", feature = "wasm", feature = "quickjs"), allow(dead_code))]
pub const DEADLINE_MESSAGE: &str = "deadline exceeded";
