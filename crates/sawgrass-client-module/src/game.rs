//! What the module asks of the game and what the game reports, independent of the runtime.
//!
//! [`GameApi`] is the only way the module changes the game (docs/architecture.md, "Client and launcher"). The real
//! implementation calls the functions located by [`crate::table`] through the runtime adapter and arrives with the
//! game; [`FakeGame`] stands in for it in tests and for the bots.
//!
//! ASSUMPTION: every type here is the module's own view. How the game stores a position, a heading or an input
//! state is unknown until the game runs; the real `GameApi` converts.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// A game entity as the game knows it: the game's own handle, opaque to everything but the real `GameApi`, and a
/// generation.
///
/// Hooks report events that the module handles on the next frame (see [`crate::hooks`]), and by then the game may
/// have removed the entity and given its handle to a new one. The generation tells the two apart, so a late event
/// about the old entity never reaches the new one. The game has no generation of its own (an ASSUMPTION until the
/// game runs); the adapter counts them with [`Generations`] in its creation hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GameHandle {
    pub raw: u64,
    pub generation: u32,
}

impl GameHandle {
    pub const fn new(raw: u64, generation: u32) -> Self {
        Self { raw, generation }
    }
}

/// The adapter's count of how often the game has handed out each raw handle.
#[derive(Debug, Default)]
pub struct Generations {
    current: HashMap<u64, u32>,
}

impl Generations {
    /// Called from a creation hook: `raw` now names a new entity.
    pub fn created(&mut self, raw: u64) -> GameHandle {
        let generation = self
            .current
            .entry(raw)
            .and_modify(|g| *g = g.wrapping_add(1))
            .or_insert(1);
        GameHandle::new(raw, *generation)
    }

    /// The entity `raw` names now, as a removal hook or a `GameApi` call that only has the raw handle reports it.
    pub fn current(&self, raw: u64) -> Option<GameHandle> {
        self.current.get(&raw).map(|g| GameHandle::new(raw, *g))
    }

    /// The entity `raw` names now, numbering it as generation 1 if no creation hook saw it: entities that existed
    /// before the module started (the local player's ped, the map's props) are met first through other hooks or
    /// calls. Its next reuse then counts from there.
    pub fn current_or_adopt(&mut self, raw: u64) -> GameHandle {
        GameHandle::new(raw, *self.current.entry(raw).or_insert(1))
    }
}

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
            Self::NoSuchEntity(h) => write!(f, "no game entity {} (generation {})", h.raw, h.generation),
            Self::Refused(why) => write!(f, "game refused: {why}"),
        }
    }
}

impl std::error::Error for GameError {}

pub use sawgrass_protocol::combat::Zone;

/// One hit of a shot, as the game resolved it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GameHit {
    pub target: GameHandle,
    /// Where on a ped; [`Zone::Other`] for anything else.
    pub zone: Zone,
    pub point: [f32; 3],
}

/// One pull of a trigger, as the fire hook saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct GameShot {
    pub shooter: GameHandle,
    pub weapon: GameAsset,
    /// Where the shooter was.
    pub from: [f32; 3],
    /// What it hit, in the order the game resolved them; empty for a miss.
    pub hits: Vec<GameHit>,
}

/// Every change the module makes to the game.
pub trait GameApi {
    fn spawn(&mut self, kind: EntityKind, asset: GameAsset, pose: Pose) -> Result<GameHandle, GameError>;
    fn remove(&mut self, handle: GameHandle) -> Result<(), GameError>;
    fn pose(&self, handle: GameHandle) -> Option<Pose>;
    fn set_pose(&mut self, handle: GameHandle, pose: Pose) -> Result<(), GameError>;
    /// The local player's ped, once the game has one.
    fn local_player(&self) -> Option<GameHandle>;
    /// The weapon in a ped's hand; `None` unarmed or for no such ped.
    fn held_weapon(&self, ped: GameHandle) -> Option<GameAsset>;
    /// The vehicle and seat (0 the driver's) a ped sits in.
    fn seat(&self, ped: GameHandle) -> Option<(GameHandle, u8)>;
    /// Puts a ped in `seat` of `vehicle` at once, without the walk to the door.
    fn enter_seat(&mut self, ped: GameHandle, vehicle: GameHandle, seat: u8) -> Result<(), GameError>;
    /// Takes a ped out of the seat it sits in.
    fn leave_seat(&mut self, ped: GameHandle) -> Result<(), GameError>;
    /// Keeps the world loaded around these points, the positions of remote players, besides the camera.
    fn set_streaming_focus(&mut self, points: &[[f32; 3]]);
    fn disable(&mut self, system: SinglePlayerSystem) -> Result<(), GameError>;
    /// Sets the clock to `minute` of the day, from 0 to below 1440, the fraction being seconds. The module sets it
    /// every frame, so the game's own clock must not move it in between.
    fn set_clock(&mut self, minute: f32) -> Result<(), GameError>;
    /// Sets the weather to `current` turning into `next`, `transition` of the way, from 0 to 1.
    fn set_weather(&mut self, current: GameAsset, next: GameAsset, transition: f32) -> Result<(), GameError>;
    /// Locks or unlocks a door and opens it `open` of the way, from 0 shut to 1 wide.
    ///
    /// ASSUMPTION: the game keeps door states by door, whether or not the door is streamed in, as earlier games on
    /// this engine did; otherwise the module keeps them and applies each again when its door streams in.
    fn set_door(&mut self, door: GameAsset, locked: bool, open: f32) -> Result<(), GameError>;
}

