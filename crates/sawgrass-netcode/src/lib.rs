//! The network core of the Sawgrass client, independent of the game and of the runtime it runs on.
//!
//! Roadmap step 8 ("Sync in the game") starts with remote players interpolated between snapshots and the local
//! player predicted and corrected from the server. Everything here is plain logic over the wire types of
//! `sawgrass-protocol`: no game access, no I/O, no clock of its own. The caller passes the local time in seconds,
//! so the same code runs in the game module, in the bots and in deterministic tests.
//!
//! - [`receive::SnapshotReceiver`] turns delta snapshot datagrams into whole snapshots and keeps the
//!   acknowledgement the client puts in its states' headers.
//! - [`clock::ServerClock`] estimates which server tick it is from snapshot arrivals, and the render tick a
//!   little behind it at which remote entities are shown.
//! - [`interp::Interpolator`] keeps a short history per remote entity and gives its pose at the render tick.
//! - [`local::LocalPlayer`] paces and quantizes the local player's states, and turns a correction from the
//!   server into a snap or into small per-frame moves.
//! - [`Client`] puts the four together behind the calls a game module makes each frame.
//!
//! Clients decide their own movement (`docs/architecture.md`, "Authority"): the game's own simulation is the
//! prediction, and the server only accepts or refuses what the client reports. A refused move leaves the player
//! where it was last accepted; [`local::LocalPlayer::correct`] brings the local player back there once the server
//! says so. The server does not send that yet (see [`Client::correct`]).

pub mod clock;
pub mod interp;
pub mod local;
pub mod receive;

mod client;

pub use client::{Client, ClientConfig};

use sawgrass_protocol::game::ClientState;
use sawgrass_protocol::{EncodeError, EntityState, Position, Rotation, Velocity};

/// An entity's position, velocity and rotation in game units: metres, metres per second and a unit quaternion
/// `[x, y, z, w]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    /// Where it is, in metres.
    pub position: [f32; 3],
    /// How fast it moves, in metres per second.
    pub velocity: [f32; 3],
    /// Which way it faces.
    pub rotation: [f32; 4],
}

impl Pose {
    /// Standing at `position`, facing the identity rotation.
    #[must_use]
    pub const fn at(position: [f32; 3]) -> Self {
        Self {
            position,
            velocity: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
        }
    }

    /// The pose a snapshot state describes.
    #[must_use]
    pub fn from_state(state: &EntityState) -> Self {
        Self {
            position: state.position.to_metres(),
            velocity: state.velocity.to_metres_per_second(),
            rotation: state.rotation.to_quaternion(),
        }
    }

    /// The quantized state to send. Fails for a value outside the wire ranges or not finite.
    pub fn to_client_state(&self) -> Result<ClientState, EncodeError> {
        Ok(ClientState {
            position: Position::from_metres(self.position)?,
            velocity: Velocity::from_metres_per_second(self.velocity)?,
            rotation: Rotation::from_quaternion(self.rotation)?,
        })
    }
}

pub(crate) fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub(crate) fn length(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}
