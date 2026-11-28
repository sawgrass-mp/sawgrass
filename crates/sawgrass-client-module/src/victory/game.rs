//! The [`GameApi`] for a VIctory build, over the title's script natives.
//!
//! Each method calls the natives the table located and checked ([`super::natives`]); what the table does not
//! locate yet answers [`GameError::Refused`] or `None`, so the module runs with what is found, as it does without a
//! function (`Module::shows_remote_peds`, `Module::moves_entities`). It lives only for one frame, on the script
//! thread, with that frame's [`NativeCaller`].
//!
//! ASSUMPTION: an entity handle is two slots, as the local player native returns it and the position native takes
//! it, and both are passed back as they came. The first slot is kept as [`GameHandle::raw`]. The second is a full
//! 64-bit value in the game (the first run with a server found it does not fit 32 bits), so [`Handles`] keeps it and
//! the handle's generation names it. A float argument is an `f32` in the low half of a slot. Both hold for the
//! natives read so far. A position argument is packed as the title packs a vector: x and y in one slot, z in the
//! low half of the next. Headings are degrees in the natives and radians in [`Pose`].
//!
//! Peds are spawned through the title's spawn request function ([`super::spawn`]), not a native: the request is
//! queued, so [`GameApi::spawn`] answers `Refused` until the ped stands at the asked place, and the network core asks
//! again on the next frames.

use std::cell::RefCell;
use std::time::Instant;

use crate::game::{EntityKind, GameApi, GameAsset, GameError, GameHandle, Pose, SinglePlayerSystem};

use super::natives::{Native, NativeCaller, vector3};
use super::spawn::{self, Spawner};

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
    /// The closest ped to a place, which finds a spawned ped.
    pub ped_closest: Option<Native>,
    pub entity_set_heading: Option<Native>,
}

/// The second slots of the entity handles seen so far, for the whole run: a handle's generation is its second
/// slot's place here, so the same entity gives the same [`GameHandle`] on every frame.
#[derive(Debug, Default)]
pub struct Handles(RefCell<Vec<u64>>);

impl Handles {
    /// Distinct second slots kept at most; the game layer reads only the local player so far.
    const MAX: usize = 256;

    fn generation(&self, second: u64) -> Option<u32> {
        let mut seen = self.0.borrow_mut();
        let at = match seen.iter().position(|&s| s == second) {
            Some(at) => at,
            None if seen.len() < Self::MAX => {
                seen.push(second);
                seen.len() - 1
            }
            None => return None,
        };
        u32::try_from(at).ok()
    }

    fn slots(&self, handle: GameHandle) -> Option<[u64; 2]> {
        let second = *self.0.borrow().get(usize::try_from(handle.generation).ok()?)?;
        Some([handle.raw, second])
    }
}

/// The game, for one frame on the script thread.
pub struct NativeGame<'a> {
    caller: &'a dyn NativeCaller,
    natives: &'a GameNatives,
    handles: &'a Handles,
    spawner: Option<&'a Spawner>,
}

impl std::fmt::Debug for NativeGame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeGame")
            .field("natives", self.natives)
            .finish_non_exhaustive()
    }
}

impl<'a> NativeGame<'a> {
    pub fn new(
        caller: &'a dyn NativeCaller,
        natives: &'a GameNatives,
        handles: &'a Handles,
        spawner: Option<&'a Spawner>,
    ) -> Self {
        Self {
            caller,
            natives,
            handles,
            spawner,
        }
    }

    /// The closest ped to `at` within `radius`, but not the local player.
    fn closest_ped(&self, at: [f32; 3], radius: f32) -> Option<GameHandle> {
        let native = self.natives.ped_closest?;
        let mut out = [0u64; 2];
        let [x, y, z] = at;
        // ASSUMPTION: the place, the radius, two flags on (any ped, alive), where the ped goes, three flags off, and
        // the ped type (-1, any); as the title's own scripts call it.
        let mut args = [
            float(x) | (float(y) << 32),
            float(z),
            float(radius),
            1,
            1,
            out.as_mut_ptr() as u64,
            0,
            0,
            0,
            u64::MAX,
        ];
        self.call(native, &mut args);
        if out[0] == 0 || self.local_player().is_some_and(|me| me.raw == out[0]) {
            return None;
        }
        Some(GameHandle::new(out[0], self.handles.generation(out[1])?))
    }

