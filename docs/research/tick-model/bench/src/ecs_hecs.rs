//! hecs: the project's current `TickLoop` (one thread, systems in order) and the same world with every system
//! split across threads through hecs batched queries and rayon.

use std::sync::{Arc, Mutex};

use rayon::prelude::*;
use sawgrass_server::hecs::{Entity, World};
use sawgrass_server::interest::{Point, SpatialGrid, VisibilityChanges, VisibleSet};
use sawgrass_server::{TickConfig, TickLoop};

use crate::Model;
use crate::logic::{self, Input, Job, Params, PlayerState, Snapshot, Transfer, Vitals};

pub struct Idx(pub u32);
pub struct Pos(pub Point);
pub struct Vel(pub Point);
pub struct Wallet(pub i64);
pub struct Viol(pub u32);
pub struct Vis(pub VisibleSet);
pub struct Chg(pub VisibilityChanges);
pub struct Snap(pub Snapshot);

/// Entities per batch handed to a rayon task.
const BATCH: u32 = 64;

fn spawn(world: &mut World, players: Vec<PlayerState>) -> Vec<Entity> {
    players
        .into_iter()
        .enumerate()
        .map(|(i, st)| {
            world.spawn((
                Idx(i as u32),
                Pos(st.pos),
                Vel(st.vel),
                st.vitals,
                st.job,
                Wallet(st.wallet),
                Viol(st.violations),
                Vis(st.vis),
                Chg(st.changes),
                Snap(st.snap),
            ))
        })
        .collect()
}

fn checksum(world: &World, entities: &[Entity]) -> u64 {
    entities.iter().fold(0, |h, &e| {
        let mut q = world.query_one::<(&Pos, &Vitals, &Job, &Wallet, &Viol, &Vis, &Snap)>(e);
        let (pos, vitals, job, wallet, viol, vis, snap) = q.get().expect("components");
        logic::fold(h, pos.0, *vitals, *job, wallet.0, viol.0, &vis.0, &snap.0)
    })
}

fn apply(world: &mut World, entities: &[Entity], transfers: &mut Vec<Transfer>) -> u32 {
    let world = std::cell::RefCell::new(world);
    let n = logic::apply_transfers(
        transfers,
        |i| {
            world
                .borrow()
                .get::<&Wallet>(entities[i as usize])
                .expect("wallet")
                .0
        },
        |i, v| {
            world
                .borrow_mut()
                .get::<&mut Wallet>(entities[i as usize])
                .expect("wallet")
                .0 = v
        },
    );
    transfers.clear();
    n
}

/// The state the current loop's systems share outside the world, as SRV-1 leaves it to the caller.
#[derive(Default)]
struct Shared {
    inputs: Vec<Input>,
    grid: SpatialGrid,
    transfers: Vec<Transfer>,
    applied: u64,
}

/// The current loop: `sawgrass_server::TickLoop` running six systems in order on one thread.
pub struct Current {
    tick_loop: TickLoop,
    shared: Arc<Mutex<Shared>>,
    entities: Arc<Vec<Entity>>,
}

impl Current {
    pub fn new(p: Params, rate_hz: u32, players: Vec<PlayerState>) -> Self {
        let mut tick_loop = TickLoop::new(TickConfig::new(rate_hz).expect("rate"));
        let entities = Arc::new(spawn(tick_loop.world_mut(), players));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let s = shared.clone();
        tick_loop.add_system("ingest", move |world, _| {
            let s = s.lock().expect("shared");
            for (idx, pos, vel, viol) in world.query_mut::<(&Idx, &mut Pos, &mut Vel, &mut Viol)>() {
                logic::ingest(&p, s.inputs[idx.0 as usize], &mut pos.0, &mut vel.0, &mut viol.0);
            }
        });
        let s = shared.clone();
        tick_loop.add_system("grid", move |world, _| {
            let mut s = s.lock().expect("shared");
            for (idx, pos) in world.query_mut::<(&Idx, &Pos)>() {
                logic::grid_upsert(&mut s.grid, idx.0, pos.0);
            }
        });
        let s = shared.clone();
        tick_loop.add_system("visibility", move |world, _| {
            let s = s.lock().expect("shared");
            for (idx, pos, vis, chg) in world.query_mut::<(&Idx, &Pos, &mut Vis, &mut Chg)>() {
                logic::visibility(&p, &s.grid, idx.0, pos.0, &mut vis.0, &mut chg.0);
            }
        });
        let (s, ents) = (shared.clone(), entities.clone());
        tick_loop.add_system("gameplay", move |world, info| {
            let mut s = s.lock().expect("shared");
            let positions = world.view::<&Pos>();
            let pos_of = |o: u32| positions.get(ents[o as usize]).expect("pos").0;
            let mut q = world.query::<(&Idx, &Pos, &Vis, &mut Vitals, &mut Job, &mut Wallet)>();
            for (idx, pos, vis, vitals, job, wallet) in q.iter() {
                let t = logic::gameplay(
                    &p,
                    info.tick,
                    idx.0,
                    pos.0,
                    &vis.0,
                    vitals,
                    job,
                    &mut wallet.0,
                    pos_of,
                );
                s.transfers.extend(t);
            }
        });
        let (s, ents) = (shared.clone(), entities.clone());
        tick_loop.add_system("transfers", move |world, _| {
            let mut s = s.lock().expect("shared");
            let mut transfers = std::mem::take(&mut s.transfers);
            s.applied += u64::from(apply(world, &ents, &mut transfers));
            s.transfers = transfers;
        });
        let ents = entities.clone();
        tick_loop.add_system("snapshot", move |world, _| {
            let positions = world.view::<&Pos>();
            let pos_of = |o: u32| positions.get(ents[o as usize]).expect("pos").0;
            let mut q = world.query::<(&Pos, &Vis, &mut Snap)>();
            for (pos, vis, snap) in q.iter() {
                logic::snapshot(pos.0, &vis.0, &mut snap.0, pos_of);
            }
        });
        Self {
            tick_loop,
            shared,
            entities,
        }
    }
}

