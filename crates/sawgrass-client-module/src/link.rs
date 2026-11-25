//! The network core plugged into the module: `sawgrass-netcode`'s [`Client`] behind [`NetCore`].
//!
//! Each frame [`NetLink`] hands the datagrams that arrived to the client, shows the remote entities at their
//! interpolated poses (spawning, moving and removing them through [`World`]), keeps the world streamed in around
//! them, applies the client's correction to the local player, and queues the local player's state to send. The
//! connection itself is not here: the code that owns the socket pushes into [`NetLink::inbox`] and sends what it
//! takes from [`NetLink::outbox`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use sawgrass_netcode::interp::Sample;
use sawgrass_netcode::{Client, Pose as NetPose};
use sawgrass_protocol::entity::{EntityKind as NetKind, Info};

use crate::entity_pool::NetId;
use crate::game::{EntityKind, Pose};
use crate::module::{CoreEvent, NetCore, World};

/// What each remote entity is.
///
/// TODO(CLIENT-6, #165): the network core will carry each entity's `Info` on its samples; this lookup goes away
/// then.
pub trait InfoSource {
    fn info(&self, id: NetId) -> Option<Info>;
}

/// Datagrams handed between the connection and the link. Cloning gives another handle to the same queue.
#[derive(Debug, Clone, Default)]
pub struct Datagrams {
    queue: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Datagrams {
    pub fn push(&self, datagram: Vec<u8>) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(datagram);
    }

    pub fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The game's kind of entity for the protocol's.
pub fn game_kind(kind: NetKind) -> EntityKind {
    match kind {
        NetKind::Player | NetKind::Npc => EntityKind::Ped,
        NetKind::Vehicle => EntityKind::Vehicle,
        NetKind::Object => EntityKind::Object,
    }
}

/// The module's pose for the network core's: the position, and the heading about the vertical axis.
///
/// ASSUMPTION: the game's world is Z-up and its heading is the yaw about Z, as the protocol's rotation is read by
/// the bots. Pitch and roll are dropped until the real `GameApi` takes them.
pub fn game_pose(pose: &NetPose) -> Pose {
    let [x, y, z, w] = pose.rotation;
    let heading = (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z));
    Pose {
        position: pose.position,
        heading,
    }
}

/// The network core's pose for the module's, with the velocity measured by the caller.
pub fn net_pose(pose: &Pose, velocity: [f32; 3]) -> NetPose {
    let half = pose.heading / 2.0;
    NetPose {
        position: pose.position,
        velocity,
        rotation: [0.0, 0.0, half.sin(), half.cos()],
    }
}

/// An entity the server gave this client to simulate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Owned {
    /// The server's ownership epoch, sent back with every state so a stale owner is refused.
    pub epoch: u32,
    /// Where the game has it now, measured on the last frame; `None` until it is spawned.
    pub pose: Option<NetPose>,
}

/// Velocity from the last position to this one over `dt`; zero without a last position.
fn velocity(last: Option<[f32; 3]>, now: [f32; 3], dt: f32) -> [f32; 3] {
    match last {
        Some(l) if dt > 0.0 => [(now[0] - l[0]) / dt, (now[1] - l[1]) / dt, (now[2] - l[2]) / dt],
        _ => [0.0; 3],
    }
}

#[derive(Debug)]
pub struct NetLink<I> {
    client: Client,
    infos: I,
    inbox: Datagrams,
    outbox: Datagrams,
    /// Local time in seconds, the sum of the frames' `dt`.
    now: f64,
    samples: Vec<Sample>,
    /// Remote entities spawned in the game.
    shown: BTreeSet<NetId>,
    last_local: Option<[f32; 3]>,
    /// Entities this client simulates, by the server's ownership epoch, with the last position measured.
    owned: BTreeMap<NetId, Owned>,
    /// Spawns, moves and removals the world refused, for diagnostics.
    pub world_errors: u64,
}

impl<I: InfoSource> NetLink<I> {
    pub fn new(client: Client, infos: I) -> Self {
        Self {
            client,
            infos,
            inbox: Datagrams::default(),
            outbox: Datagrams::default(),
            now: 0.0,
            samples: Vec::new(),
            shown: BTreeSet::new(),
            last_local: None,
            owned: BTreeMap::new(),
            world_errors: 0,
        }
    }

    /// Where the connection pushes datagrams from the server.
    pub fn inbox(&self) -> Datagrams {
        self.inbox.clone()
    }

    /// Where the connection takes datagrams to send.
    pub fn outbox(&self) -> Datagrams {
        self.outbox.clone()
    }

