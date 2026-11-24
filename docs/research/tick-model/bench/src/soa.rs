//! Plain arrays, one per field, indexed by player: on one thread, and with every phase split across players
//! with rayon.

use std::time::{Duration, Instant};

use rayon::prelude::*;
use sawgrass_server::interest::{Point, SpatialGrid, VisibilityChanges, VisibleSet};

use crate::Model;
use crate::logic::{self, Input, Job, Params, PlayerState, Snapshot, Transfer, Vitals};

/// Players per rayon task, so that splitting does not cost more than the work.
const MIN_LEN: usize = 32;

pub struct Soa {
    p: Params,
    pool: Option<rayon::ThreadPool>,
    pos: Vec<Point>,
    vel: Vec<Point>,
    vitals: Vec<Vitals>,
    job: Vec<Job>,
    wallet: Vec<i64>,
    violations: Vec<u32>,
    vis: Vec<VisibleSet>,
    changes: Vec<VisibilityChanges>,
    snap: Vec<Snapshot>,
    grid: SpatialGrid,
    transfers: Vec<Transfer>,
    applied: u64,
    /// Time spent in each of the six phases, summed over ticks.
    phases: [Duration; 6],
}

impl Soa {
    /// `threads` of `None` runs everything on the calling thread, without rayon.
    pub fn new(p: Params, players: Vec<PlayerState>, threads: Option<usize>) -> Self {
        let pool = threads.map(|n| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .build()
                .expect("rayon pool")
        });
        let mut s = Self {
            p,
            pool,
            pos: Vec::new(),
            vel: Vec::new(),
            vitals: Vec::new(),
            job: Vec::new(),
            wallet: Vec::new(),
            violations: Vec::new(),
            vis: Vec::new(),
            changes: Vec::new(),
            snap: Vec::new(),
            grid: SpatialGrid::default(),
            transfers: Vec::new(),
            applied: 0,
            phases: [Duration::ZERO; 6],
        };
        for st in players {
            s.pos.push(st.pos);
            s.vel.push(st.vel);
            s.vitals.push(st.vitals);
            s.job.push(st.job);
            s.wallet.push(st.wallet);
            s.violations.push(st.violations);
            s.vis.push(st.vis);
            s.changes.push(st.changes);
            s.snap.push(st.snap);
        }
        s
    }

    fn tick_serial(&mut self, inputs: &[Input], tick: u64) {
        let p = &self.p;
        let mut lap = Lap::new();
        for i in 0..self.pos.len() {
            logic::ingest(
                p,
                inputs[i],
                &mut self.pos[i],
                &mut self.vel[i],
                &mut self.violations[i],
            );
        }
        lap.mark(&mut self.phases[0]);
        for (i, &pos) in self.pos.iter().enumerate() {
            logic::grid_upsert(&mut self.grid, i as u32, pos);
        }
        lap.mark(&mut self.phases[1]);
        for i in 0..self.pos.len() {
            logic::visibility(
                p,
                &self.grid,
                i as u32,
                self.pos[i],
                &mut self.vis[i],
                &mut self.changes[i],
            );
        }
        lap.mark(&mut self.phases[2]);
        let pos = &self.pos;
        for i in 0..pos.len() {
            let t = logic::gameplay(
                p,
                tick,
                i as u32,
                pos[i],
                &self.vis[i],
                &mut self.vitals[i],
                &mut self.job[i],
                &mut self.wallet[i],
                |o| pos[o as usize],
            );
            self.transfers.extend(t);
        }
        lap.mark(&mut self.phases[3]);
        self.apply();
        lap.mark(&mut self.phases[4]);
        let pos = &self.pos;
        for i in 0..pos.len() {
            logic::snapshot(pos[i], &self.vis[i], &mut self.snap[i], |o| pos[o as usize]);
        }
        lap.mark(&mut self.phases[5]);
    }

    fn tick_parallel(&mut self, inputs: &[Input], tick: u64) {
        let pool = self.pool.take().expect("pool");
        let mut lap = Lap::new();
        pool.install(|| {
            let p = &self.p;
            (&mut self.pos, &mut self.vel, &mut self.violations, inputs)
                .into_par_iter()
                .with_min_len(MIN_LEN)
                .for_each(|(pos, vel, viol, &input)| logic::ingest(p, input, pos, vel, viol));
            lap.mark(&mut self.phases[0]);
            for (i, &pos) in self.pos.iter().enumerate() {
                logic::grid_upsert(&mut self.grid, i as u32, pos);
            }
            lap.mark(&mut self.phases[1]);
            let grid = &self.grid;
            (&self.pos, &mut self.vis, &mut self.changes)
                .into_par_iter()
                .with_min_len(MIN_LEN)
                .enumerate()
                .for_each(|(i, (&pos, vis, ch))| logic::visibility(p, grid, i as u32, pos, vis, ch));
            lap.mark(&mut self.phases[2]);
            let pos = &self.pos;
            let transfers: Vec<Transfer> = (&self.vis, &mut self.vitals, &mut self.job, &mut self.wallet)
                .into_par_iter()
                .with_min_len(MIN_LEN)
                .enumerate()
                .filter_map(|(i, (vis, vitals, job, wallet))| {
                    logic::gameplay(p, tick, i as u32, pos[i], vis, vitals, job, wallet, |o| {
                        pos[o as usize]
                    })
                })
                .collect();
            self.transfers.extend(transfers);
            lap.mark(&mut self.phases[3]);
            self.apply();
            lap.mark(&mut self.phases[4]);
            let pos = &self.pos;
            (&self.vis, &mut self.snap)
                .into_par_iter()
                .with_min_len(MIN_LEN)
                .enumerate()
                .for_each(|(i, (vis, snap))| logic::snapshot(pos[i], vis, snap, |o| pos[o as usize]));
            lap.mark(&mut self.phases[5]);
        });
        self.pool = Some(pool);
    }

    fn apply(&mut self) {
        let wallet = std::cell::RefCell::new(&mut self.wallet);
        self.applied += u64::from(logic::apply_transfers(
            &mut self.transfers,
            |i| wallet.borrow()[i as usize],
            |i, v| wallet.borrow_mut()[i as usize] = v,
        ));
        self.transfers.clear();
    }
}

impl Model for Soa {
    fn tick(&mut self, inputs: &[Input], tick: u64) {
        if self.pool.is_some() {
            self.tick_parallel(inputs, tick);
        } else {
            self.tick_serial(inputs, tick);
        }
    }

    fn checksum(&self) -> u64 {
        (0..self.pos.len()).fold(0, |h, i| {
            logic::fold(
                h,
                self.pos[i],
                self.vitals[i],
                self.job[i],
                self.wallet[i],
                self.violations[i],
                &self.vis[i],
                &self.snap[i],
            )
        })
    }

    fn applied(&self) -> u64 {
        self.applied
    }

    fn phases(&self) -> Option<[Duration; 6]> {
        Some(self.phases)
    }

    fn sizes(&self) -> Option<(f64, f64)> {
        let n = self.pos.len() as f64;
        let vis: usize = self.vis.iter().map(VisibleSet::len).sum();
        let snap: usize = self.snap.iter().map(|s| s.entries.len()).sum();
        Some((vis as f64 / n, snap as f64 / n))
    }
}

/// Adds the time since the last mark to a phase total.
struct Lap(Instant);

impl Lap {
    fn new() -> Self {
        Self(Instant::now())
    }

    fn mark(&mut self, total: &mut Duration) {
        let now = Instant::now();
        *total += now - self.0;
        self.0 = now;
    }
}