impl Model for Current {
    fn prepare(&mut self, inputs: &[Input]) {
        let mut s = self.shared.lock().expect("shared");
        s.inputs.clear();
        s.inputs.extend_from_slice(inputs);
    }

    fn tick(&mut self, _inputs: &[Input], tick: u64) {
        debug_assert_eq!(tick, self.tick_loop.tick());
        self.tick_loop.run_tick();
    }

    fn checksum(&self) -> u64 {
        checksum(self.tick_loop.world(), &self.entities)
    }

    fn applied(&self) -> u64 {
        self.shared.lock().expect("shared").applied
    }
}

/// hecs with each system split across threads: `iter_batched` hands batches of entities to rayon.
pub struct HecsRayon {
    p: Params,
    pool: rayon::ThreadPool,
    world: World,
    entities: Vec<Entity>,
    grid: SpatialGrid,
    transfers: Vec<Transfer>,
    applied: u64,
}

impl HecsRayon {
    pub fn new(p: Params, players: Vec<PlayerState>, threads: usize) -> Self {
        let mut world = World::new();
        let entities = spawn(&mut world, players);
        Self {
            p,
            pool: rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("rayon pool"),
            world,
            entities,
            grid: SpatialGrid::default(),
            transfers: Vec::new(),
            applied: 0,
        }
    }
}

impl Model for HecsRayon {
    fn tick(&mut self, inputs: &[Input], tick: u64) {
        let p = &self.p;
        let (world, ents, grid) = (&mut self.world, &self.entities, &mut self.grid);
        let transfers: Vec<Transfer> = self.pool.install(|| {
            world
                .query::<(&Idx, &mut Pos, &mut Vel, &mut Viol)>()
                .iter_batched(BATCH)
                .par_bridge()
                .for_each(|batch| {
                    for (idx, pos, vel, viol) in batch {
                        logic::ingest(p, inputs[idx.0 as usize], &mut pos.0, &mut vel.0, &mut viol.0);
                    }
                });
            for (idx, pos) in world.query_mut::<(&Idx, &Pos)>() {
                logic::grid_upsert(grid, idx.0, pos.0);
            }
            let grid = &*grid;
            world
                .query::<(&Idx, &Pos, &mut Vis, &mut Chg)>()
                .iter_batched(BATCH)
                .par_bridge()
                .for_each(|batch| {
                    for (idx, pos, vis, chg) in batch {
                        logic::visibility(p, grid, idx.0, pos.0, &mut vis.0, &mut chg.0);
                    }
                });
            let positions = world.view::<&Pos>();
            let pos_of = |o: u32| positions.get(ents[o as usize]).expect("pos").0;
            world
                .query::<(&Idx, &Pos, &Vis, &mut Vitals, &mut Job, &mut Wallet)>()
                .iter_batched(BATCH)
                .par_bridge()
                .flat_map_iter(|batch| {
                    batch
                        .filter_map(|(idx, pos, vis, vitals, job, wallet)| {
                            logic::gameplay(p, tick, idx.0, pos.0, &vis.0, vitals, job, &mut wallet.0, pos_of)
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        });
        self.transfers.extend(transfers);
        self.applied += u64::from(apply(&mut self.world, &self.entities, &mut self.transfers));
        let (world, ents) = (&self.world, &self.entities);
        self.pool.install(|| {
            let positions = world.view::<&Pos>();
            let pos_of = |o: u32| positions.get(ents[o as usize]).expect("pos").0;
            world
                .query::<(&Pos, &Vis, &mut Snap)>()
                .iter_batched(BATCH)
                .par_bridge()
                .for_each(|batch| {
                    for (pos, vis, snap) in batch {
                        logic::snapshot(pos.0, &vis.0, &mut snap.0, pos_of);
                    }
                });
        });
    }

    fn checksum(&self) -> u64 {
        checksum(&self.world, &self.entities)
    }

    fn applied(&self) -> u64 {
        self.applied
    }
}
