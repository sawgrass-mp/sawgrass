//! The module's life on the fake runtime and the fake game, with a network core that records what it sees.

use sawgrass_client_module::assets::{AssetId, AssetTable};
use sawgrass_client_module::fake::FakeRuntime;
use sawgrass_client_module::game::{
    EntityKind, FakeGame, GameApi, GameAsset, GameError, GameHandle, InputState, Pose, SinglePlayerSystem,
};
use sawgrass_client_module::hooks::{EventSink, GameEvent, HookPoint};
use sawgrass_client_module::module::{
    CoreEvent, Module, NetCore, REQUIRED_FUNCTIONS, StartError, World, WorldError, all_functions,
    optional_functions,
};
use sawgrass_client_module::runtime::{GameBuild, RuntimeKind};
use sawgrass_client_module::table::{FunctionTable, LookupError, ResolveError};

const BUILD_KEY: &str = "victory/1.0.0/00ff";
const CAR: GameAsset = GameAsset(0xCA);

fn runtime() -> FakeRuntime {
    let build = GameBuild {
        runtime: RuntimeKind::Victory,
        title_version: "1.0.0".into(),
        image_digest: "00ff".into(),
    };
    // Made-up bytes: function `i` sits at 0x10 * i and starts with 5A i A5 3C.
    let mut image = vec![0u8; 0x10 * all_functions().len()];
    for i in 0..all_functions().len() {
        image[0x10 * i..0x10 * i + 4].copy_from_slice(&[0x5A, i as u8, 0xA5, 0x3C]);
    }
    FakeRuntime::new(build, 0x4000, image)
}

