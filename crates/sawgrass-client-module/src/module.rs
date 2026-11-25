//! The module's life: start on a runtime, run each game frame, stop. Also where the network core plugs in.
//!
//! The network core (the `sawgrass-netcode` crate, CLIENT-3) implements [`NetCore`]. It sees the game only through
//! [`World`], which speaks server ids and protocol asset ids, so it never handles a game pointer or a game build.

use std::fmt;

use crate::assets::{AssetId, AssetTable};
use crate::entity_pool::{BindError, EntityPool, NetId};
use sawgrass_protocol::world::{Door, Environment};

use crate::game::{
    CameraPose, EntityKind, GameApi, GameAsset, GameError, InputState, Pose, SinglePlayerSystem,
};
use crate::hooks::{EventSink, GameEvent, HookPoint};
use crate::runtime::{HookHandle, HookRequest, Runtime, RuntimeError};
use crate::table::{FunctionTable, ResolveError, ResolvedFunctions};

/// What the network core learns from the game, between frames.
#[derive(Debug, Clone, PartialEq)]
pub enum CoreEvent {
    Input(InputState),
    Camera(CameraPose),
    /// The game removed an entity the server owns (streamed out, destroyed). The core decides whether to spawn it
    /// again.
    EntityLost(NetId),
}

/// The network core's side of the module.
pub trait NetCore {
    fn on_event(&mut self, event: &CoreEvent);
    /// Once per game frame, after the frame's events: apply snapshots, move remote entities, read the local player.
    fn on_frame(&mut self, dt: f32, world: &mut World<'_>);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldError {
    UnknownAsset(AssetId),
    UnknownEntity(NetId),
    /// The game has no local player ped yet.
    NoLocalPlayer,
    Bind(BindError),
    Game(GameError),
}

/// The game as the network core sees it: entities by server id, assets by protocol id.
pub struct World<'a> {
    game: &'a mut dyn GameApi,
    pool: &'a mut EntityPool,
    assets: &'a AssetTable,
}

impl fmt::Debug for World<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("World")
            .field("entities", &self.pool.len())
            .finish_non_exhaustive()
    }
}

