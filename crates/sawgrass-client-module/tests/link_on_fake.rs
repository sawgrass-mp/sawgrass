//! The network core inside the module: snapshots from a made-up server become entities in the fake game, and the
//! local player's state goes out.

use std::collections::BTreeMap;

use sawgrass_client_module::assets::{AssetId, AssetTable};
use sawgrass_client_module::entity_pool::NetId;
use sawgrass_client_module::fake::FakeRuntime;
use sawgrass_client_module::game::{EntityKind, FakeGame, GameAsset, Pose};
use sawgrass_client_module::link::{InfoSource, NetLink};
use sawgrass_client_module::module::{Module, required_functions};
use sawgrass_client_module::runtime::{GameBuild, RuntimeKind};
use sawgrass_client_module::table::FunctionTable;
use sawgrass_netcode::{Client, ClientConfig};
use sawgrass_protocol::delta::{DeltaEncoder, fragments};
use sawgrass_protocol::entity::{EntityKind as NetKind, Info};
use sawgrass_protocol::{EntityState, PacketHeader, PacketKind, Position, Rotation, Velocity};

const BUILD_KEY: &str = "victory/1.0.0/00ff";
const LOCAL: u32 = 99;
const TICK_HZ: u16 = 20;
const FRAME_HZ: f32 = 60.0;

fn module() -> Module<FakeRuntime> {
    let build = GameBuild {
        runtime: RuntimeKind::Victory,
        title_version: "1.0.0".into(),
        image_digest: "00ff".into(),
    };
    let names = required_functions();
    let mut image = vec![0u8; 0x10 * names.len()];
    let mut table = format!("build = {BUILD_KEY}\n");
    for (i, name) in names.iter().enumerate() {
        image[0x10 * i..0x10 * i + 4].copy_from_slice(&[0x5A, i as u8, 0xA5, 0x3C]);
        table += &format!("{name} = 5A {i:02X} A5 3C\n");
    }
    let functions = FunctionTable::parse(&table).expect("functions");
    let assets = AssetTable::parse(&format!("build = {BUILD_KEY}\n5 = 0xCA\n")).expect("assets");
    let Ok(module) = Module::start(FakeRuntime::new(build, 0x4000, image), &functions, assets) else {
        panic!("module starts");
    };
    module
}

struct Infos(BTreeMap<NetId, Info>);

impl InfoSource for Infos {
    fn info(&self, id: NetId) -> Option<Info> {
        self.0.get(&id).copied()
    }
}

fn state(id: u32, x: f32) -> EntityState {
    EntityState {
        id,
        position: Position::from_metres([x, 0.0, 30.0]).expect("position"),
        velocity: Velocity::from_metres_per_second([10.0, 0.0, 0.0]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    }
}

/// Full snapshot datagrams for one tick.
fn snapshot(encoder: &mut DeltaEncoder, tick: u32, states: &[EntityState]) -> Vec<Vec<u8>> {
    let mut payload = Vec::new();
    encoder.encode(tick, states, None, &mut payload).expect("encode");
    fragments(&payload)
        .expect("fragments")
        .map(|(fragment, part)| {
            let mut d = vec![0; PacketHeader::SIZE];
            PacketHeader {
                kind: PacketKind::DeltaSnapshot,
                sequence: 0,
                tick,
            }
            .encode(&mut d)
            .expect("header");
            d.push(fragment);
            d.extend_from_slice(part);
            d
        })
        .collect()
}

#[test]
fn remote_entities_follow_the_snapshots_and_the_local_player_reports() {
    let mut module = module();
    let mut game = FakeGame::default();
    let me = game.create_natively(
        EntityKind::Ped,
        GameAsset(1),
        Pose {
            position: [0.0, 5.0, 30.0],
            heading: 0.0,
        },
    );
    game.local_player = Some(me);
    let infos = Infos(BTreeMap::from([(1, Info::new(NetKind::Vehicle, AssetId(5)))]));
    let mut link = NetLink::new(Client::new(LOCAL, TICK_HZ, ClientConfig::default()), infos);
    let (inbox, outbox) = (link.inbox(), link.outbox());
    let mut encoder = DeltaEncoder::default();
    let mut sent = 0;
    let mut seen_at = Vec::new();

    // Three seconds at 60 frames a second; the server sends entity 1 for the first two, then drops it.
    for frame in 0..180u32 {
        if frame % 3 == 0 {
            let tick = frame / 3;
            let x = tick as f32 * 0.5;
            // Ascending by id, as the server sends them.
            let mut states = Vec::new();
            if tick < 40 {
                states.push(state(1, x));
            }
            states.push(state(LOCAL, 0.0));
            for d in snapshot(&mut encoder, tick, &states) {
                inbox.push(d);
            }
        }
        module.frame(1.0 / FRAME_HZ, &mut game, &mut link).expect("frame");
        sent += outbox.take().len();
        if let Some(handle) = module.entities().handle(1) {
            let (kind, asset, pose) = game.entities[&handle];
            assert_eq!((kind, asset), (EntityKind::Vehicle, GameAsset(0xCA)));
            seen_at.push((frame, pose.position[0]));
        }
        assert!(
            module.entities().handle(LOCAL).is_none(),
            "the local player is never spawned as remote"
        );
    }

    assert!(
        seen_at.len() > 60,
        "entity 1 shown for most of the first two seconds, got {} frames",
        seen_at.len()
    );
    assert!(
        seen_at.windows(2).all(|w| w[1].1 >= w[0].1),
        "entity 1 only moves forward"
    );
    assert!(
        seen_at.last().map(|(_, x)| *x).unwrap_or_default() > 10.0,
        "entity 1 moved along its path"
    );
    assert!(
        module.entities().handle(1).is_none(),
        "entity 1 is removed once the server drops it"
    );
    assert!(sent > 0, "the local player's state is sent");
    assert_eq!(link.world_errors, 0);
    assert!(game.streaming_focus.is_empty(), "nothing left to stream around");
}
