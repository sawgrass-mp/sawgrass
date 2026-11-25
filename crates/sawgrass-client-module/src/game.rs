//! What the module asks of the game and what the game reports, independent of the runtime.
//!
//! [`GameApi`] is the only way the module changes the game (docs/architecture.md, "Client and launcher"). The real
//! implementation calls the functions located by [`crate::table`] through the runtime adapter and arrives with the
//! game; [`FakeGame`] stands in for it in tests and for the bots.
//!
//! ASSUMPTION: every type here is the module's own view. How the game stores a position, a heading or an input
//! state is unknown until the game runs; the real `GameApi` converts.

use std::collections::BTreeMap;
use std::fmt;

/// A game entity as the game knows it. Opaque: the real `GameApi` decides what the number means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GameHandle(pub u64);

/// A model or other asset as the game knows it. Opaque, like [`GameHandle`]; [`crate::assets`] maps the protocol's
/// asset ids to it per game build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GameAsset(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Ped,
    Vehicle,
    Object,
}

/// Position in metres in world space and heading in radians.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub position: [f32; 3],
    pub heading: f32,
}

/// The local player's input for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct InputState {
    /// Movement stick, each axis in -1..=1.
    pub movement: [f32; 2],
    /// Look stick or mouse delta.
    pub look: [f32; 2],
    /// One bit per action. Which bit is which action is decided with the real input hook.
    pub buttons: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraPose {
    pub position: [f32; 3],
    /// Pitch, roll and yaw in radians.
    pub rotation: [f32; 3],
    /// Vertical field of view in radians.
    pub fov: f32,
}

/// Single-player systems the module switches off: in multiplayer the server decides who and what exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SinglePlayerSystem {
    Story,
    AmbientPeds,
    AmbientTraffic,
    Police,
    Saves,
}

impl SinglePlayerSystem {
    pub const ALL: [Self; 5] = [
        Self::Story,
        Self::AmbientPeds,
        Self::AmbientTraffic,
        Self::Police,
        Self::Saves,
    ];

    /// The function table name of the game function the real `GameApi` calls to switch the system off.
    ///
    /// ASSUMPTION: one function per system. Some may turn out to need a hook that refuses work instead; then the
    /// entry moves to [`crate::hooks::HookPoint`].
    pub fn function(self) -> &'static str {
        match self {
            Self::Story => "sp_story_disable",
            Self::AmbientPeds => "sp_ambient_peds_disable",
            Self::AmbientTraffic => "sp_ambient_traffic_disable",
            Self::Police => "sp_police_disable",
            Self::Saves => "sp_saves_disable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameError {
    NoSuchEntity(GameHandle),
    /// The game refused or failed, with what the real `GameApi` could tell.
    Refused(String),
}

impl fmt::Display for GameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchEntity(h) => write!(f, "no game entity {}", h.0),
            Self::Refused(why) => write!(f, "game refused: {why}"),
        }
    }
}

impl std::error::Error for GameError {}

/// Every change the module makes to the game.
pub trait GameApi {
    fn spawn(&mut self, kind: EntityKind, asset: GameAsset, pose: Pose) -> Result<GameHandle, GameError>;
    fn remove(&mut self, handle: GameHandle) -> Result<(), GameError>;
    fn pose(&self, handle: GameHandle) -> Option<Pose>;
    fn set_pose(&mut self, handle: GameHandle, pose: Pose) -> Result<(), GameError>;
    /// The local player's ped, once the game has one.
    fn local_player(&self) -> Option<GameHandle>;
    /// Keeps the world loaded around these points, the positions of remote players, besides the camera.
    fn set_streaming_focus(&mut self, points: &[[f32; 3]]);
    fn disable(&mut self, system: SinglePlayerSystem) -> Result<(), GameError>;
}

/// A game in memory: entities with a kind, an asset and a pose.
#[derive(Debug, Default)]
pub struct FakeGame {
    pub entities: BTreeMap<GameHandle, (EntityKind, GameAsset, Pose)>,
    pub local_player: Option<GameHandle>,
    pub streaming_focus: Vec<[f32; 3]>,
    pub disabled: Vec<SinglePlayerSystem>,
    next_handle: u64,
}

impl FakeGame {
    /// An entity the game made by itself (ambient traffic, the local player), as the creation hook would see it.
    pub fn create_natively(&mut self, kind: EntityKind, asset: GameAsset, pose: Pose) -> GameHandle {
        self.next_handle += 1;
        let handle = GameHandle(self.next_handle);
        self.entities.insert(handle, (kind, asset, pose));
        handle
    }
}

impl GameApi for FakeGame {
    fn spawn(&mut self, kind: EntityKind, asset: GameAsset, pose: Pose) -> Result<GameHandle, GameError> {
        Ok(self.create_natively(kind, asset, pose))
    }

    fn remove(&mut self, handle: GameHandle) -> Result<(), GameError> {
        self.entities
            .remove(&handle)
            .map(|_| ())
            .ok_or(GameError::NoSuchEntity(handle))
    }

    fn pose(&self, handle: GameHandle) -> Option<Pose> {
        self.entities.get(&handle).map(|(_, _, pose)| *pose)
    }

    fn set_pose(&mut self, handle: GameHandle, pose: Pose) -> Result<(), GameError> {
        let entity = self
            .entities
            .get_mut(&handle)
            .ok_or(GameError::NoSuchEntity(handle))?;
        entity.2 = pose;
        Ok(())
    }

    fn local_player(&self) -> Option<GameHandle> {
        self.local_player
    }

    fn set_streaming_focus(&mut self, points: &[[f32; 3]]) {
        self.streaming_focus = points.to_vec();
    }

    fn disable(&mut self, system: SinglePlayerSystem) -> Result<(), GameError> {
        if !self.disabled.contains(&system) {
            self.disabled.push(system);
        }
        Ok(())
    }
}
