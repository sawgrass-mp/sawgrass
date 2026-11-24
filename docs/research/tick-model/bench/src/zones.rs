//! Zones in parallel: the map is cut into square zones, each zone owns the players inside it, and each phase
//! runs one rayon task per zone. A player who crosses a border is handed to the next zone between phases.
//!
//! This is zoning inside one process: every zone still reads one shared grid and one array of positions, so
//! there is no border protocol. It measures how evenly zones share the work, not the cost of separate processes.

use rayon::prelude::*;
use sawgrass_server::interest::{Point, SpatialGrid};

use crate::Model;
use crate::logic::{self, Input, Params, PlayerState, Transfer};

pub const ZONE_SIZE: f32 = 1000.0;

struct Zone {
    ids: Vec<u32>,
    players: Vec<PlayerState>,
}

pub struct Zones {
    p: Params,
    pool: rayon::ThreadPool,
    per_side: usize,
    zones: Vec<Zone>,
    /// Every player's position by id, rebuilt after movement: what other zones read.
    mirror: Vec<Point>,
    /// Zone and slot of every player by id, for the serial transfer phase.
    loc: Vec<(u32, u32)>,
    grid: SpatialGrid,
    transfers: Vec<Transfer>,
    applied: u64,
}

impl Zones {
    pub fn new(p: Params, players: Vec<PlayerState>, threads: usize) -> Self {
        let per_side = (p.area / ZONE_SIZE).ceil() as usize;
        let mut z = Self {
            p,
            pool: rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("rayon pool"),
            per_side,
            zones: (0..per_side * per_side)
                .map(|_| Zone {
                    ids: Vec::new(),
                    players: Vec::new(),
                })
                .collect(),
            mirror: vec![Point::default(); players.len()],
            loc: vec![(0, 0); players.len()],
            grid: SpatialGrid::default(),
            transfers: Vec::new(),
            applied: 0,
        };
        for (i, st) in players.into_iter().enumerate() {
            let zi = z.zone_of(st.pos);
            z.zones[zi].ids.push(i as u32);
            z.zones[zi].players.push(st);
        }
        z
    }

    fn zone_of(&self, pos: Point) -> usize {
        let side = self.per_side;
        let cx = ((pos.x / ZONE_SIZE) as usize).min(side - 1);
        let cy = ((pos.y / ZONE_SIZE) as usize).min(side - 1);
        cy * side + cx
    }
}

impl Model for Zones {
    fn tick(&mut self, inputs: &[Input], tick: u64) {
        let p = self.p;
        self.pool.install(|| {
            self.zones.par_iter_mut().for_each(|z| {
                for (id, st) in z.ids.iter().zip(&mut z.players) {
                    logic::ingest(
                        &p,
                        inputs[*id as usize],
                        &mut st.pos,
                        &mut st.vel,
                        &mut st.violations,
                    );
                }
            });
        });
        // Hand-off: move every player whose position left its zone.
        let mut moving = Vec::new();
        for zi in 0..self.zones.len() {
            let mut k = 0;
            while k < self.zones[zi].players.len() {
                let to = self.zone_of(self.zones[zi].players[k].pos);
                if to != zi {
                    let id = self.zones[zi].ids.swap_remove(k);
                    let st = self.zones[zi].players.swap_remove(k);
                    moving.push((to, id, st));
                } else {
                    k += 1;
                }
            }
        }
        for (to, id, st) in moving {
            self.zones[to].ids.push(id);
            self.zones[to].players.push(st);
        }
        for (zi, z) in self.zones.iter().enumerate() {
            for (slot, (&id, st)) in z.ids.iter().zip(&z.players).enumerate() {
                self.mirror[id as usize] = st.pos;
                self.loc[id as usize] = (zi as u32, slot as u32);
                logic::grid_upsert(&mut self.grid, id, st.pos);
            }
        }
        let (grid, mirror) = (&self.grid, &self.mirror);
        let transfers: Vec<Transfer> = self.pool.install(|| {
            self.zones.par_iter_mut().for_each(|z| {
                for (&id, st) in z.ids.iter().zip(&mut z.players) {
                    logic::visibility(&p, grid, id, st.pos, &mut st.vis, &mut st.changes);
                }
            });
            self.zones
                .par_iter_mut()
                .flat_map_iter(|z| {
                    let mut out = Vec::new();
                    for (&id, st) in z.ids.iter().zip(&mut z.players) {
                        out.extend(logic::gameplay(
                            &p,
                            tick,
                            id,
                            st.pos,
                            &st.vis,
                            &mut st.vitals,
                            &mut st.job,
                            &mut st.wallet,
                            |o| mirror[o as usize],
                        ));
                    }
                    out
                })
                .collect()
        });
        self.transfers.extend(transfers);
        let (zones, loc) = (std::cell::RefCell::new(&mut self.zones), &self.loc);
        self.applied += u64::from(logic::apply_transfers(
            &mut self.transfers,
            |i| {
                let (z, s) = loc[i as usize];
                zones.borrow()[z as usize].players[s as usize].wallet
            },
            |i, v| {
                let (z, s) = loc[i as usize];
                zones.borrow_mut()[z as usize].players[s as usize].wallet = v;
            },
        ));
        self.transfers.clear();
        let mirror = &self.mirror;
        self.pool.install(|| {
            self.zones.par_iter_mut().for_each(|z| {
                for st in &mut z.players {
                    logic::snapshot(st.pos, &st.vis, &mut st.snap, |o| mirror[o as usize]);
                }
            });
        });
    }

    fn checksum(&self) -> u64 {
        let mut all: Vec<(u32, &PlayerState)> = self
            .zones
            .iter()
            .flat_map(|z| z.ids.iter().copied().zip(&z.players))
            .collect();
        all.sort_unstable_by_key(|e| e.0);
        all.iter().fold(0, |h, (_, st)| {
            logic::fold(
                h,
                st.pos,
                st.vitals,
                st.job,
                st.wallet,
                st.violations,
                &st.vis,
                &st.snap,
            )
        })
    }

    fn applied(&self) -> u64 {
        self.applied
    }
}
