//! The world server's tick with each allocator: `sawgrass-server`'s `TickLoop` at 20 Hz, its spatial grid and
//! visible sets, and the allocations a real tick makes around them.
//!
//! Each tick: every entity moves (hecs query); some non-player entities despawn and new ones spawn, with a name and
//! an inventory of varying size; the grid is updated and every player's visible set recomputed; every player gets
//! a snapshot freshly encoded with `sawgrass-protocol` (up to 150 visible entities) and a few players send chat
//! events. Snapshots and events are handed to two "gateway" threads over channels, which drop them there, so memory
//! allocated on the tick thread is freed on another, as it will be when the server hands packets to gateways.

use std::ops::ControlFlow;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sawgrass_protocol::{EntityState, PacketHeader, PacketKind, Position, Rotation, Velocity};
use sawgrass_server::hecs::{self, World};
use sawgrass_server::interest::{
    EntityId, Point, SpatialGrid, VisibilityChanges, VisibilityConfig, VisibleSet,
};
use sawgrass_server::{TickConfig, TickLoop};

use crate::common::*;

/// Edge of the square everything moves in: the IM-1 "city" scenario.
const AREA: f32 = 3000.0;
/// Entities that are not players (vehicles, NPCs, dropped items).
const OTHERS: u32 = 4096;
/// Non-player entities replaced every tick.
const CHURN: u32 = 40;
/// Chat or script events per tick.
const EVENTS: u32 = 200;
/// Most entities in one snapshot (architecture.md: about 100 to 150 by priority).
const SNAPSHOT_ENTITIES: usize = 150;

struct Pos(Point);
struct Vel(Point);
struct Player(u32);
struct Id(u32);
#[allow(dead_code)]
struct Name(String);
#[allow(dead_code)]
struct Inventory(Vec<u32>);

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn spawn_other(world: &mut World, rng: &mut Rng, id: u32) -> hecs::Entity {
    let speed = if id.is_multiple_of(2) { 30.0 } else { 2.0 } * rng.unit();
    let angle = rng.unit() * std::f32::consts::TAU;
    let items = (rng.next() % 32) as usize;
    world.spawn((
        Id(id),
        Pos(Point::new(rng.unit() * AREA, rng.unit() * AREA)),
        Vel(Point::new(speed * angle.cos(), speed * angle.sin())),
        Name(format!("entity-{id}")),
        Inventory((0..items as u32).collect()),
    ))
}

