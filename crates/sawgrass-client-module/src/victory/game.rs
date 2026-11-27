//! The [`GameApi`] for a VIctory build, over the title's script natives.
//!
//! Each method calls the natives the table located and checked ([`super::natives`]); what the table does not
//! locate yet answers [`GameError::Refused`] or `None`, so the module runs with what is found, as it does without a
//! function (`Module::shows_remote_peds`, `Module::moves_entities`). It lives only for one frame, on the script
//! thread, with that frame's [`NativeCaller`].
//!
//! ASSUMPTION: an entity handle is two slots, as the local player native returns it and the position native takes
//! it; the first slot is kept as [`GameHandle::raw`] and the second as its generation. A float argument is an `f32`
//! in the low half of a slot. Both hold for the natives read so far; the setter's arguments are checked once it is
//! located.

use crate::game::{EntityKind, GameApi, GameAsset, GameError, GameHandle, Pose, SinglePlayerSystem};

use super::natives::{Native, NativeCaller, vector3};

/// Table names of the natives the game layer uses, beside `local_player` and `entity_position`.
pub const ENTITY_HEADING: &str = "entity_heading";

const NOT_LOCATED: &str = "not located in this build's table yet";

/// The natives the table located and checked.
#[derive(Debug, Clone, Copy, Default)]
pub struct GameNatives {
    pub local_player: Option<Native>,
    pub entity_position: Option<Native>,
    pub entity_heading: Option<Native>,
    pub entity_set_pose: Option<Native>,
}

/// The game, for one frame on the script thread.
pub struct NativeGame<'a> {
    caller: &'a dyn NativeCaller,
    natives: &'a GameNatives,
}

impl std::fmt::Debug for NativeGame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeGame")
            .field("natives", self.natives)
            .finish_non_exhaustive()
    }
}

impl<'a> NativeGame<'a> {
    pub fn new(caller: &'a dyn NativeCaller, natives: &'a GameNatives) -> Self {
        Self { caller, natives }
    }

    fn call(&self, native: Native, args: &mut [u64]) -> [u64; 4] {
        let mut ret = [0u64; 4];
        // SAFETY: the table is trusted that each name's `hash=` is that native's, so a handler that passed the check
        // is the native the name says; each caller below passes what that native takes, per the module's
        // ASSUMPTION; `ret` has room for the largest return the game layer reads (a vector, two slots).
        unsafe { self.caller.call(native, args, &mut ret) };
        ret
    }
}

/// The two slots of an entity handle.
fn slots(handle: GameHandle) -> [u64; 2] {
    [handle.raw, u64::from(handle.generation)]
}

fn float(value: f32) -> u64 {
    u64::from(value.to_bits())
}

fn refused<T>() -> Result<T, GameError> {
    Err(GameError::Refused(NOT_LOCATED.into()))
}

