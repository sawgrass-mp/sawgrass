//! The roleplay workloads as a WebAssembly guest, the same logic as `../scripts/rp.lua`, `rp.js` and `rp.rhai`.
//!
//! A mod author would write this in Rust, AssemblyScript, C, Go or any language that targets WebAssembly; Rust is
//! used here. Strings cross the boundary through two buffers in guest memory: the host asks for the input buffer
//! with `buf_in(len)`, writes into it, calls the function, and reads the reply from `buf_out()`.
//!
//! `unsafe extern` and `unsafe(no_mangle)` are how Rust declares WebAssembly imports and exports; the imports are
//! declared `safe`, and no code here dereferences a pointer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hint::black_box;

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    safe fn host_add(x: i64) -> i64;
    safe fn get_money(id: i64) -> i64;
    safe fn add_money(id: i64, delta: i64) -> i32;
    safe fn player_count() -> i64;
    safe fn player_job(id: i64) -> i64;
}

thread_local! {
    static IN: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static ZONES: Vec<(f64, f64, f64)> = zones();
}

fn lcg(seed: i64) -> i64 {
    (seed * 16807) % 2147483647
}

fn zones() -> Vec<(f64, f64, f64)> {
    let mut s = 42;
    (0..64)
        .map(|_| {
            s = lcg(s);
            let x = s % 4000 - 2000;
            s = lcg(s);
            let y = s % 4000 - 2000;
            s = lcg(s);
            let r = 20 + s % 80;
            (x as f64, y as f64, (r * r) as f64)
        })
        .collect()
}

const SALARY: [i64; 8] = [0, 300, 450, 500, 650, 800, 1200, 2000];
const NAMES: [&str; 16] = [
    "water",
    "bread",
    "phone",
    "bandage",
    "lockpick",
    "radio",
    "cigarettes",
    "lighter",
    "burger",
    "coffee",
    "repairkit",
    "rope",
    "flashlight",
    "map",
    "keys",
    "wallet",
];

fn weight(name: &str) -> i64 {
    NAMES
        .iter()
        .position(|n| *n == name)
        .map_or(0, |i| (i as i64 + 1) * 10)
}

/// Resizes the input buffer to `len` bytes and returns its address.
#[unsafe(no_mangle)]
pub extern "C" fn buf_in(len: i32) -> i32 {
    IN.with_borrow_mut(|b| {
        b.clear();
        b.resize(len as usize, 0);
        b.as_ptr() as i32
    })
}

/// Address of the output buffer, valid until the next call.
#[unsafe(no_mangle)]
pub extern "C" fn buf_out() -> i32 {
    OUT.with_borrow(|b| b.as_ptr() as i32)
}

fn reply(s: &str) -> i32 {
    OUT.with_borrow_mut(|b| {
        b.clear();
        b.extend_from_slice(s.as_bytes());
        b.len() as i32
    })
}

fn input() -> String {
    IN.with_borrow(|b| String::from_utf8_lossy(b).into_owned())
}

#[unsafe(no_mangle)]
pub extern "C" fn noop(x: i64) -> i64 {
    x + 1
}

#[unsafe(no_mangle)]
pub extern "C" fn host_loop(n: i64) -> i64 {
    (1..=n).map(|i| host_add(i)).sum()
}

/// Input in the input buffer; returns the reply's length.
#[unsafe(no_mangle)]
pub extern "C" fn echo() -> i32 {
    let mut s = input();
    s.push('!');
    reply(&s)
}

#[unsafe(no_mangle)]
pub extern "C" fn zone(x: f64, y: f64) -> i64 {
    ZONES.with(|zones| {
        for (i, &(zx, zy, r2)) in zones.iter().enumerate() {
            let (dx, dy) = (x - zx, y - zy);
            if dx * dx + dy * dy <= r2 {
                return i as i64;
            }
        }
        -1
    })
}

/// Line in the input buffer; returns the reply's length.
#[unsafe(no_mangle)]
pub extern "C" fn chat(from: i64) -> i32 {
    let line = input();
    let words: Vec<&str> = line.split_whitespace().collect();
    let out = match words.first().copied().unwrap_or("") {
        "/pay" => {
            let to = words.get(1).and_then(|w| w.parse::<i64>().ok());
            let amount = words.get(2).and_then(|w| w.parse::<i64>().ok());
            match (to, amount) {
                (Some(to), Some(amount)) if amount > 0 => {
                    if get_money(from) < amount {
                        "Not enough money".to_string()
                    } else {
                        add_money(from, -amount);
                        add_money(to, amount);
                        format!("Paid ${amount} to {to}")
                    }
                }
                _ => "Usage: /pay <id> <amount>".to_string(),
            }
        }
        "/me" => format!("* Player {from} {}", words[1..].join(" ")),
        "/roll" => format!("Player {from} rolled {}", (from * 7) % 100),
        cmd => format!("Unknown command: {cmd}"),
    };
    reply(&out)
}

#[unsafe(no_mangle)]
pub extern "C" fn payday() -> i64 {
    let mut total = 0;
    for i in 0..player_count() {
        let s = SALARY[player_job(i) as usize];
        if s > 0 {
            add_money(i, s);
            total += s;
        }
    }
    total
}

struct Item {
    name: &'static str,
    count: i64,
}

#[unsafe(no_mangle)]
pub extern "C" fn inventory(seed: i64) -> i64 {
    let mut items = Vec::new();
    let mut s = seed;
    for _ in 0..64 {
        s = lcg(s);
        let name = NAMES[(s % 16) as usize];
        s = lcg(s);
        items.push(Item {
            name,
            count: 1 + s % 5,
        });
    }
    let mut stacks: HashMap<&str, i64> = HashMap::new();
    for it in &items {
        *stacks.entry(it.name).or_insert(0) += it.count;
    }
    let w: i64 = stacks.iter().map(|(name, count)| count * weight(name)).sum();
    w * 100 + stacks.len() as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn spin() {
    let mut i = 0u64;
    loop {
        i = black_box(i + 1);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn bomb() {
    let mut t = Vec::new();
    let mut i = 1u64;
    loop {
        t.push(format!("{}{i}", "x".repeat(1024)));
        black_box(&t);
        i += 1;
    }
}

#[unsafe(no_mangle)]
#[allow(unconditional_recursion)] // the point: it overflows the stack
pub extern "C" fn recurse(n: i64) -> i64 {
    black_box(recurse(black_box(n + 1))) + 1
}

#[unsafe(no_mangle)]
pub extern "C" fn fail() {
    panic!("boom");
}