/// Runs `ticks` measured ticks after `warmup` ticks with `players` players, and prints the tick statistics.
pub fn tick(players: u32, warmup: u64, ticks: u64) {
    let started = Instant::now();
    let cpu0 = process_cpu();
    let (tx_a, rx_a) = mpsc::channel::<Vec<Vec<u8>>>();
    let (tx_b, rx_b) = mpsc::channel::<Vec<Vec<u8>>>();
    let gateways: Vec<_> = [rx_a, rx_b]
        .into_iter()
        .map(|rx| {
            std::thread::spawn(move || {
                let mut bytes = 0u64;
                while let Ok(packets) = rx.recv() {
                    bytes += packets.iter().map(|p| p.len() as u64).sum::<u64>();
                }
                bytes
            })
        })
        .collect();

    let config = TickConfig::default().with_window_secs((ticks / 20).max(1) as u32);
    let mut tick_loop = TickLoop::new(config);
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let world = tick_loop.world_mut();
    for p in 0..players {
        let speed = if p % 4 == 0 { 40.0 } else { 7.0 } * rng.unit();
        let angle = rng.unit() * std::f32::consts::TAU;
        world.spawn((
            Id(p),
            Player(p),
            Pos(Point::new(rng.unit() * AREA, rng.unit() * AREA)),
            Vel(Point::new(speed * angle.cos(), speed * angle.sin())),
        ));
    }
    let mut others: Vec<hecs::Entity> = (0..OTHERS)
        .map(|i| spawn_other(world, &mut rng, players + i))
        .collect();
    let mut next_id = players + OTHERS;

    tick_loop.add_system("movement", |world, info| {
        let dt = info.dt.as_secs_f32();
        for (pos, vel) in world.query_mut::<(&mut Pos, &mut Vel)>() {
            pos.0.x += vel.0.x * dt;
            pos.0.y += vel.0.y * dt;
            if !(0.0..=AREA).contains(&pos.0.x) {
                vel.0.x = -vel.0.x;
                pos.0.x = pos.0.x.clamp(0.0, AREA);
            }
            if !(0.0..=AREA).contains(&pos.0.y) {
                vel.0.y = -vel.0.y;
                pos.0.y = pos.0.y.clamp(0.0, AREA);
            }
        }
    });

    let mut churn_rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut gone: Vec<u32> = Vec::new();
    let (gone_tx, gone_rx) = mpsc::channel::<Vec<u32>>();
    tick_loop.add_system("churn", move |world, _| {
        gone.clear();
        for _ in 0..CHURN {
            let slot = (churn_rng.next() % others.len() as u64) as usize;
            if let Ok(id) = world.get::<&Id>(others[slot]) {
                gone.push(id.0);
            }
            let _ = world.despawn(others[slot]);
            others[slot] = spawn_other(world, &mut churn_rng, next_id);
            next_id += 1;
        }
        let _ = gone_tx.send(gone.clone());
    });

    let visibility = VisibilityConfig::default();
    let mut grid = SpatialGrid::default();
    let mut sets: Vec<VisibleSet> = (0..players).map(|_| VisibleSet::new()).collect();
    let mut changes = VisibilityChanges::default();
    let mut viewers: Vec<(u32, Point)> = Vec::with_capacity(players as usize);
    let mut event_rng = Rng(0xD1B5_4A32_D192_ED03);
    let gateway_tx = [tx_a, tx_b];
    // Encoded state by entity id; ids only grow, so a dense vector indexed by id.
    let mut encoded: Vec<[u8; EntityState::SIZE]> = Vec::new();
    tick_loop.add_system("interest-and-snapshots", move |world, info| {
        for ids in gone_rx.try_iter() {
            for id in ids {
                grid.remove(EntityId(id));
            }
        }
        viewers.clear();
        for (id, pos, vel, player) in world.query_mut::<(&Id, &Pos, &Vel, Option<&Player>)>() {
            grid.upsert(EntityId(id.0), pos.0);
            // Each entity is quantized and encoded once a tick; snapshots copy the bytes.
            let state = EntityState {
                id: id.0,
                position: Position::from_metres([pos.0.x, pos.0.y, 30.0]).expect("in range"),
                velocity: Velocity::from_metres_per_second([vel.0.x, vel.0.y, 0.0]).expect("in range"),
                rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("unit"),
            };
            let mut bytes = [0u8; EntityState::SIZE];
            state.encode(&mut bytes).expect("state");
            if encoded.len() <= id.0 as usize {
                encoded.resize(id.0 as usize + 1, [0; EntityState::SIZE]);
            }
            encoded[id.0 as usize] = bytes;
            if let Some(p) = player {
                viewers.push((p.0, pos.0));
            }
        }
        let mut outbox: [Vec<Vec<u8>>; 2] = Default::default();
        for &(p, at) in &viewers {
            let set = &mut sets[p as usize];
            set.update(&grid, Some(EntityId(p)), at, &visibility, &mut changes);
            let members = &set.members()[..set.len().min(SNAPSHOT_ENTITIES)];
            let mut packet = Vec::with_capacity(PacketHeader::SIZE + members.len() * EntityState::SIZE);
            let header = PacketHeader {
                kind: PacketKind::Snapshot,
                sequence: info.tick as u16,
                tick: info.tick as u32,
            };
            let mut head = [0u8; PacketHeader::SIZE];
            header.encode(&mut head).expect("header");
            packet.extend_from_slice(&head);
            for id in members {
                if let Some(bytes) = encoded.get(id.0 as usize) {
                    packet.extend_from_slice(bytes);
                }
            }
            outbox[p as usize % 2].push(packet);
        }
        for e in 0..EVENTS {
            let from = event_rng.next() % u64::from(players);
            let text = format!(
                "player {from} says hello to everyone nearby at tick {} ({e})",
                info.tick
            );
            outbox[e as usize % 2].push(text.into_bytes());
        }
        // One hand-off per gateway per tick; a send per packet would cost a thread wake-up each.
        for (tx, out) in gateway_tx.iter().zip(&mut outbox) {
            let _ = tx.send(std::mem::take(out));
        }
    });

    let total = warmup + ticks;
    tick_loop.run(|tl| {
        if tl.tick() >= total {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    let stats = tick_loop.metrics().stats();
    let rss = process_rss();
    let entities = tick_loop.world().len();
    let cpu = (process_cpu() - cpu0).as_secs_f64() / started.elapsed().as_secs_f64();
    drop(tick_loop);
    let bytes: u64 = gateways.into_iter().map(|g| g.join().unwrap_or(0)).sum();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "tick allocator={} players={players} entities={entities} ticks={} p50_ms={:.3} p99_ms={:.3} max_ms={:.3} \
         overruns={} rss_mb={:.1} cpu_cores={cpu:.2} handed_off_mb={:.0}",
        allocator(),
        stats.window,
        ms(stats.p50),
        ms(stats.p99),
        ms(stats.max),
        stats.overruns,
        rss as f64 / 1048576.0,
        bytes as f64 / 1048576.0,
    );
    flush();
}