    /// Turns an entity, when the table has the heading setter.
    fn set_heading(&self, handle: GameHandle, heading: f32) -> Result<(), GameError> {
        let native = self
            .natives
            .entity_set_heading
            .ok_or_else(|| GameError::Refused(NOT_LOCATED.into()))?;
        let [lo, hi] = self
            .handles
            .slots(handle)
            .ok_or(GameError::NoSuchEntity(handle))?;
        self.call(native, &mut [lo, hi, float(heading.to_degrees())]);
        Ok(())
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

fn float(value: f32) -> u64 {
    u64::from(value.to_bits())
}

fn refused<T>() -> Result<T, GameError> {
    Err(GameError::Refused(NOT_LOCATED.into()))
}

impl GameApi for NativeGame<'_> {
    fn spawn(&mut self, kind: EntityKind, _asset: GameAsset, pose: Pose) -> Result<GameHandle, GameError> {
        let Some(spawner) = self
            .spawner
            .filter(|_| kind == EntityKind::Ped && self.natives.ped_closest.is_some())
        else {
            return refused();
        };
        if let Some(at) = spawner.awaited() {
            let closest = self.closest_ped(at, spawn::NEAR * 2.0);
            let place = closest.and_then(|ped| self.pose(ped)).map(|p| p.position);
            spawner.report.borrow_mut().closest = place.map(|p| (p[0] - at[0]).hypot(p[1] - at[1]));
            let found = closest
                .filter(|ped| !spawner.is_spawned(ped.raw) && place.is_some_and(|p| spawn::near(p, at)));
            if let Some(ped) = found {
                spawner.found(ped.raw);
                // The place was the asked one; only the heading is left.
                let _ = self.set_heading(ped, pose.heading);
                return Ok(ped);
            }
        }
        // SAFETY: the frame runs on a script thread, and the spawner's addresses are the table's for this build.
        unsafe { spawner.request(pose.position, Instant::now())? };
        Err(GameError::Refused(
            "spawn requested, the game creates the ped in a moment".into(),
        ))
    }

    fn remove(&mut self, handle: GameHandle) -> Result<(), GameError> {
        Err(GameError::NoSuchEntity(handle))
    }

    /// ASSUMPTION: the handle names a live entity. That holds for the local player, the only one the module reads
    /// before `ped_spawn` is located; with remote entities this needs the game's check that the entity exists.
    fn pose(&self, handle: GameHandle) -> Option<Pose> {
        let position = self.natives.entity_position?;
        let [lo, hi] = self.handles.slots(handle)?;
        let ret = self.call(position, &mut [lo, hi, 0, 0]);
        let heading = match self.natives.entity_heading {
            Some(native) => f32::from_bits(self.call(native, &mut [lo, hi])[0] as u32).to_radians(),
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
        let Some([lo, hi]) = self.handles.slots(handle) else {
            return Err(GameError::NoSuchEntity(handle));
        };
        let [x, y, z] = pose.position;
        // The entity, the packed position, and five flags left off, as the title's scripts call it (run in the game
        // with a spawned ped).
        self.call(
            native,
            &mut [lo, hi, float(x) | (float(y) << 32), float(z), 0, 0, 0, 0, 0],
        );
        let _ = self.set_heading(handle, pose.heading);
        Ok(())
    }

    fn local_player(&self) -> Option<GameHandle> {
        let ret = self.call(self.natives.local_player?, &mut []);
        if ret[0] == 0 {
            return None;
        }
        Some(GameHandle::new(ret[0], self.handles.generation(ret[1])?))
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
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::victory::natives::{Call, Direct};

    /// The stand-in title: one entity, the local player, at a position and heading, and the last pose set.
    static SET: Mutex<Vec<[u64; 9]>> = Mutex::new(Vec::new());
    /// Tests that share the spawner's statics run one at a time.
    pub(crate) static SERIAL: Mutex<()> = Mutex::new(());
    /// The second slot as the game gives it: wider than 32 bits.
    const PLAYER: [u64; 2] = [0x1234, 0x0000_7ff6_1234_5678];

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
        // SAFETY: as above, nine argument slots.
        unsafe {
            let call = &mut *call;
            assert_eq!(call.argc, 9);
            let mut args = [0u64; 9];
            args.copy_from_slice(std::slice::from_raw_parts(call.args, 9));
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
            ..GameNatives::default()
        };
        let handles = Handles::default();
        let mut game = NativeGame::new(&Direct, &natives, &handles, None);
        let me = game.local_player().expect("player");
        assert_eq!(game.local_player(), Some(me), "the same handle on every call");
        assert_eq!(me.raw, PLAYER[0]);
        assert_eq!(
            game.pose(me),
            Some(Pose {
                position: [1.5, -2.0, 30.25],
                heading: std::f32::consts::FRAC_PI_2
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
        assert_eq!(
            (
                f32::from_bits(set[0][2] as u32),
                f32::from_bits((set[0][2] >> 32) as u32)
            ),
            (4.0, 5.0)
        );
        assert_eq!(f32::from_bits(set[0][3] as u32), 6.0);
    }

    #[test]
    fn what_is_not_located_is_refused() {
        let natives = GameNatives {
            local_player: Some(Native::from_handler(local_player)),
            ..GameNatives::default()
        };
        let handles = Handles::default();
        let mut game = NativeGame::new(&Direct, &natives, &handles, None);
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
        assert_eq!(
            NativeGame::new(&Direct, &none, &handles, None).local_player(),
            None
        );
        // A handle whose second slot was never seen names no entity.
        let natives = GameNatives {
            entity_position: Some(Native::from_handler(position)),
            ..GameNatives::default()
        };
        let fresh = Handles::default();
        let game = NativeGame::new(&Direct, &natives, &fresh, None);
        assert_eq!(game.pose(GameHandle::new(PLAYER[0], 0)), None);
    }

    /// The stand-in ped the title creates for the spawner, at the place it was asked for.
    const SPAWNED: u64 = 0x77;

    unsafe extern "C" fn closest(call: *mut Call) {
        // SAFETY: the closest-ped native's ten slots, the sixth where the ped goes.
        unsafe {
            let call = &mut *call;
            assert_eq!(call.argc, 10);
            let out = *call.args.add(5) as *mut u64;
            if crate::victory::spawn::tests::REQUESTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                > 1
            {
                *out = SPAWNED;
                *out.add(1) = PLAYER[1];
            }
        }
    }

    unsafe extern "C" fn anywhere(call: *mut Call) {
        // SAFETY: as `position`, for any entity: it stands at (10, 20, 30).
        unsafe {
            let call = &mut *call;
            *call.ret = u64::from(10.0f32.to_bits()) | (u64::from(20.0f32.to_bits()) << 32);
            *call.ret.add(1) = u64::from(30.0f32.to_bits());
        }
    }

    #[test]
    fn spawns_a_ped_through_the_request_and_finds_it_at_the_place() {
        use crate::victory::spawn::{Spawner, tests as stand_in};
        let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        stand_in::REQUESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        stand_in::title_spawns_once();
        let natives = GameNatives {
            local_player: Some(Native::from_handler(local_player)),
            entity_position: Some(Native::from_handler(anywhere)),
            ped_closest: Some(Native::from_handler(closest)),
            ..GameNatives::default()
        };
        let handles = Handles::default();
        let spawner = Spawner::new(stand_in::functions(), stand_in::LAYOUT_FOR_TESTS);
        let mut game = NativeGame::new(&Direct, &natives, &handles, Some(&spawner));
        let pose = Pose {
            position: [10.0, 20.0, 30.0],
            heading: 0.0,
        };
        assert!(matches!(
            game.spawn(EntityKind::Ped, GameAsset(1), pose),
            Err(GameError::Refused(_))
        ));
        assert_eq!(
            spawner.awaited(),
            Some([10.0, 20.0, 30.0]),
            "requested, and awaited"
        );
        let ped = game
            .spawn(EntityKind::Ped, GameAsset(1), pose)
            .expect("found at the place");
        assert_eq!(ped.raw, SPAWNED);
        assert_eq!(spawner.awaited(), None);
        // The same ped is never handed out again: a second spawn makes a new request.
        assert!(game.spawn(EntityKind::Ped, GameAsset(1), pose).is_err());
        assert_eq!(
            stand_in::REQUESTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            3
        );
        // Vehicles and objects are not spawned this way.
        assert!(game.spawn(EntityKind::Vehicle, GameAsset(1), pose).is_err());
    }
}
