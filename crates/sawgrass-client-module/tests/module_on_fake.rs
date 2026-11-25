//! The module's life on the fake runtime and the fake game, with a network core that records what it sees.

use sawgrass_client_module::assets::{AssetId, AssetTable};
use sawgrass_client_module::fake::FakeRuntime;
use sawgrass_client_module::game::{EntityKind, FakeGame, GameAsset, InputState, Pose, SinglePlayerSystem};
use sawgrass_client_module::hooks::{GameEvent, HookPoint};
use sawgrass_client_module::module::{
    CoreEvent, Module, NetCore, StartError, World, WorldError, required_functions,
};
use sawgrass_client_module::runtime::{GameBuild, RuntimeKind};
use sawgrass_client_module::table::FunctionTable;

const BUILD_KEY: &str = "victory/1.0.0/00ff";
const CAR: GameAsset = GameAsset(0xCA);

fn runtime() -> FakeRuntime {
    let build = GameBuild {
        runtime: RuntimeKind::Victory,
        title_version: "1.0.0".into(),
        image_digest: "00ff".into(),
    };
    // Made-up bytes: function `i` sits at 0x10 * i and starts with 5A i A5 3C.
    let mut image = vec![0u8; 0x10 * required_functions().len()];
    for i in 0..required_functions().len() {
        image[0x10 * i..0x10 * i + 4].copy_from_slice(&[0x5A, i as u8, 0xA5, 0x3C]);
    }
    FakeRuntime::new(build, 0x4000, image)
}

fn functions() -> FunctionTable {
    let mut text = format!("build = {BUILD_KEY}\n");
    for (i, name) in required_functions().iter().enumerate() {
        text += &format!("{name} = 5A {i:02X} A5 3C\n");
    }
    FunctionTable::parse(&text).expect("valid table")
}

fn assets(build: &str) -> AssetTable {
    AssetTable::parse(&format!("build = {build}\n5 = 0xCA\n")).expect("valid table")
}

fn pose(x: f32) -> Pose {
    Pose {
        position: [x, 0.0, 0.0],
        heading: 0.0,
    }
}

/// Spawns server entity 1 as a car on the first frame and records the events.
#[derive(Default)]
struct RecordingCore {
    events: Vec<CoreEvent>,
    frames: u32,
    local: Option<Pose>,
}

impl NetCore for RecordingCore {
    fn on_event(&mut self, event: &CoreEvent) {
        self.events.push(event.clone());
    }

    fn on_frame(&mut self, _dt: f32, world: &mut World<'_>) {
        if self.frames == 0 {
            world
                .spawn(1, EntityKind::Vehicle, AssetId(5), pose(10.0))
                .expect("spawned");
            assert!(
                world
                    .spawn(2, EntityKind::Vehicle, AssetId(6), pose(0.0))
                    .is_err(),
                "unknown asset"
            );
        } else if world.pose(1).is_some() {
            world.set_pose(1, pose(11.0)).expect("moved");
        } else {
            assert_eq!(world.set_pose(1, pose(11.0)), Err(WorldError::UnknownEntity(1)));
        }
        self.local = world.local_player_pose();
        world.set_streaming_focus(&[[10.0, 0.0, 0.0]]);
        self.frames += 1;
    }
}

#[test]
fn starts_hooks_everything_and_runs_frames() {
    let mut module = Module::start(runtime(), &functions(), assets(BUILD_KEY)).expect("started");
    assert_eq!(module.functions().address("frame"), Some(0x4000));
    let mut game = FakeGame::default();
    let me = game.create_natively(EntityKind::Ped, GameAsset(1), pose(1.0));
    game.local_player = Some(me);
    let mut core = RecordingCore::default();

    module.push_event(GameEvent::EntityCreated {
        handle: me,
        kind: EntityKind::Ped,
        asset: GameAsset(1),
    });
    module.frame(0.016, &mut game, &mut core).expect("frame");
    assert_eq!(game.disabled, SinglePlayerSystem::ALL.to_vec());
    assert_eq!(core.local, Some(pose(1.0)), "the local player is kept");
    let car = module.entities().handle(1).expect("car bound");
    assert_eq!(game.entities[&car].1, CAR);
    assert_eq!(game.streaming_focus, vec![[10.0, 0.0, 0.0]]);

    // The game makes a pedestrian and a prop by itself, reads input, and streams the car out.
    let walker = game.create_natively(EntityKind::Ped, GameAsset(2), pose(3.0));
    let prop = game.create_natively(EntityKind::Object, GameAsset(3), pose(4.0));
    module.push_event(GameEvent::EntityCreated {
        handle: walker,
        kind: EntityKind::Ped,
        asset: GameAsset(2),
    });
    module.push_event(GameEvent::EntityCreated {
        handle: prop,
        kind: EntityKind::Object,
        asset: GameAsset(3),
    });
    module.push_event(GameEvent::EntityCreated {
        handle: car,
        kind: EntityKind::Vehicle,
        asset: CAR,
    });
    let input = InputState {
        movement: [0.0, 1.0],
        ..InputState::default()
    };
    module.push_event(GameEvent::Input(input));
    module.frame(0.016, &mut game, &mut core).expect("frame");
    assert!(!game.entities.contains_key(&walker), "ambient pedestrian removed");
    assert!(game.entities.contains_key(&prop), "map props stay");
    assert_eq!(game.entities[&car].2, pose(11.0), "our car stays and moves");
    assert_eq!(module.ambient_removed, 1);

    game.entities.remove(&car);
    module.push_event(GameEvent::EntityRemoved {
        handle: car,
        kind: EntityKind::Vehicle,
    });
    module.frame(0.016, &mut game, &mut core).expect("frame");
    assert_eq!(
        core.events,
        vec![CoreEvent::Input(input), CoreEvent::EntityLost(1)]
    );
    assert!(module.entities().is_empty());

    let runtime = module.stop().expect("stopped");
    assert!(runtime.hooks.is_empty());
}

#[test]
fn installs_one_hook_per_hook_point() {
    let module = Module::start(runtime(), &functions(), assets(BUILD_KEY)).expect("started");
    let hooked: Vec<_> = module
        .runtime()
        .hooks
        .iter()
        .map(|(_, r)| r.function.as_str())
        .collect();
    assert_eq!(hooked, HookPoint::ALL.map(HookPoint::function).to_vec());
}

#[test]
fn a_refused_hook_leaves_nothing_installed() {
    let mut rt = runtime();
    rt.refuse.push(HookPoint::StreamingUpdate.function().into());
    let Err((StartError::Runtime(_), rt)) = Module::start(rt, &functions(), assets(BUILD_KEY)) else {
        panic!("a refused hook stops the start");
    };
    assert!(rt.hooks.is_empty());
}

#[test]
fn refuses_an_asset_table_for_another_build() {
    let result = Module::start(runtime(), &functions(), assets("victory/1.0.1/00ff"));
    assert!(matches!(result, Err((StartError::AssetsForOtherBuild { .. }, _))));
}
