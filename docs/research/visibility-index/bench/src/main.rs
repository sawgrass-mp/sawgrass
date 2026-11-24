//! IM-2 harness: spatial indexes for visible sets at 4096 players.
//!
//!     visibility-index-bench run <index> <scenario> <mode> <threads> <ticks> [sorted|bits]
//!     visibility-index-bench verify <scenario> <ticks>
//!     visibility-index-bench list
//!
//! `run` moves 4096 players at 20 Hz and every tick brings the index up to date, then recomputes every
//! player's visible set (enter 200 m, leave 230 m, as the server's `VisibilityConfig::default`). It prints one
//! JSON line. `verify` checks every index's query results against brute force.

mod external;
mod index;
mod tree;
mod world;

use std::time::{Duration, Instant};

use cap::Cap;
use sawgrass_server::interest::{EntityId, Point, VisibilityChanges, VisibilityConfig, VisibleSet};

use index::{Brute, CurrentGrid, DenseGrid, SpatialIndex, keep_nearest};
use world::{SCENARIOS, World};

#[global_allocator]
static ALLOC: Cap<std::alloc::System> = Cap::new(std::alloc::System, usize::MAX);

const PLAYERS: usize = 4096;
const ENTER: f32 = 200.0;
const LEAVE: f32 = 230.0;
/// The map the dense grid covers. The scenarios stay inside it.
const MAP: f32 = 8192.0;
const WARMUP: usize = 20;

const INDEXES: [&str; 12] = [
    "current",
    "grid-current",
    "grid-dense-25",
    "grid-dense-50",
    "grid-dense-100",
    "quadtree",
    "bvh",
    "rstar-bulk",
    "rstar-insert",
    "kiddo-immutable",
    "kiddo-mutable",
    "brute",
];