    /// The server gives (`yours`) or takes back the simulation of `entity`.
    ///
    /// From the server's ownership event (SRV-14, #170, in review). An owned entity is left to the game's own
    /// physics instead of being moved to the interpolated pose, and its state is reported by [`NetLink::owned`].
    pub fn set_ownership(&mut self, entity: NetId, epoch: u32, yours: bool) {
        if yours {
            self.owned.insert(entity, Owned { epoch, pose: None });
        } else if self.owned.get(&entity).is_some_and(|o| o.epoch <= epoch) {
            self.owned.remove(&entity);
        }
    }

    /// The entities this client simulates and their measured state, for the owned states it sends.
    ///
    /// TODO(SRV-14): send these with the client state once the protocol's owned-state encoding lands.
    pub fn owned(&self) -> &BTreeMap<NetId, Owned> {
        &self.owned
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    fn count<T, E>(&mut self, result: Result<T, E>) -> Option<T> {
        result.map_err(|_| self.world_errors += 1).ok()
    }

    fn show_remote(&mut self, dt: f32, world: &mut World<'_>) {
        // Datagrams are stamped with the frame that takes them, up to a frame later than they arrived.
        for datagram in self.inbox.take() {
            let _ = self.client.receive(&datagram, self.now);
        }
        self.client.remote(self.now, &mut self.samples);
        let present: BTreeSet<NetId> = self.samples.iter().map(|s| s.id).collect();
        let gone: Vec<NetId> = self.shown.difference(&present).copied().collect();
        for id in gone {
            self.shown.remove(&id);
            let result = world.despawn(id);
            self.count(result);
        }
        let samples = std::mem::take(&mut self.samples);
        for sample in &samples {
            let pose = game_pose(&sample.pose);
            if let Some(owned) = self.owned.get_mut(&sample.id) {
                // The game simulates it: measure it, and spawn it only if it is not there yet.
                if let Some(now) = world.pose(sample.id) {
                    let previous = owned.pose.map(|p| p.position);
                    owned.pose = Some(net_pose(&now, velocity(previous, now.position, dt)));
                    continue;
                }
            }
            if self.shown.contains(&sample.id) {
                let result = world.set_pose(sample.id, pose);
                self.count(result);
            } else if let Some(info) = self.infos.info(sample.id) {
                let result = world.spawn(sample.id, game_kind(info.kind), info.model, pose);
                if self.count(result).is_some() {
                    self.shown.insert(sample.id);
                }
            }
        }
        let focus: Vec<[f32; 3]> = samples.iter().map(|s| s.pose.position).collect();
        world.set_streaming_focus(&focus);
        self.samples = samples;
    }

    fn report_local(&mut self, dt: f32, world: &mut World<'_>) {
        let Some(mut local) = world.local_player_pose() else {
            self.last_local = None;
            return;
        };
        if let Some(offset) = self.client.frame(dt) {
            for (p, o) in local.position.iter_mut().zip(offset) {
                *p += o;
            }
            let result = world.set_local_player_pose(local);
            self.count(result);
        }
        let velocity = match self.last_local {
            Some(last) if dt > 0.0 => {
                let p = local.position;
                [
                    (p[0] - last[0]) / dt,
                    (p[1] - last[1]) / dt,
                    (p[2] - last[2]) / dt,
                ]
            }
            _ => [0.0; 3],
        };
        self.last_local = Some(local.position);
        if let Some(datagram) = self.client.outgoing(self.now, &net_pose(&local, velocity)) {
            self.outbox.push(datagram);
        }
    }
}

impl<I: InfoSource> NetCore for NetLink<I> {
    fn on_event(&mut self, event: &CoreEvent) {
        // An entity the game removed by itself is spawned again on the next frame it is still in the snapshot.
        if let CoreEvent::EntityLost(id) = event {
            self.shown.remove(id);
        }
    }

    fn on_frame(&mut self, dt: f32, world: &mut World<'_>) {
        self.now += f64::from(dt);
        self.show_remote(dt, world);
        self.report_local(dt, world);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_survives_the_round_trip() {
        for heading in [-3.0f32, -1.0, 0.0, 0.5, 2.5] {
            let pose = Pose {
                position: [1.0, 2.0, 3.0],
                heading,
            };
            let back = game_pose(&net_pose(&pose, [0.0; 3]));
            assert!(
                (back.heading - heading).abs() < 1e-5,
                "{heading} came back as {}",
                back.heading
            );
            assert_eq!(back.position, pose.position);
        }
    }

    #[test]
    fn kinds_map_to_the_game() {
        assert_eq!(game_kind(NetKind::Player), EntityKind::Ped);
        assert_eq!(game_kind(NetKind::Npc), EntityKind::Ped);
        assert_eq!(game_kind(NetKind::Vehicle), EntityKind::Vehicle);
        assert_eq!(game_kind(NetKind::Object), EntityKind::Object);
    }
}
