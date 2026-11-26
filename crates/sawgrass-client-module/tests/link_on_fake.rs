//! The network core inside the module: snapshots from a made-up server become entities in the fake game, and the
//! local player's state goes out.

use sawgrass_client_module::assets::{AssetId, AssetTable};
use sawgrass_client_module::fake::FakeRuntime;
use sawgrass_client_module::game::{DoorState, EntityKind, FakeGame, GameApi, GameAsset, Pose, WeatherState};
use sawgrass_client_module::link::{MAX_QUEUED, NetLink};
use sawgrass_client_module::module::{Module, all_functions};
use sawgrass_client_module::runtime::{GameBuild, RuntimeKind};
use sawgrass_client_module::table::FunctionTable;
use sawgrass_netcode::{Client, ClientConfig};
use sawgrass_protocol::delta::{DeltaEncoder, fragments};
use sawgrass_protocol::entity::{EntityKind as NetKind, Info};
use sawgrass_protocol::game::{ClientState, OwnedState, ServerEvent};
use sawgrass_protocol::world::{Door, Environment};
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
    let names = all_functions();
    let mut image = vec![0u8; 0x10 * names.len()];
    let mut table = format!("build = {BUILD_KEY}\n");
    for (i, name) in names.iter().enumerate() {
        image[0x10 * i..0x10 * i + 4].copy_from_slice(&[0x5A, i as u8, 0xA5, 0x3C]);
        table += &format!("{name} = 5A {i:02X} A5 3C\n");
    }
    let functions = FunctionTable::parse(&table).expect("functions");
    let assets = AssetTable::parse(&format!(
        "build = {BUILD_KEY}\n5 = 0xCA\n6 = 0xCB\n7 = 0xD0\n8 = 0xD1\n"
    ))
    .expect("assets");
    let Ok(module) = Module::start(FakeRuntime::new(build, 0x4000, image), &functions, assets) else {
        panic!("module starts");
    };
    module
}

