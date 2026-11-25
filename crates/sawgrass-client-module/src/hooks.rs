//! The game functions the module hooks, and what each hook reports.
//!
//! The runtime adapter installs a hook on each [`HookPoint`] at the address the function table gives, and turns
//! every call into a [`GameEvent`] (or, for the frame, a call to [`crate::module::Module::frame`]). How a hook reads
//! the game's arguments is decided per build once the game runs; nothing above this file depends on it.
//!
//! Hooks re-enter: when the network core spawns or removes an entity during `Module::frame`, the game runs its own
//! creation or removal function, which is itself hooked, while `frame` still holds the module. So a hook never
//! touches `Module`. It pushes into an [`EventSink`], a handle the adapter takes from the module at start-up, and the
//! next frame drains it. Hooks may also fire on game threads other than the frame's, which is why the sink is
//! `Send + Sync`.

use std::sync::{Arc, Mutex, PoisonError};

use crate::game::{CameraPose, EntityKind, GameAsset, GameHandle, GameShot, InputState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookPoint {
    /// Once per game frame; drives [`crate::module::Module::frame`].
    Frame,
    PedCreate,
    PedRemove,
    VehicleCreate,
    VehicleRemove,
    ObjectCreate,
    ObjectRemove,
    /// Where the game reads the local player's input.
    InputPoll,
    /// Where the game updates the camera.
    CameraUpdate,
    /// Where the game decides what to stream in, so that the world loads around remote players too.
    StreamingUpdate,
    /// Where the game resolves one pull of a trigger and what it hit, reported as [`GameEvent::Shot`].
    ///
    /// ASSUMPTION: one function sees the shooter, the weapon and every hit of the shot, a shotgun's pellets
    /// included. If the hits come one by one, the adapter gathers them per shot before it pushes the event.
    WeaponFire,
}

impl HookPoint {
    pub const ALL: [Self; 11] = [
        Self::Frame,
        Self::PedCreate,
        Self::PedRemove,
        Self::VehicleCreate,
        Self::VehicleRemove,
        Self::ObjectCreate,
        Self::ObjectRemove,
        Self::InputPoll,
        Self::CameraUpdate,
        Self::StreamingUpdate,
        Self::WeaponFire,
    ];

    /// The function table name of the hooked function.
    pub fn function(self) -> &'static str {
        match self {
            Self::Frame => "frame",
            Self::PedCreate => "ped_create",
            Self::PedRemove => "ped_remove",
            Self::VehicleCreate => "vehicle_create",
            Self::VehicleRemove => "vehicle_remove",
            Self::ObjectCreate => "object_create",
            Self::ObjectRemove => "object_remove",
            Self::InputPoll => "input_poll",
            Self::CameraUpdate => "camera_update",
            Self::StreamingUpdate => "streaming_update",
            Self::WeaponFire => "weapon_fire",
        }
    }
}

/// What a hook saw, queued by the adapter and handled on the next frame.
#[derive(Debug, Clone, PartialEq)]
pub enum GameEvent {
    EntityCreated {
        handle: GameHandle,
        kind: EntityKind,
        asset: GameAsset,
    },
    EntityRemoved {
        handle: GameHandle,
        kind: EntityKind,
    },
    Input(InputState),
    Camera(CameraPose),
    /// A ped fired once. Only the local player's shots are reported to the server.
    Shot(GameShot),
}

/// Where hooks queue events. Cloning gives another handle to the same queue.
#[derive(Debug, Clone, Default)]
pub struct EventSink {
    queue: Arc<Mutex<Vec<GameEvent>>>,
}

impl EventSink {
    pub fn push(&self, event: GameEvent) {
        // A panic while the lock was held cannot leave a `Vec` half-pushed, so a poisoned queue is still usable.
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }

    /// Everything queued so far, oldest first, leaving the queue empty.
    pub fn take(&self) -> Vec<GameEvent> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_queue_across_threads() {
        let sink = EventSink::default();
        let hook = sink.clone();
        std::thread::spawn(move || hook.push(GameEvent::Input(InputState::default())))
            .join()
            .expect("hook thread");
        sink.push(GameEvent::Input(InputState {
            buttons: 1,
            ..InputState::default()
        }));
        let events = sink.take();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            GameEvent::Input(InputState {
                buttons: 1,
                ..InputState::default()
            })
        );
        assert!(sink.take().is_empty());
    }
}