fn make(name: &str) -> Option<Box<dyn SpatialIndex>> {
    Some(match name {
        "grid-current" => Box::new(CurrentGrid::new(100.0)),
        "grid-dense-25" => Box::new(DenseGrid::new([0.0, 0.0], MAP, 25.0)),
        "grid-dense-50" => Box::new(DenseGrid::new([0.0, 0.0], MAP, 50.0)),
        "grid-dense-100" => Box::new(DenseGrid::new([0.0, 0.0], MAP, 100.0)),
        "quadtree" => Box::new(tree::Tree::new(tree::Split::Quad {
            leaf: 16,
            max_depth: 20,
        })),
        "bvh" => Box::new(tree::Tree::new(tree::Split::Median { leaf: 8 })),
        "rstar-bulk" => Box::new(external::RStar::new(true)),
        "rstar-insert" => Box::new(external::RStar::new(false)),
        "kiddo-immutable" => Box::new(external::Kiddo::immutable()),
        "kiddo-mutable" => Box::new(external::Kiddo::mutable()),
        "brute" => Box::new(Brute::default()),
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Everyone within the radii, as the server does today.
    Radius,
    /// At most this many nearest within the radii: the cut architecture.md plans (100 to 150 entities).
    Nearest(usize),
}

/// One player's visible set. `Sorted`: the same algorithm as the server's `VisibleSet::update` (sorted ids,
/// a binary search per candidate, a sort and a merge), over any index. `Bits`: one bit per entity id, so
/// membership is a bit test and the diff is a few word operations; entered and left ids are still listed.
enum Viewer {
    Sorted {
        members: Vec<u32>,
        next: Vec<u32>,
    },
    Bits {
        members: Vec<u64>,
        next: Vec<u64>,
        len: usize,
    },
}

impl Viewer {
    fn new(bits: bool) -> Self {
        if bits {
            let words = PLAYERS.div_ceil(64);
            Self::Bits {
                members: vec![0; words],
                next: vec![0; words],
                len: 0,
            }
        } else {
            Self::Sorted {
                members: Vec::new(),
                next: Vec::new(),
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Sorted { members, .. } => members.len(),
            Self::Bits { len, .. } => *len,
        }
    }
}

impl Viewer {
    /// Returns how many entities entered plus left. `changes` is the caller's scratch list of them (bits only;
    /// a server hands them to the gateway right away, so one list per thread is enough).
    fn update(
        &mut self,
        index: &dyn SpatialIndex,
        me: u32,
        at: [f32; 2],
        mode: Mode,
        cand: &mut Vec<(f32, u32)>,
        changes: &mut Vec<u32>,
    ) -> usize {
        cand.clear();
        match mode {
            Mode::Radius => index.within(at, LEAVE, cand),
            Mode::Nearest(k) => {
                index.nearest(at, LEAVE, k + 1, cand);
                cand.retain(|&(_, id)| id != me);
                keep_nearest(cand, k);
            }
        }
        let enter2 = ENTER * ENTER;
        match self {
            Self::Sorted { members, next } => {
                next.clear();
                for &(d, id) in cand.iter() {
                    if id != me && (d <= enter2 || members.binary_search(&id).is_ok()) {
                        next.push(id);
                    }
                }
                next.sort_unstable();
                let changed = diff(members, next);
                std::mem::swap(members, next);
                changed
            }
            Self::Bits { members, next, len } => {
                next.iter_mut().for_each(|w| *w = 0);
                for &(d, id) in cand.iter() {
                    let (w, b) = (id as usize / 64, 1u64 << (id % 64));
                    if id != me && (d <= enter2 || members[w] & b != 0) {
                        next[w] |= b;
                    }
                }
                // List what entered and left, as a server must, then swap.
                changes.clear();
                let mut count = 0;
                for (w, (&old, &new)) in members.iter().zip(next.iter()).enumerate() {
                    count += new.count_ones() as usize;
                    let mut moved = old ^ new;
                    while moved != 0 {
                        changes.push((w * 64) as u32 + moved.trailing_zeros());
                        moved &= moved - 1;
                    }
                }
                *len = count;
                std::mem::swap(members, next);
                changes.len()
            }
        }
    }
}

fn diff(old: &[u32], new: &[u32]) -> usize {
    let (mut i, mut j, mut n) = (0, 0, 0);
    while i < old.len() && j < new.len() {
        match old[i].cmp(&new[j]) {
            std::cmp::Ordering::Less => (i, n) = (i + 1, n + 1),
            std::cmp::Ordering::Greater => (j, n) = (j + 1, n + 1),
            std::cmp::Ordering::Equal => (i, j) = (i + 1, j + 1),
        }
    }
    n + (old.len() - i) + (new.len() - j)
}

/// Runs `work(first, chunk)` over `items` split into `threads` contiguous chunks; returns the sum.
fn parallel<T: Send>(
    items: &mut [T],
    threads: usize,
    work: impl Fn(usize, &mut [T]) -> usize + Sync,
) -> usize {
    if threads <= 1 {
        return work(0, items);
    }
    let size = items.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks_mut(size)
            .enumerate()
            .map(|(c, chunk)| {
                let work = &work;
                s.spawn(move || work(c * size, chunk))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
    })
}

struct Stats(Vec<Duration>);

impl Stats {
    fn json(&mut self, name: &str) -> String {
        self.0.sort_unstable();
        let n = self.0.len();
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        let p = |q: f64| us(self.0[((n - 1) as f64 * q).round() as usize]);
        let mean = us(self.0.iter().sum::<Duration>()) / n as f64;
        format!(
            r#""{name}_p50_us":{:.1},"{name}_p99_us":{:.1},"{name}_max_us":{:.1},"{name}_mean_us":{mean:.1}"#,
            p(0.5),
            p(0.99),
            p(1.0)
        )
    }
}

fn run(name: &str, scenario: &str, mode: Mode, threads: usize, ticks: usize, bits: bool) -> String {
    let mode_name = match mode {
        Mode::Radius => "radius".to_string(),
        Mode::Nearest(k) => format!("nearest-{k}"),
    };
    let head = format!(
        r#"{{"index":"{name}","scenario":"{scenario}","mode":"{mode_name}","sets":"{}","threads":{threads},"players":{PLAYERS},"ticks":{ticks}"#,
        if bits { "bits" } else { "sorted" }
    );
    let mut world = World::new(scenario, PLAYERS);
    let before = ALLOC.allocated();
    // "current" is the server's own code end to end: SpatialGrid plus its VisibleSet, radius mode only.
    let current = name == "current";
    let mut index = if current { None } else { make(name) };
    let mut grid = current.then(|| CurrentGrid::new(100.0));
    let mut viewers: Vec<Viewer> = (0..PLAYERS).map(|_| Viewer::new(bits)).collect();
    let mut sets: Vec<(VisibleSet, VisibilityChanges)> = if current {
        (0..PLAYERS).map(|_| Default::default()).collect()
    } else {
        Vec::new()
    };
    let config = VisibilityConfig::new(ENTER, LEAVE);
    let (mut update_t, mut query_t, mut tick_t) = (Vec::new(), Vec::new(), Vec::new());
    let (mut visible, mut changes, mut candidates) = (0u64, 0u64, 0u64);

    let mut ids: Vec<u32> = (0..PLAYERS as u32).collect();
    for t in 0..WARMUP + ticks {
        world.step();
        let points = world.points();

        let start = Instant::now();
        let updated = match (&mut index, &mut grid) {
            (Some(index), _) => index.update(points),
            (None, Some(grid)) => grid.update(points),
            _ => Ok(()),
        };
        let update = start.elapsed();
        if let Err(e) = updated {
            return format!(r#"{head},"error":"{}"}}"#, e.replace('"', "'"));
        }

        // Queries alone: what the index costs, without the visible-set bookkeeping.
        let start = Instant::now();
        let found = if let Some(index) = &index {
            let index = &**index;
            parallel(&mut ids, threads, |_, chunk| {
                let mut cand = Vec::new();
                let mut n = 0;
                for &me in chunk.iter() {
                    cand.clear();
                    let at = points[me as usize];
                    match mode {
                        Mode::Radius => index.within(at, LEAVE, &mut cand),
                        Mode::Nearest(k) => index.nearest(at, LEAVE, k + 1, &mut cand),
                    }
                    n += cand.len();
                }
                n
            })
        } else {
            0
        };
        let query = start.elapsed();

        let start = Instant::now();
        let changed = if let Some(index) = &index {
            let index = &**index;
            parallel(&mut viewers, threads, |first, chunk| {
                let (mut cand, mut changes) = (Vec::new(), Vec::new());
                let mut n = 0;
                for (i, v) in chunk.iter_mut().enumerate() {
                    let me = (first + i) as u32;
                    n += v.update(index, me, points[me as usize], mode, &mut cand, &mut changes);
                }
                n
            })
        } else if let Some(grid) = &grid {
            let grid = grid.grid();
            parallel(&mut sets, threads, |first, chunk| {
                let mut n = 0;
                for (i, (set, ch)) in chunk.iter_mut().enumerate() {
                    let me = first + i;
                    let p = points[me];
                    set.update(
                        grid,
                        Some(EntityId(me as u32)),
                        Point::new(p[0], p[1]),
                        &config,
                        ch,
                    );
                    n += ch.entered.len() + ch.left.len();
                }
                n
            })
        } else {
            0
        };
        let sets_time = start.elapsed();

        if t >= WARMUP {
            update_t.push(update);
            query_t.push(query);
            tick_t.push(update + sets_time);
            changes += changed as u64;
            candidates += found as u64;
            visible += if current {
                sets.iter().map(|(s, _)| s.len() as u64).sum::<u64>()
            } else {
                viewers.iter().map(|v| v.len() as u64).sum::<u64>()
            };
        }
    }

    let held = ALLOC.allocated();
    drop(index);
    drop(grid);
    let index_bytes = held - ALLOC.allocated();
    let held = ALLOC.allocated();
    drop(viewers);
    drop(sets);
    let sets_bytes = held - ALLOC.allocated();
    let _ = before;
    let per = (PLAYERS * ticks) as f64;
    let mut out = head;
    out += &format!(",{}", Stats(update_t).json("update"));
    if !current {
        out += &format!(",{}", Stats(query_t).json("query"));
    }
    out += &format!(",{}", Stats(tick_t).json("tick"));
    out += &format!(
        r#","visible_avg":{:.1},"candidates_avg":{:.1},"changes_avg":{:.3},"index_bytes":{index_bytes},"sets_bytes":{sets_bytes}}}"#,
        visible as f64 / per,
        candidates as f64 / per,
        changes as f64 / per
    );
    out
}

/// Every index against brute force: `within` must return exactly the same entities, `nearest` the same
/// distances (which of several tied entities it picks may differ).
fn verify(scenario: &str, ticks: usize) -> Vec<String> {
    let mut world = World::new(scenario, PLAYERS);
    let names: Vec<&str> = INDEXES
        .iter()
        .copied()
        .filter(|&n| n != "current" && n != "brute")
        .collect();
    let mut indexes: Vec<Option<Box<dyn SpatialIndex>>> = names.iter().map(|n| make(n)).collect();
    let mut errors: Vec<Option<String>> = vec![None; names.len()];
    let mut bad = vec![(0u64, 0u64); names.len()];
    let mut brute = Brute::default();
    let (mut checked, mut a, mut b) = (0u64, Vec::new(), Vec::new());
    for _ in 0..ticks {
        world.step();
        let points = world.points();
        brute.update(points).ok();
        for (i, slot) in indexes.iter_mut().enumerate() {
            if let Some(index) = slot
                && let Err(e) = index.update(points)
            {
                errors[i] = Some(e);
                *slot = None;
            }
        }
        for (v, &at) in points.iter().enumerate().step_by(7) {
            checked += 1;
            let radius = [LEAVE, 25.0, 1000.0][v % 3];
            for (i, slot) in indexes.iter().enumerate() {
                let Some(index) = slot else { continue };
                for nearest in [false, true] {
                    a.clear();
                    b.clear();
                    if nearest {
                        brute.nearest(at, radius, 129, &mut a);
                        index.nearest(at, radius, 129, &mut b);
                        a.iter_mut().chain(b.iter_mut()).for_each(|e| e.1 = 0);
                    } else {
                        brute.within(at, radius, &mut a);
                        index.within(at, radius, &mut b);
                    }
                    a.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
                    b.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
                    // Same entities; distances may differ in the last bit between formulas.
                    let same = a.len() == b.len()
                        && a.iter()
                            .zip(&b)
                            .all(|(x, y)| x.1 == y.1 && (x.0 - y.0).abs() <= 1e-3 * x.0.max(1.0));
                    if !same {
                        if nearest {
                            bad[i].1 += 1;
                        } else {
                            bad[i].0 += 1;
                        }
                    }
                }
            }
        }
    }
    names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let error = errors[i].as_deref().map_or("null".to_string(), |e| format!("\"{}\"", e.replace('"', "'")));
            format!(
                r#"{{"suite":"verify","scenario":"{scenario}","index":"{n}","queries":{checked},"within_mismatches":{},"nearest_mismatches":{},"error":{error}}}"#,
                bad[i].0, bad[i].1
            )
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    match arg(0) {
        "run" => {
            let mode = match arg(3) {
                "radius" => Mode::Radius,
                m => Mode::Nearest(
                    m.strip_prefix("nearest-")
                        .and_then(|k| k.parse().ok())
                        .unwrap_or(128),
                ),
            };
            if arg(1) != "current" && make(arg(1)).is_none() {
                eprintln!("unknown index {}", arg(1));
                std::process::exit(2);
            }
            println!(
                "{}",
                run(
                    arg(1),
                    arg(2),
                    mode,
                    arg(4).parse().unwrap_or(1),
                    arg(5).parse().unwrap_or(300),
                    arg(6) == "bits"
                )
            );
        }
        "verify" => {
            for line in verify(arg(1), arg(2).parse().unwrap_or(20)) {
                println!("{line}");
            }
        }
        _ => {
            println!("indexes: {}", INDEXES.join(" "));
            println!(
                "scenarios: {}",
                SCENARIOS.iter().map(|s| s.name).collect::<Vec<_>>().join(" ")
            );
        }
    }
}