/// A door's state in [`FakeGame`]: locked, and how far open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DoorState {
    pub locked: bool,
    pub open: f32,
}

/// The weather in [`FakeGame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeatherState {
    pub current: GameAsset,
    pub next: GameAsset,
    pub transition: f32,
}

/// A game in memory: entities with a kind, an asset and a pose.
#[derive(Debug, Default)]
pub struct FakeGame {
    pub entities: BTreeMap<GameHandle, (EntityKind, GameAsset, Pose)>,
    pub local_player: Option<GameHandle>,
    pub streaming_focus: Vec<[f32; 3]>,
    pub disabled: Vec<SinglePlayerSystem>,
    /// Systems whose switch the fake refuses, to test that path.
    pub refuse_disable: Vec<SinglePlayerSystem>,
    /// The weapon in each armed ped's hand.
    pub weapons: BTreeMap<GameHandle, GameAsset>,
    /// The vehicle and seat of each seated ped.
    pub seats: BTreeMap<GameHandle, (GameHandle, u8)>,
    /// The minute of the day last set.
    pub clock: Option<f32>,
    pub weather: Option<WeatherState>,
    pub doors: BTreeMap<GameAsset, DoorState>,
    generations: Generations,
    /// Raw handles of removed entities, handed out again first, as a game's entity pool does.
    free: Vec<u64>,
    next_raw: u64,
}

impl FakeGame {
    /// An entity the game made by itself (ambient traffic, the local player), as the creation hook would see it.
    pub fn create_natively(&mut self, kind: EntityKind, asset: GameAsset, pose: Pose) -> GameHandle {
        let raw = self.free.pop().unwrap_or_else(|| {
            self.next_raw += 1;
            self.next_raw
        });
        let handle = self.generations.created(raw);
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
            .ok_or(GameError::NoSuchEntity(handle))?;
        self.free.push(handle.raw);
        Ok(())
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

    fn seat(&self, ped: GameHandle) -> Option<(GameHandle, u8)> {
        self.seats.get(&ped).copied()
    }

    fn enter_seat(&mut self, ped: GameHandle, vehicle: GameHandle, seat: u8) -> Result<(), GameError> {
        for handle in [ped, vehicle] {
            if !self.entities.contains_key(&handle) {
                return Err(GameError::NoSuchEntity(handle));
            }
        }
        self.seats.insert(ped, (vehicle, seat));
        Ok(())
    }

    fn leave_seat(&mut self, ped: GameHandle) -> Result<(), GameError> {
        self.seats.remove(&ped);
        Ok(())
    }

    fn held_weapon(&self, ped: GameHandle) -> Option<GameAsset> {
        self.entities
            .contains_key(&ped)
            .then(|| self.weapons.get(&ped).copied())
            .flatten()
    }

    fn set_streaming_focus(&mut self, points: &[[f32; 3]]) {
        self.streaming_focus = points.to_vec();
    }

    fn disable(&mut self, system: SinglePlayerSystem) -> Result<(), GameError> {
        if self.refuse_disable.contains(&system) {
            return Err(GameError::Refused("refused by the fake".into()));
        }
        if !self.disabled.contains(&system) {
            self.disabled.push(system);
        }
        Ok(())
    }

    fn set_clock(&mut self, minute: f32) -> Result<(), GameError> {
        self.clock = Some(minute);
        Ok(())
    }

    fn set_weather(&mut self, current: GameAsset, next: GameAsset, transition: f32) -> Result<(), GameError> {
        self.weather = Some(WeatherState {
            current,
            next,
            transition,
        });
        Ok(())
    }

    fn set_door(&mut self, door: GameAsset, locked: bool, open: f32) -> Result<(), GameError> {
        self.doors.insert(door, DoorState { locked, open });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_count_each_reuse_of_a_raw_handle() {
        let mut generations = Generations::default();
        assert_eq!(generations.current(5), None);
        assert_eq!(generations.created(5), GameHandle::new(5, 1));
        assert_eq!(generations.created(6), GameHandle::new(6, 1));
        assert_eq!(generations.created(5), GameHandle::new(5, 2));
        assert_eq!(generations.current(5), Some(GameHandle::new(5, 2)));
    }

    #[test]
    fn entities_older_than_the_module_are_adopted_as_generation_one() {
        let mut generations = Generations::default();
        assert_eq!(generations.current_or_adopt(9), GameHandle::new(9, 1));
        assert_eq!(generations.current_or_adopt(9), GameHandle::new(9, 1));
        assert_eq!(generations.current(9), Some(GameHandle::new(9, 1)));
        assert_eq!(
            generations.created(9),
            GameHandle::new(9, 2),
            "its reuse counts on from the adoption"
        );
        assert_eq!(generations.current_or_adopt(9), GameHandle::new(9, 2));
    }

    #[test]
    fn the_fake_reuses_raw_handles_and_refuses_stale_ones() {
        let mut game = FakeGame::default();
        let pose = Pose {
            position: [0.0; 3],
            heading: 0.0,
        };
        let first = game.create_natively(EntityKind::Ped, GameAsset(1), pose);
        game.remove(first).expect("removed");
        let second = game.create_natively(EntityKind::Ped, GameAsset(2), pose);
        assert_eq!(second.raw, first.raw);
        assert_ne!(second, first);
        assert_eq!(game.remove(first), Err(GameError::NoSuchEntity(first)));
        assert_eq!(game.pose(first), None);
        assert!(game.entities.contains_key(&second));
    }
}
