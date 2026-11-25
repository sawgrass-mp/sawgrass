//! The game functions the module hooks, and what each hook reports.
//!
//! The runtime adapter installs a hook on each [`HookPoint`] at the address the function table gives, and turns
//! every call into a [`GameEvent`] (or, for the frame, a call to [`crate::module::Module::frame`]). How a hook reads
//! the game's arguments is decided per build once the game runs; nothing above this file depends on it.

use crate::game::{CameraPose, EntityKind, GameAsset, GameHandle, InputState};

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
}

impl HookPoint {
    pub const ALL: [Self; 10] = [
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
}