fn functions() -> FunctionTable {
    let mut text = format!("build = {BUILD_KEY}\n");
    for (i, name) in all_functions().iter().enumerate() {
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
    let hooks = module.event_sink();

    hooks.push(GameEvent::EntityCreated {
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
    hooks.push(GameEvent::EntityCreated {
        handle: walker,
        kind: EntityKind::Ped,
        asset: GameAsset(2),
    });
    hooks.push(GameEvent::EntityCreated {
        handle: prop,
        kind: EntityKind::Object,
        asset: GameAsset(3),
    });
    hooks.push(GameEvent::EntityCreated {
        handle: car,
        kind: EntityKind::Vehicle,
        asset: CAR,
    });
    let input = InputState {
        movement: [0.0, 1.0],
        ..InputState::default()
    };
    hooks.push(GameEvent::Input(input));
    module.frame(0.016, &mut game, &mut core).expect("frame");
    assert!(!game.entities.contains_key(&walker), "ambient pedestrian removed");
    assert!(game.entities.contains_key(&prop), "map props stay");
    assert_eq!(game.entities[&car].2, pose(11.0), "our car stays and moves");
    assert_eq!(module.ambient_removed, 1);

    game.entities.remove(&car);
    hooks.push(GameEvent::EntityRemoved {
        handle: car,
        kind: EntityKind::Vehicle,
    });
    module.frame(0.016, &mut game, &mut core).expect("frame");
    assert_eq!(
        core.events,
        vec![CoreEvent::Input(input), CoreEvent::EntityLost(1)]
    );
    assert!(module.entities().is_empty());

    let (runtime, errors) = module.stop();
    assert!(errors.is_empty());
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

/// A table with only these entries, at the same made-up places as [`functions`].
fn functions_only(names: &[&str]) -> FunctionTable {
    let mut text = format!("build = {BUILD_KEY}\n");
    for (i, name) in all_functions().iter().enumerate() {
        if names.contains(name) {
            text += &format!("{name} = 5A {i:02X} A5 3C\n");
        }
    }
    FunctionTable::parse(&text).expect("valid table")
}

#[test]
fn starts_with_the_four_required_functions_and_switches_the_rest_off() {
    let mut module =
        Module::start(runtime(), &functions_only(&REQUIRED_FUNCTIONS), assets(BUILD_KEY)).expect("started");
    // Only the frame is hooked; the other three are the real game interface's to call.
    let hooked: Vec<_> = module
        .runtime()
        .hooks
        .iter()
        .map(|(_, r)| r.function.as_str())
        .collect();
    assert_eq!(hooked, vec!["frame"]);
    assert!(module.is_hooked(HookPoint::Frame));
    assert!(!module.is_hooked(HookPoint::PedCreate));
    for name in REQUIRED_FUNCTIONS {
        assert!(module.functions().address(name).is_some(), "{name}");
    }
    let missing: Vec<_> = module
        .missing_functions()
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(missing, optional_functions());
    assert!(
        module
            .missing_functions()
            .iter()
            .all(|(_, e)| *e == LookupError::NotInTable)
    );

    // Frames run, and no single-player switch is called without its function.
    let mut game = FakeGame::default();
    let mut core = RecordingCore::default();
    for _ in 0..3 {
        module.frame(1.0 / 60.0, &mut game, &mut core).expect("frame");
    }
    assert!(game.disabled.is_empty());
    assert_eq!(core.frames, 3);
}

#[test]
fn some_switches_located_turn_only_those_systems_off() {
    let mut names = REQUIRED_FUNCTIONS.to_vec();
    names.push(SinglePlayerSystem::Police.function());
    let mut module = Module::start(runtime(), &functions_only(&names), assets(BUILD_KEY)).expect("started");
    let mut game = FakeGame::default();
    module
        .frame(1.0 / 60.0, &mut game, &mut RecordingCore::default())
        .expect("frame");
    assert_eq!(game.disabled, vec![SinglePlayerSystem::Police]);
}

#[test]
fn a_missing_required_function_stops_the_start() {
    let table = functions_only(&["frame", "local_player", "ped_spawn"]);
    let Err((StartError::Functions(ResolveError::Functions(failed)), rt)) =
        Module::start(runtime(), &table, assets(BUILD_KEY))
    else {
        panic!("the start needs every required function");
    };
    assert_eq!(
        failed,
        vec![("entity_set_pose".to_owned(), LookupError::NotInTable)]
    );
    assert!(rt.hooks.is_empty());
}

#[test]
fn a_refused_frame_hook_leaves_nothing_installed() {
    let mut rt = runtime();
    rt.refuse.push(HookPoint::Frame.function().into());
    let Err((StartError::Runtime(_), rt)) = Module::start(rt, &functions(), assets(BUILD_KEY)) else {
        panic!("a refused frame hook stops the start");
    };
    assert!(rt.hooks.is_empty());
}

#[test]
fn a_refused_optional_hook_only_switches_its_events_off() {
    let mut rt = runtime();
    rt.refuse.push(HookPoint::StreamingUpdate.function().into());
    let module = Module::start(rt, &functions(), assets(BUILD_KEY)).expect("started");
    assert!(!module.is_hooked(HookPoint::StreamingUpdate));
    assert!(module.is_hooked(HookPoint::PedCreate));
    let refused: Vec<_> = module.refused_hooks().iter().map(|(p, _)| *p).collect();
    assert_eq!(refused, vec![HookPoint::StreamingUpdate]);
}

#[test]
fn a_runtime_that_drives_frames_needs_no_frame_entry() {
    let mut rt = runtime();
    rt.drives_frames = true;
    let table = functions_only(&["local_player", "ped_spawn", "entity_set_pose"]);
    let mut module = Module::start(rt, &table, assets(BUILD_KEY)).expect("started");
    assert!(module.runtime().hooks.is_empty());
    assert!(!module.is_hooked(HookPoint::Frame));
    module
        .frame(
            1.0 / 60.0,
            &mut FakeGame::default(),
            &mut RecordingCore::default(),
        )
        .expect("frame");
}

#[test]
fn refuses_an_asset_table_for_another_build() {
    let result = Module::start(runtime(), &functions(), assets("victory/1.0.1/00ff"));
    assert!(matches!(result, Err((StartError::AssetsForOtherBuild { .. }, _))));
}

/// A game whose creation and removal functions run the hooks, as the real game will, while `frame` is running.
struct ReentrantGame {
    inner: FakeGame,
    hooks: EventSink,
}

impl GameApi for ReentrantGame {
    fn spawn(&mut self, kind: EntityKind, asset: GameAsset, pose: Pose) -> Result<GameHandle, GameError> {
        let handle = self.inner.spawn(kind, asset, pose)?;
        self.hooks.push(GameEvent::EntityCreated { handle, kind, asset });
        Ok(handle)
    }
    fn remove(&mut self, handle: GameHandle) -> Result<(), GameError> {
        let kind = self
            .inner
            .entities
            .get(&handle)
            .map(|e| e.0)
            .ok_or(GameError::NoSuchEntity(handle))?;
        self.inner.remove(handle)?;
        self.hooks.push(GameEvent::EntityRemoved { handle, kind });
        Ok(())
    }
    fn pose(&self, handle: GameHandle) -> Option<Pose> {
        self.inner.pose(handle)
    }
    fn set_pose(&mut self, handle: GameHandle, pose: Pose) -> Result<(), GameError> {
        self.inner.set_pose(handle, pose)
    }
    fn local_player(&self) -> Option<GameHandle> {
        self.inner.local_player()
    }
    fn set_streaming_focus(&mut self, points: &[[f32; 3]]) {
        self.inner.set_streaming_focus(points);
    }
    fn disable(&mut self, system: SinglePlayerSystem) -> Result<(), GameError> {
        self.inner.disable(system)
    }
    fn set_clock(&mut self, minute: f32) -> Result<(), GameError> {
        self.inner.set_clock(minute)
    }
    fn set_weather(&mut self, current: GameAsset, next: GameAsset, transition: f32) -> Result<(), GameError> {
        self.inner.set_weather(current, next, transition)
    }
    fn set_door(&mut self, door: GameAsset, locked: bool, open: f32) -> Result<(), GameError> {
        self.inner.set_door(door, locked, open)
    }
}

/// Spawns entity 1 and, on the first frame only, entity 2, which it despawns in the same frame.
struct SpawnAndDespawn {
    frames: u32,
}

impl NetCore for SpawnAndDespawn {
    fn on_event(&mut self, _event: &CoreEvent) {}
    fn on_frame(&mut self, _dt: f32, world: &mut World<'_>) {
        if self.frames == 0 {
            world
                .spawn(1, EntityKind::Vehicle, AssetId(5), pose(1.0))
                .expect("spawned");
            world
                .spawn(2, EntityKind::Vehicle, AssetId(5), pose(2.0))
                .expect("spawned");
            world.despawn(2).expect("despawned");
        }
        self.frames += 1;
    }
}

#[test]
fn hooks_that_fire_inside_a_frame_are_handled_on_the_next() {
    let mut module = Module::start(runtime(), &functions(), assets(BUILD_KEY)).expect("started");
    let mut game = ReentrantGame {
        inner: FakeGame::default(),
        hooks: module.event_sink(),
    };
    let mut core = SpawnAndDespawn { frames: 0 };
    module.frame(0.016, &mut game, &mut core).expect("frame");
    module.frame(0.016, &mut game, &mut core).expect("frame");
    let ours = module.entities().handle(1).expect("bound");
    assert!(
        game.inner.entities.contains_key(&ours),
        "our own spawn is not taken for ambient traffic"
    );
    assert_eq!(module.ambient_removed, 0, "an entity already gone is not counted");
}

#[test]
fn stop_gives_the_runtime_back_with_every_failure() {
    let mut rt = runtime();
    rt.refuse_removal = vec![
        HookPoint::Frame.function().into(),
        HookPoint::InputPoll.function().into(),
    ];
    let module = Module::start(rt, &functions(), assets(BUILD_KEY)).expect("started");
    let (rt, errors) = module.stop();
    assert_eq!(errors.len(), 2);
    let left: Vec<_> = rt.hooks.iter().map(|(_, r)| r.function.as_str()).collect();
    assert_eq!(
        left,
        vec!["frame", "input_poll"],
        "every other hook was still removed"
    );
}

/// Spawns entity 1, then on the next frame despawns it and spawns entity 2, which gets the same raw handle.
struct Replace {
    frames: u32,
    events: Vec<CoreEvent>,
}

impl NetCore for Replace {
    fn on_event(&mut self, event: &CoreEvent) {
        self.events.push(event.clone());
    }
    fn on_frame(&mut self, _dt: f32, world: &mut World<'_>) {
        match self.frames {
            0 => world
                .spawn(1, EntityKind::Vehicle, AssetId(5), pose(1.0))
                .expect("spawned"),
            1 => {
                world.despawn(1).expect("despawned");
                world
                    .spawn(2, EntityKind::Vehicle, AssetId(5), pose(2.0))
                    .expect("spawned");
            }
            _ => {}
        }
        self.frames += 1;
    }
}

#[test]
fn a_late_event_about_a_reused_handle_does_not_reach_the_new_entity() {
    let mut module = Module::start(runtime(), &functions(), assets(BUILD_KEY)).expect("started");
    let mut game = ReentrantGame {
        inner: FakeGame::default(),
        hooks: module.event_sink(),
    };
    let mut core = Replace {
        frames: 0,
        events: Vec::new(),
    };
    for _ in 0..3 {
        module.frame(0.016, &mut game, &mut core).expect("frame");
    }
    let second = module.entities().handle(2).expect("entity 2 bound");
    assert_eq!(second.generation, 2, "the raw handle of entity 1 was reused");
    assert!(
        game.inner.entities.contains_key(&second),
        "entity 2 is neither removed as ambient nor lost"
    );
    assert!(
        core.events.is_empty(),
        "the removal of entity 1 is not reported as a loss of entity 2"
    );
    assert_eq!(module.ambient_removed, 0);
}
