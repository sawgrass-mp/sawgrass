//! bevy_ecs: the same six phases as systems in one schedule, chained in order, each split across bevy's
//! compute task pool with `par_iter_mut`. bevy's multi-threaded executor runs the schedule.

use std::sync::Mutex;

use bevy_ecs::prelude::*;
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};
use sawgrass_server::interest::{Point, SpatialGrid, VisibilityChanges, VisibleSet};

use crate::Model;
use crate::logic::{self, Input, Job, Params, PlayerState, Snapshot, Transfer, Vitals};

#[derive(Component)]
struct Idx(u32);
#[derive(Component)]
struct Pos(Point);
#[derive(Component)]
struct Vel(Point);
#[derive(Component)]
struct VitalsC(Vitals);
#[derive(Component)]
struct JobC(Job);
#[derive(Component)]
struct Wallet(i64);
#[derive(Component)]
struct Viol(u32);
#[derive(Component)]
struct Vis(VisibleSet);
#[derive(Component)]
struct Chg(VisibilityChanges);
#[derive(Component)]
struct Snap(Snapshot);

#[derive(Resource)]
struct Ctx {
    p: Params,
    tick: u64,
    inputs: Vec<Input>,
    entities: Vec<Entity>,
}

#[derive(Resource, Default)]
struct Grid(SpatialGrid);

#[derive(Resource, Default)]
struct Transfers {
    pending: Mutex<Vec<Transfer>>,
    applied: u64,
}

fn ingest(ctx: Res<Ctx>, mut q: Query<(&Idx, &mut Pos, &mut Vel, &mut Viol)>) {
    q.par_iter_mut().for_each(|(idx, mut pos, mut vel, mut viol)| {
        logic::ingest(
            &ctx.p,
            ctx.inputs[idx.0 as usize],
            &mut pos.0,
            &mut vel.0,
            &mut viol.0,
        );
    });
}

fn grid(mut grid: ResMut<Grid>, q: Query<(&Idx, &Pos)>) {
    for (idx, pos) in &q {
        logic::grid_upsert(&mut grid.0, idx.0, pos.0);
    }
}

fn visibility(ctx: Res<Ctx>, grid: Res<Grid>, mut q: Query<(&Idx, &Pos, &mut Vis, &mut Chg)>) {
    q.par_iter_mut().for_each(|(idx, pos, mut vis, mut chg)| {
        logic::visibility(&ctx.p, &grid.0, idx.0, pos.0, &mut vis.0, &mut chg.0);
    });
}

fn gameplay(
    ctx: Res<Ctx>,
    transfers: Res<Transfers>,
    positions: Query<&Pos>,
    mut q: Query<(&Idx, &Pos, &Vis, &mut VitalsC, &mut JobC, &mut Wallet)>,
) {
    let pos_of = |o: u32| positions.get(ctx.entities[o as usize]).expect("pos").0;
    q.par_iter_mut()
        .for_each(|(idx, pos, vis, mut vitals, mut job, mut wallet)| {
            let t = logic::gameplay(
                &ctx.p,
                ctx.tick,
                idx.0,
                pos.0,
                &vis.0,
                &mut vitals.0,
                &mut job.0,
                &mut wallet.0,
                pos_of,
            );
            if let Some(t) = t {
                transfers.pending.lock().expect("transfers").push(t);
            }
        });
}

fn apply(ctx: Res<Ctx>, mut transfers: ResMut<Transfers>, mut wallets: Query<&mut Wallet>) {
    let mut pending = std::mem::take(transfers.pending.get_mut().expect("transfers"));
    let wallets = std::cell::RefCell::new(&mut wallets);
    let n = logic::apply_transfers(
        &mut pending,
        |i| wallets.borrow().get(ctx.entities[i as usize]).expect("wallet").0,
        |i, v| {
            wallets
                .borrow_mut()
                .get_mut(ctx.entities[i as usize])
                .expect("wallet")
                .0 = v
        },
    );
    transfers.applied += u64::from(n);
    pending.clear();
    *transfers.pending.get_mut().expect("transfers") = pending;
}

fn snapshot(ctx: Res<Ctx>, positions: Query<&Pos>, mut q: Query<(&Pos, &Vis, &mut Snap)>) {
    let pos_of = |o: u32| positions.get(ctx.entities[o as usize]).expect("pos").0;
    q.par_iter_mut().for_each(|(pos, vis, mut snap)| {
        logic::snapshot(pos.0, &vis.0, &mut snap.0, pos_of);
    });
}

pub struct Bevy {
    world: World,
    schedule: Schedule,
}

impl Bevy {
    /// Sets the global compute pool to `threads`; call once per process.
    pub fn new(p: Params, players: Vec<PlayerState>, threads: usize, single_executor: bool) -> Self {
        ComputeTaskPool::get_or_init(|| TaskPoolBuilder::new().num_threads(threads).build());
        let mut world = World::new();
        let entities = players
            .into_iter()
            .enumerate()
            .map(|(i, st)| {
                world
                    .spawn((
                        Idx(i as u32),
                        Pos(st.pos),
                        Vel(st.vel),
                        VitalsC(st.vitals),
                        JobC(st.job),
                        Wallet(st.wallet),
                        Viol(st.violations),
                        Vis(st.vis),
                        Chg(st.changes),
                        Snap(st.snap),
                    ))
                    .id()
            })
            .collect();
        world.insert_resource(Ctx {
            p,
            tick: 0,
            inputs: Vec::new(),
            entities,
        });
        world.insert_resource(Grid::default());
        world.insert_resource(Transfers::default());
        let mut schedule = Schedule::default();
        schedule.add_systems((ingest, grid, visibility, gameplay, apply, snapshot).chain());
        if single_executor {
            // The systems are chained, so the multi-threaded executor has nothing to overlap; run them in order
            // on the calling thread and keep the parallelism inside each system's `par_iter_mut`.
            schedule.set_executor(bevy_ecs::schedule::SingleThreadedExecutor::new());
        }
        Self { world, schedule }
    }
}

impl Model for Bevy {
    fn prepare(&mut self, inputs: &[Input]) {
        let mut ctx = self.world.resource_mut::<Ctx>();
        ctx.inputs.clear();
        ctx.inputs.extend_from_slice(inputs);
    }

    fn tick(&mut self, _inputs: &[Input], tick: u64) {
        self.world.resource_mut::<Ctx>().tick = tick;
        self.schedule.run(&mut self.world);
    }

    fn checksum(&self) -> u64 {
        let ctx = self.world.resource::<Ctx>();
        ctx.entities.iter().fold(0, |h, &e| {
            let e = self.world.entity(e);
            logic::fold(
                h,
                e.get::<Pos>().expect("pos").0,
                e.get::<VitalsC>().expect("vitals").0,
                e.get::<JobC>().expect("job").0,
                e.get::<Wallet>().expect("wallet").0,
                e.get::<Viol>().expect("viol").0,
                &e.get::<Vis>().expect("vis").0,
                &e.get::<Snap>().expect("snap").0,
            )
        })
    }

    fn applied(&self) -> u64 {
        self.world.resource::<Transfers>().applied
    }
}