impl World<'_> {
    pub fn spawn(
        &mut self,
        net: NetId,
        kind: EntityKind,
        asset: AssetId,
        pose: Pose,
    ) -> Result<(), WorldError> {
        if self.pool.handle(net).is_some() {
            return Err(WorldError::Bind(BindError::NetIdTaken(net)));
        }
        let fallback = (asset == AssetId::NONE && kind == EntityKind::Ped).then(|| self.assets.default_ped());
        let game_asset = fallback
            .flatten()
            .or_else(|| self.assets.game_asset(asset))
            .ok_or(WorldError::UnknownAsset(asset))?;
        let handle = self
            .game
            .spawn(kind, game_asset, pose)
            .map_err(WorldError::Game)?;
        self.pool.bind(net, handle, kind).map_err(WorldError::Bind)
    }

    pub fn despawn(&mut self, net: NetId) -> Result<(), WorldError> {
        let (handle, _) = self.pool.unbind_net(net).ok_or(WorldError::UnknownEntity(net))?;
        self.game.remove(handle).map_err(WorldError::Game)
    }

    pub fn pose(&self, net: NetId) -> Option<Pose> {
        self.game.pose(self.pool.handle(net)?)
    }

    pub fn set_pose(&mut self, net: NetId, pose: Pose) -> Result<(), WorldError> {
        let handle = self.pool.handle(net).ok_or(WorldError::UnknownEntity(net))?;
        self.game.set_pose(handle, pose).map_err(WorldError::Game)
    }

    pub fn local_player_pose(&self) -> Option<Pose> {
        self.game.pose(self.game.local_player()?)
    }

    /// Moves the local player, to apply a correction from the server.
    pub fn set_local_player_pose(&mut self, pose: Pose) -> Result<(), WorldError> {
        let handle = self.game.local_player().ok_or(WorldError::NoLocalPlayer)?;
        self.game.set_pose(handle, pose).map_err(WorldError::Game)
    }

    pub fn set_streaming_focus(&mut self, points: &[[f32; 3]]) {
        self.game.set_streaming_focus(points);
    }

    /// Sets the game's clock to `minute` of the day, below [`sawgrass_protocol::world::MINUTES_PER_DAY`].
    pub fn set_clock(&mut self, minute: f32) -> Result<(), WorldError> {
        self.game.set_clock(minute).map_err(WorldError::Game)
    }

    /// Sets the weather from the server's [`Environment`]. Weather [`AssetId::NONE`] leaves the game's own, and a
    /// next weather of none means no change is under way.
    pub fn set_weather(&mut self, environment: &Environment) -> Result<(), WorldError> {
        if environment.weather == AssetId::NONE {
            return Ok(());
        }
        let current = self.asset(environment.weather)?;
        let next = if environment.next_weather == AssetId::NONE {
            current
        } else {
            self.asset(environment.next_weather)?
        };
        let transition = f32::from(environment.transition) / 255.0;
        self.game
            .set_weather(current, next, transition)
            .map_err(WorldError::Game)
    }

    /// Applies a door's state from the server. The door's id is looked up in the asset table (ADR 0006,
    /// assumption A4).
    pub fn set_door(&mut self, door: &Door) -> Result<(), WorldError> {
        let asset = self.asset(door.id)?;
        let open = f32::from(door.open) / 255.0;
        self.game
            .set_door(asset, door.locked, open)
            .map_err(WorldError::Game)
    }

    fn asset(&self, id: AssetId) -> Result<GameAsset, WorldError> {
        self.assets.game_asset(id).ok_or(WorldError::UnknownAsset(id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    Runtime(RuntimeError),
    /// The asset table was written for another build.
    AssetsForOtherBuild {
        table: String,
        running: String,
    },
    Functions(ResolveError),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "{e}"),
            Self::AssetsForOtherBuild { table, running } => {
                write!(f, "asset table is for build {table}, but {running} is running")
            }
            Self::Functions(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// Every function the module locates at start-up: the hooks, then the single-player switches.
pub fn required_functions() -> Vec<&'static str> {
    let hooks = HookPoint::ALL.iter().map(|h| h.function());
    hooks
        .chain(SinglePlayerSystem::ALL.iter().map(|s| s.function()))
        .collect()
}

#[derive(Debug)]
pub struct Module<R: Runtime> {
    runtime: R,
    functions: ResolvedFunctions,
    hooks: Vec<HookHandle>,
    assets: AssetTable,
    pool: EntityPool,
    events: EventSink,
    single_player_off: bool,
    /// Peds and vehicles the game made by itself and the module removed. Entities that were already gone when the
    /// frame got to them (one of ours spawned and despawned within a frame, say) are not counted.
    pub ambient_removed: u64,
}

impl<R: Runtime> Module<R> {
    /// Locates every required function for the running build and installs the hooks. If any step fails, nothing is
    /// left installed and the runtime is given back with the reason, so the adapter can report it in the game.
    pub fn start(
        mut runtime: R,
        functions: &FunctionTable,
        assets: AssetTable,
    ) -> Result<Self, (StartError, R)> {
        let resolved = match Self::resolve(&runtime, functions, &assets) {
            Ok(resolved) => resolved,
            Err(error) => return Err((error, runtime)),
        };
        let mut hooks = Vec::new();
        for point in HookPoint::ALL {
            let function = point.function().to_owned();
            let address = resolved
                .address(&function)
                .expect("resolve located every required function");
            match runtime.install_hook(HookRequest { function, address }) {
                Ok(handle) => hooks.push(handle),
                Err(error) => {
                    for handle in hooks {
                        // Best effort: the start already failed, and the error that caused it is the one to report.
                        let _ = runtime.remove_hook(handle);
                    }
                    return Err((StartError::Runtime(error), runtime));
                }
            }
        }
        Ok(Self {
            runtime,
            functions: resolved,
            hooks,
            assets,
            pool: EntityPool::default(),
            events: EventSink::default(),
            single_player_off: false,
            ambient_removed: 0,
        })
    }

    fn resolve(
        runtime: &R,
        functions: &FunctionTable,
        assets: &AssetTable,
    ) -> Result<ResolvedFunctions, StartError> {
        let build = runtime.build().map_err(StartError::Runtime)?;
        if !assets.is_for(&build) {
            return Err(StartError::AssetsForOtherBuild {
                table: assets.build.clone(),
                running: build.key(),
            });
        }
        let image = runtime.code_image().map_err(StartError::Runtime)?;
        functions
            .resolve(&build, image, &required_functions())
            .map_err(StartError::Functions)
    }

    pub fn runtime(&self) -> &R {
        &self.runtime
    }

    pub fn functions(&self) -> &ResolvedFunctions {
        &self.functions
    }

    pub fn entities(&self) -> &EntityPool {
        &self.pool
    }

    /// The queue the adapter's hooks push into (see [`crate::hooks`]). Take it once at start-up.
    pub fn event_sink(&self) -> EventSink {
        self.events.clone()
    }

    /// Called by the adapter from the frame hook.
    pub fn frame(
        &mut self,
        dt: f32,
        game: &mut dyn GameApi,
        core: &mut dyn NetCore,
    ) -> Result<(), GameError> {
        if !self.single_player_off {
            for system in SinglePlayerSystem::ALL {
                game.disable(system)?;
            }
            self.single_player_off = true;
        }
        for event in self.events.take() {
            match event {
                GameEvent::EntityCreated { handle, kind, .. } => {
                    let ours = self.pool.net_id(handle).is_some() || game.local_player() == Some(handle);
                    // Objects include the map's own props, so only peds and vehicles count as ambient life.
                    if !ours && kind != EntityKind::Object {
                        match game.remove(handle) {
                            Ok(()) => self.ambient_removed += 1,
                            Err(GameError::NoSuchEntity(_)) => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                GameEvent::EntityRemoved { handle, .. } => {
                    if let Some(net) = self.pool.unbind_handle(handle) {
                        core.on_event(&CoreEvent::EntityLost(net));
                    }
                }
                GameEvent::Input(input) => core.on_event(&CoreEvent::Input(input)),
                GameEvent::Camera(camera) => core.on_event(&CoreEvent::Camera(camera)),
            }
        }
        let mut world = World {
            game,
            pool: &mut self.pool,
            assets: &self.assets,
        };
        core.on_frame(dt, &mut world);
        Ok(())
    }

    /// Removes every hook and gives the runtime back, with the hooks it could not remove, if any.
    pub fn stop(mut self) -> (R, Vec<RuntimeError>) {
        let errors = std::mem::take(&mut self.hooks)
            .into_iter()
            .filter_map(|handle| self.runtime.remove_hook(handle).err())
            .collect();
        (self.runtime, errors)
    }
}