impl GameApi for NativeGame<'_> {
    fn spawn(&mut self, _kind: EntityKind, _asset: GameAsset, _pose: Pose) -> Result<GameHandle, GameError> {
        refused()
    }

    fn remove(&mut self, handle: GameHandle) -> Result<(), GameError> {
        Err(GameError::NoSuchEntity(handle))
    }

    /// ASSUMPTION: the handle names a live entity. That holds for the local player, the only one the module reads
    /// before `ped_spawn` is located; with remote entities this needs the game's check that the entity exists.
    fn pose(&self, handle: GameHandle) -> Option<Pose> {
        let [lo, hi] = slots(handle);
        let ret = self.call(self.natives.entity_position?, &mut [lo, hi, 0, 0]);
        let heading = match self.natives.entity_heading {
            Some(native) => f32::from_bits(self.call(native, &mut [lo, hi])[0] as u32),
            None => 0.0,
        };
        Some(Pose {
            position: vector3(&ret),
            heading,
        })
    }

    fn set_pose(&mut self, handle: GameHandle, pose: Pose) -> Result<(), GameError> {
        let Some(native) = self.natives.entity_set_pose else {
            return refused();
        };
        let [lo, hi] = slots(handle);
        let [x, y, z] = pose.position;
        // ASSUMPTION: the entity, the position, and three flags left off (keep tasks, keep IK, dead check). Not yet
        // run in the game, and the heading is not set: that needs its own native.
        self.call(native, &mut [lo, hi, float(x), float(y), float(z), 0, 0, 0]);
        Ok(())
    }

    fn local_player(&self) -> Option<GameHandle> {
        let ret = self.call(self.natives.local_player?, &mut []);
        let generation = u32::try_from(ret[1]).ok()?;
        (ret[0] != 0).then_some(GameHandle::new(ret[0], generation))
    }

    fn held_weapon(&self, _ped: GameHandle) -> Option<GameAsset> {
        None
    }

    fn seat(&self, _ped: GameHandle) -> Option<(GameHandle, u8)> {
        None
    }

    fn enter_seat(&mut self, _ped: GameHandle, _vehicle: GameHandle, _seat: u8) -> Result<(), GameError> {
        refused()
    }

    fn leave_seat(&mut self, _ped: GameHandle) -> Result<(), GameError> {
        refused()
    }

    fn set_streaming_focus(&mut self, _points: &[[f32; 3]]) {}

    fn disable(&mut self, _system: SinglePlayerSystem) -> Result<(), GameError> {
        refused()
    }

    fn set_clock(&mut self, _minute: f32) -> Result<(), GameError> {
        refused()
    }

    fn set_weather(
        &mut self,
        _current: GameAsset,
        _next: GameAsset,
        _transition: f32,
    ) -> Result<(), GameError> {
        refused()
    }

    fn set_door(&mut self, _door: GameAsset, _locked: bool, _open: f32) -> Result<(), GameError> {
        refused()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::victory::natives::{Call, Direct};

    /// The stand-in title: one entity, the local player, at a position and heading, and the last pose set.
    static SET: Mutex<Vec<[u64; 8]>> = Mutex::new(Vec::new());
    const PLAYER: [u64; 2] = [0x1234, 7];

    unsafe extern "C" fn local_player(call: *mut Call) {
        // SAFETY: the bridge's call, with room for two slots.
        unsafe {
            let call = &mut *call;
            assert_eq!(call.argc, 0);
            *call.ret = PLAYER[0];
            *call.ret.add(1) = PLAYER[1];
        }
    }

    unsafe extern "C" fn position(call: *mut Call) {
        // SAFETY: as above.
        unsafe {
            let call = &mut *call;
            assert_eq!((*call.args, *call.args.add(1)), (PLAYER[0], PLAYER[1]));
            *call.ret = u64::from(1.5f32.to_bits()) | (u64::from((-2.0f32).to_bits()) << 32);
            *call.ret.add(1) = u64::from(30.25f32.to_bits());
        }
    }

    unsafe extern "C" fn heading(call: *mut Call) {
        // SAFETY: as above.
        unsafe { *(*call).ret = u64::from(90.0f32.to_bits()) };
    }

    unsafe extern "C" fn set_pose(call: *mut Call) {
        // SAFETY: as above, eight argument slots.
        unsafe {
            let call = &mut *call;
            let mut args = [0u64; 8];
            args.copy_from_slice(std::slice::from_raw_parts(call.args, 8));
            SET.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(args);
        }
    }

    #[test]
    fn reads_and_moves_the_local_player_through_natives() {
        let natives = GameNatives {
            local_player: Some(Native::from_handler(local_player)),
            entity_position: Some(Native::from_handler(position)),
            entity_heading: Some(Native::from_handler(heading)),
            entity_set_pose: Some(Native::from_handler(set_pose)),
        };
        let mut game = NativeGame::new(&Direct, &natives);
        let me = game.local_player().expect("player");
        assert_eq!(me, GameHandle::new(PLAYER[0], 7));
        assert_eq!(
            game.pose(me),
            Some(Pose {
                position: [1.5, -2.0, 30.25],
                heading: 90.0
            })
        );
        game.set_pose(
            me,
            Pose {
                position: [4.0, 5.0, 6.0],
                heading: 0.0,
            },
        )
        .expect("set");
        let set = SET
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(set.len(), 1);
        assert_eq!(&set[0][..2], &PLAYER);
        assert_eq!(f32::from_bits(set[0][3] as u32), 5.0);
    }

    #[test]
    fn what_is_not_located_is_refused() {
        let natives = GameNatives {
            local_player: Some(Native::from_handler(local_player)),
            ..GameNatives::default()
        };
        let mut game = NativeGame::new(&Direct, &natives);
        let me = game.local_player().expect("player");
        assert_eq!(game.pose(me), None);
        assert!(
            game.set_pose(
                me,
                Pose {
                    position: [0.0; 3],
                    heading: 0.0
                }
            )
            .is_err()
        );
        assert!(
            game.spawn(
                EntityKind::Ped,
                GameAsset(1),
                Pose {
                    position: [0.0; 3],
                    heading: 0.0
                }
            )
            .is_err()
        );
        let none = GameNatives::default();
        assert_eq!(NativeGame::new(&Direct, &none).local_player(), None);
    }
}