fn state(id: u32, x: f32) -> EntityState {
    EntityState {
        id,
        position: Position::from_metres([x, 0.0, 30.0]).expect("position"),
        velocity: Velocity::from_metres_per_second([10.0, 0.0, 0.0]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    }
}

/// The infos the tests' server sends: entity 1 is a vehicle with model 5, the rest are players.
fn infos(states: &[EntityState]) -> Vec<Info> {
    states
        .iter()
        .map(|s| {
            if s.id == 1 {
                Info::new(NetKind::Vehicle, AssetId(5))
            } else {
                Info::PLAYER
            }
        })
        .collect()
}

/// Full snapshot datagrams for one tick.
fn snapshot(encoder: &mut DeltaEncoder, tick: u32, states: &[EntityState]) -> Vec<Vec<u8>> {
    snapshot_with(encoder, tick, states, &infos(states))
}

fn snapshot_with(
    encoder: &mut DeltaEncoder,
    tick: u32,
    states: &[EntityState],
    infos: &[Info],
) -> Vec<Vec<u8>> {
    let mut payload = Vec::new();
    encoder
        .encode_with_info(tick, states, infos, None, &[], &mut payload)
        .expect("encode");
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
    let mut link = NetLink::new(Client::new(LOCAL, TICK_HZ, ClientConfig::default()));
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

#[test]
fn an_owned_entity_is_left_to_the_game_and_measured() {
    let mut module = module();
    let mut game = FakeGame::default();
    let mut link = NetLink::new(Client::new(LOCAL, TICK_HZ, ClientConfig::default()));
    let inbox = link.inbox();
    let mut encoder = DeltaEncoder::default();
    let run = |frames: std::ops::Range<u32>,
               module: &mut Module<FakeRuntime>,
               game: &mut FakeGame,
               link: &mut NetLink,
               encoder: &mut DeltaEncoder| {
        for frame in frames {
            if frame % 3 == 0 {
                let tick = frame / 3;
                for d in snapshot(encoder, tick, &[state(1, tick as f32 * 0.5), state(LOCAL, 0.0)]) {
                    inbox.push(d);
                }
            }
            module.frame(1.0 / FRAME_HZ, game, link).expect("frame");
        }
    };
    run(0..60, &mut module, &mut game, &mut link, &mut encoder);
    let handle = module.entities().handle(1).expect("shown");

    link.set_ownership(1, 7, true);
    // The game's physics put the car somewhere else; the link must not pull it back to the snapshots.
    let parked = Pose {
        position: [-40.0, 3.0, 30.0],
        heading: 1.0,
    };
    game.set_pose(handle, parked).expect("moved by the game");
    run(60..90, &mut module, &mut game, &mut link, &mut encoder);
    assert_eq!(game.entities[&handle].2, parked);
    let owned = link.owned()[&1];
    assert_eq!(owned.epoch, 7);
    assert_eq!(owned.pose.map(|p| p.position), Some(parked.position));

    // The server takes it back, and the snapshots move it again.
    link.set_ownership(1, 7, false);
    run(90..120, &mut module, &mut game, &mut link, &mut encoder);
    assert!(link.owned().is_empty());
    assert_ne!(game.entities[&handle].2, parked);
}

/// The owned states in the newest datagram the link sent, emptying the outbox.
fn last_owned(link: &NetLink) -> Vec<OwnedState> {
    let sent = link.outbox().take();
    let last = sent.last().expect("a datagram went out");
    let mut owned = Vec::new();
    let (_, ped) = ClientState::decode_with_owned(&last[PacketHeader::SIZE..], &mut owned).expect("decodes");
    assert!(ped.is_some(), "the player's ped block goes with its state");
    owned
}

#[test]
fn owned_entities_go_out_with_the_player_state() {
    let mut module = module();
    let mut game = FakeGame::default();
    let me = game.create_natively(
        EntityKind::Ped,
        GameAsset(1),
        Pose {
            position: [-38.0, 3.0, 30.0],
            heading: 0.0,
        },
    );
    game.local_player = Some(me);
    let mut link = NetLink::new(Client::new(LOCAL, TICK_HZ, ClientConfig::default()));
    let inbox = link.inbox();
    let mut encoder = DeltaEncoder::default();
    let car = Info::with_fields(NetKind::Vehicle, AssetId(5), &[3, 4]).expect("info");
    let mut run = |frames: std::ops::Range<u32>,
                   module: &mut Module<FakeRuntime>,
                   game: &mut FakeGame,
                   link: &mut NetLink| {
        for frame in frames {
            if frame % 3 == 0 {
                let tick = frame / 3;
                let states = [state(1, tick as f32 * 0.5), state(LOCAL, 0.0)];
                for d in snapshot_with(&mut encoder, tick, &states, &[car, Info::PLAYER]) {
                    inbox.push(d);
                }
            }
            module.frame(1.0 / FRAME_HZ, game, link).expect("frame");
        }
    };
    run(0..60, &mut module, &mut game, &mut link);
    assert!(last_owned(&link).is_empty());
    link.set_ownership(1, 7, true);
    let handle = module.entities().handle(1).expect("shown");
    let parked = Pose {
        position: [-40.0, 3.0, 30.0],
        heading: 1.0,
    };
    game.set_pose(handle, parked).expect("moved by the game");
    run(60..90, &mut module, &mut game, &mut link);
    let owned = last_owned(&link);
    assert_eq!(owned.len(), 1);
    assert_eq!((owned[0].id, owned[0].epoch), (1, 7));
    // The server's own block goes back until the game's is read, so the merge keeps it.
    assert_eq!(owned[0].fields(), car.fields());
    let at = owned[0].state.position.to_metres();
    for (a, b) in at.iter().zip(parked.position) {
        assert!(
            (a - b).abs() < 0.01,
            "sent {at:?}, the game has {:?}",
            parked.position
        );
    }

    // The server takes it back: the player's state goes out alone.
    link.set_ownership(1, 7, false);
    run(90..120, &mut module, &mut game, &mut link);
    assert!(last_owned(&link).is_empty());
}

#[test]
fn block_changes_keep_the_entity_and_a_new_model_respawns_it() {
    let mut module = module();
    let mut game = FakeGame::default();
    let mut link = NetLink::new(Client::new(LOCAL, TICK_HZ, ClientConfig::default()));
    let inbox = link.inbox();
    let mut encoder = DeltaEncoder::default();
    let car = |model: u32, health: u32| {
        Info::with_fields(NetKind::Vehicle, AssetId(model), &[health]).expect("info")
    };
    let mut run =
        |frames: std::ops::Range<u32>, info: Info, module: &mut Module<FakeRuntime>, game: &mut FakeGame| {
            for frame in frames {
                if frame % 3 == 0 {
                    let tick = frame / 3;
                    let states = [state(1, tick as f32 * 0.5), state(LOCAL, 0.0)];
                    for d in snapshot_with(&mut encoder, tick, &states, &[info, Info::PLAYER]) {
                        inbox.push(d);
                    }
                }
                module.frame(1.0 / FRAME_HZ, game, &mut link).expect("frame");
            }
        };
    run(0..60, car(5, 100), &mut module, &mut game);
    let first = module.entities().handle(1).expect("shown");

    // Damage changes a block field: the same car stays in the game.
    run(60..120, car(5, 40), &mut module, &mut game);
    assert_eq!(
        module.entities().handle(1),
        Some(first),
        "a block change keeps the game entity"
    );

    // A new model cannot be swapped in place: the car is spawned again with it.
    run(120..180, car(6, 40), &mut module, &mut game);
    let second = module.entities().handle(1).expect("shown again");
    assert_ne!(second, first);
    assert_eq!(game.entities[&second].1, GameAsset(0xCB));
    assert!(!game.entities.contains_key(&first));
}

#[test]
fn ownership_epochs_wrap_and_each_event_applies_as_it_comes() {
    let mut link = NetLink::new(Client::new(LOCAL, TICK_HZ, ClientConfig::default()));
    // 256 hand-offs of one entity: the epoch wraps from 255 to 0, and every give and take-back still applies.
    for epoch in 0..=300u32 {
        let epoch = (epoch % 256) as u8;
        link.set_ownership(1, epoch, true);
        assert_eq!(link.owned()[&1].epoch, epoch);
        link.set_ownership(1, epoch, false);
        assert!(link.owned().is_empty(), "take-back with epoch {epoch} applied");
    }
}

#[test]
fn the_inbox_keeps_only_the_newest_datagrams() {
    let link = NetLink::new(Client::new(LOCAL, TICK_HZ, ClientConfig::default()));
    let inbox = link.inbox();
    for i in 0..MAX_QUEUED as u32 + 10 {
        inbox.push(i.to_le_bytes().to_vec());
    }
    let kept = inbox.take();
    assert_eq!(kept.len(), MAX_QUEUED);
    assert_eq!(kept[0], 10u32.to_le_bytes().to_vec(), "the oldest were dropped");
}

#[test]
fn a_joining_link_waits_for_the_join_and_takes_corrections() {
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
    let mut link = NetLink::joining(ClientConfig::default());
    let (inbox, events) = (link.inbox(), link.events());
    let mut encoder = DeltaEncoder::default();
    let mut frame_at =
        |frame: u32, module: &mut Module<FakeRuntime>, game: &mut FakeGame, link: &mut NetLink| {
            if frame % 3 == 0 {
                let tick = frame / 3;
                for d in snapshot(
                    &mut encoder,
                    tick,
                    &[state(1, tick as f32 * 0.5), state(LOCAL, 0.0)],
                ) {
                    inbox.push(d);
                }
            }
            module.frame(1.0 / FRAME_HZ, game, link).expect("frame");
        };

    // Snapshots before the join are dropped: nothing is shown, nothing is sent.
    for frame in 0..30 {
        frame_at(frame, &mut module, &mut game, &mut link);
    }
    assert!(link.client().is_none());
    assert!(module.entities().is_empty());
    assert!(link.outbox().take().is_empty());

    events.push(ServerEvent::Joined {
        entity: LOCAL,
        tick_hz: TICK_HZ,
    });
    for frame in 30..90 {
        frame_at(frame, &mut module, &mut game, &mut link);
    }
    assert_eq!(link.client().map(|c| c.entity()), Some(LOCAL));
    let car = module.entities().handle(1).expect("shown after the join");

    // A correction far from where the game has the local player snaps it there.
    let held = state(LOCAL, 200.0);
    events.push(ServerEvent::Corrected(held));
    frame_at(90, &mut module, &mut game, &mut link);
    let local = game.entities[&me].2.position;
    assert!(
        (local[0] - 200.0).abs() < 0.05,
        "local player snapped to the server's position, at {local:?}"
    );

    // A correction for an entity this client does not own is dropped (Corrected carries no epoch)...
    let before = game.entities[&car].2;
    events.push(ServerEvent::Corrected(state(1, -500.0)));
    frame_at(91, &mut module, &mut game, &mut link);
    assert!(
        game.entities[&car].2.position[0] > -400.0,
        "not moved to {before:?}'s correction"
    );

    // ...and taken for one it owns.
    events.push(ServerEvent::Ownership {
        entity: 1,
        epoch: 3,
        yours: true,
    });
    events.push(ServerEvent::Corrected(state(1, -500.0)));
    frame_at(92, &mut module, &mut game, &mut link);
    assert!(
        (game.entities[&car].2.position[0] + 500.0).abs() < 0.05,
        "owned entity moved back by the server"
    );
}

#[test]
fn a_second_join_starts_the_session_over() {
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
    let mut link = NetLink::joining(ClientConfig::default());
    let (inbox, events) = (link.inbox(), link.events());
    let mut encoder = DeltaEncoder::default();
    events.push(ServerEvent::Joined {
        entity: LOCAL,
        tick_hz: TICK_HZ,
    });
    for frame in 0..60u32 {
        if frame % 3 == 0 {
            let tick = frame / 3;
            for d in snapshot(
                &mut encoder,
                tick,
                &[state(1, tick as f32 * 0.5), state(LOCAL, 0.0)],
            ) {
                inbox.push(d);
            }
        }
        module.frame(1.0 / FRAME_HZ, &mut game, &mut link).expect("frame");
    }
    let old = module.entities().handle(1).expect("shown in the first session");
    link.set_ownership(1, 4, true);

    // A reconnect: the new session's snapshots no longer hold entity 1.
    events.push(ServerEvent::Joined {
        entity: LOCAL,
        tick_hz: TICK_HZ,
    });
    let mut encoder = DeltaEncoder::default();
    for frame in 0..60u32 {
        if frame % 3 == 0 {
            for d in snapshot(&mut encoder, frame / 3, &[state(LOCAL, 0.0)]) {
                inbox.push(d);
            }
        }
        module.frame(1.0 / FRAME_HZ, &mut game, &mut link).expect("frame");
    }
    assert!(
        module.entities().handle(1).is_none(),
        "the old session's entity is gone"
    );
    assert!(!game.entities.contains_key(&old));
    assert!(
        link.owned().is_empty(),
        "the new session owns nothing for this client yet"
    );
    assert!(game.entities.contains_key(&me), "the local player stays");
    assert_eq!(link.world_errors, 0);
}

#[test]
fn the_clock_weather_and_doors_follow_the_server() {
    let mut module = module();
    let mut game = FakeGame::default();
    let mut link = NetLink::joining(ClientConfig::default());
    let (inbox, events) = (link.inbox(), link.events());
    let mut encoder = DeltaEncoder::default();
    let mut run = |frames: std::ops::Range<u32>,
                   module: &mut Module<FakeRuntime>,
                   game: &mut FakeGame,
                   link: &mut NetLink| {
        for frame in frames {
            if frame % 3 == 0 {
                for d in snapshot(&mut encoder, frame / 3, &[state(LOCAL, 0.0)]) {
                    inbox.push(d);
                }
            }
            module.frame(1.0 / FRAME_HZ, game, link).expect("frame");
        }
    };
    // 10:00, two game minutes a real second, clear (7) all along.
    let mut environment = Environment {
        tick: 0,
        minute: 600,
        rate: 200,
        weather: AssetId(7),
        next_weather: AssetId(7),
        transition: 0,
    };
    events.push(ServerEvent::Joined {
        entity: LOCAL,
        tick_hz: TICK_HZ,
    });
    events.push(ServerEvent::Environment(environment));
    let door = Door {
        id: AssetId(8),
        locked: true,
        open: 0,
    };
    events.push(ServerEvent::Door(door));
    // A door the asset table does not know is counted, not applied.
    events.push(ServerEvent::Door(Door {
        id: AssetId(40),
        locked: true,
        open: 0,
    }));

    // Three seconds: the clock runs about six game minutes on from 10:00.
    run(0..180, &mut module, &mut game, &mut link);
    let minute = game.clock.expect("clock set");
    assert!((604.0..=607.0).contains(&minute), "the clock shows {minute}");
    let clear = WeatherState {
        current: GameAsset(0xD0),
        next: GameAsset(0xD0),
        transition: 0.0,
    };
    assert_eq!(game.weather, Some(clear));
    assert_eq!(
        game.doors[&GameAsset(0xD1)],
        DoorState {
            locked: true,
            open: 0.0
        }
    );
    assert_eq!(game.doors.len(), 1);
    assert_eq!(link.world_errors, 1);

    // Rain (6) half way in, the clock stopped at noon, and the door unlocked and wide open.
    environment = Environment {
        tick: 60,
        minute: 720,
        rate: 0,
        next_weather: AssetId(6),
        transition: 255 / 2 + 1,
        ..environment
    };
    events.push(ServerEvent::Environment(environment));
    events.push(ServerEvent::Door(Door {
        locked: false,
        open: 255,
        ..door
    }));
    run(180..240, &mut module, &mut game, &mut link);
    assert_eq!(game.clock, Some(720.0));
    let weather = game.weather.expect("weather");
    assert_eq!(
        (weather.current, weather.next),
        (GameAsset(0xD0), GameAsset(0xCB))
    );
    assert!((weather.transition - 0.5).abs() < 0.01);
    assert_eq!(
        game.doors[&GameAsset(0xD1)],
        DoorState {
            locked: false,
            open: 1.0
        }
    );
    assert_eq!(link.world_errors, 1);
}
