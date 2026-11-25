//! The network core plugged into the module: `sawgrass-netcode`'s [`Client`] behind [`NetCore`].
//!
//! Each frame [`NetLink`] hands the datagrams that arrived to the client, shows the remote entities at their
//! interpolated poses (spawning, moving and removing them through [`World`]), keeps the world streamed in around
//! them, applies the client's correction to the local player, and queues the local player's state to send. The
//! connection itself is not here: the code that owns the socket pushes into [`NetLink::inbox`] and sends what it
//! takes from [`NetLink::outbox`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use sawgrass_netcode::interp::{Event, Sample};
use sawgrass_netcode::local::Correction;
use sawgrass_netcode::{Client, ClientConfig, Pose as NetPose};
use sawgrass_protocol::combat::{Hit, Shot};
use sawgrass_protocol::entity::{EntityKind as NetKind, Info};
use sawgrass_protocol::game::{ClientEvent, OwnedState, ServerEvent};
use sawgrass_protocol::ped::PedBlock;
use sawgrass_protocol::{EntityState, Position};

use crate::entity_pool::NetId;
use crate::game::{EntityKind, Pose};
use crate::module::{CoreEvent, NetCore, ShotReport, World};

/// Datagrams kept at most in a [`Datagrams`] queue. While frames stop (a loading screen, a pause) the connection
/// keeps pushing, and only the newest snapshots matter, so the oldest are dropped above this.
pub const MAX_QUEUED: usize = 256;

/// Datagrams handed between the connection and the link. Cloning gives another handle to the same queue.
#[derive(Debug, Clone, Default)]
pub struct Datagrams {
    queue: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Datagrams {
    /// Queues `datagram`, dropping the oldest one when [`MAX_QUEUED`] are waiting.
    pub fn push(&self, datagram: Vec<u8>) {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.len() >= MAX_QUEUED {
            queue.remove(0);
        }
        queue.push(datagram);
    }

    pub fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Reliable events handed between the connection and the link, oldest first. Cloning gives another handle to the
/// same queue.
#[derive(Debug)]
pub struct EventQueue<T> {
    queue: Arc<Mutex<Vec<T>>>,
}

impl<T> Clone for EventQueue<T> {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
        }
    }
}

impl<T> Default for EventQueue<T> {
    fn default() -> Self {
        Self {
            queue: Arc::default(),
        }
    }
}

impl<T> EventQueue<T> {
    pub fn push(&self, event: T) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }

    pub fn take(&self) -> Vec<T> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Server events, from the connection to the link.
pub type ServerEvents = EventQueue<ServerEvent>;

/// Client events, from the link to the connection.
pub type ClientEvents = EventQueue<ClientEvent>;

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
    pub epoch: u8,
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
pub struct NetLink {
    /// The network core, once the server said who we are (`ServerEvent::Joined`).
    client: Option<Client>,
    config: ClientConfig,
    events: ServerEvents,
    client_events: ClientEvents,
    inbox: Datagrams,
    outbox: Datagrams,
    /// Local time in seconds, the sum of the frames' `dt`.
    now: f64,
    samples: Vec<Sample>,
    /// Remote entities spawned in the game, with the info they were last shown with.
    shown: BTreeMap<NetId, Info>,
    last_local: Option<[f32; 3]>,
    /// Entities this client simulates, by the server's ownership epoch, with the last position measured.
    owned: BTreeMap<NetId, Owned>,
    /// The owned states built for this frame's datagram, kept to reuse the allocation.
    owned_states: Vec<OwnedState>,
    /// Spawns, moves and removals the world refused, for diagnostics.
    pub world_errors: u64,
}

impl NetLink {
    /// A link that waits for the server's `Joined` event, which [`NetLink::events`] delivers.
    pub fn joining(config: ClientConfig) -> Self {
        Self::with_client(None, config)
    }

    /// A link for a client that has already joined.
    pub fn new(client: Client) -> Self {
        Self::with_client(Some(client), ClientConfig::default())
    }

    fn with_client(client: Option<Client>, config: ClientConfig) -> Self {
        Self {
            client,
            config,
            events: ServerEvents::default(),
            client_events: ClientEvents::default(),
            inbox: Datagrams::default(),
            outbox: Datagrams::default(),
            now: 0.0,
            samples: Vec::new(),
            shown: BTreeMap::new(),
            last_local: None,
            owned: BTreeMap::new(),
            owned_states: Vec::new(),
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

    /// The server gives (`yours`) or takes back the simulation of `entity`, from its `ServerEvent::Ownership`.
    ///
    /// An owned entity is left to the game's own physics instead of being moved to the interpolated pose, and its
    /// state is reported by [`NetLink::owned`]. The epoch is a wrapping byte, so it is stored and never compared:
    /// ownership events arrive in order on the one reliable event stream, and each one is applied as it comes.
    pub fn set_ownership(&mut self, entity: NetId, epoch: u8, yours: bool) {
        if yours {
            let pose = self.owned.get(&entity).and_then(|o| o.pose);
            self.owned.insert(entity, Owned { epoch, pose });
        } else {
            self.owned.remove(&entity);
        }
    }

    /// The entities this client simulates and their measured state. Each frame's datagram carries the owned
    /// states of those the game has spawned.
    pub fn owned(&self) -> &BTreeMap<NetId, Owned> {
        &self.owned
    }

    /// Where the connection takes the events to send.
    pub fn client_events(&self) -> ClientEvents {
        self.client_events.clone()
    }

    /// Where the connection pushes server events.
    pub fn events(&self) -> ServerEvents {
        self.events.clone()
    }

    /// The network core, once joined.
    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref()
    }

    /// Server events since the last frame: join, and where the server holds an entity after a refused state.
    fn handle_events(&mut self, world: &mut World<'_>) {
        for event in self.events.take() {
            match event {
                ServerEvent::Joined { entity, tick_hz } => self.joined(entity, tick_hz, world),
                ServerEvent::Corrected(state) => self.corrected(&state, world),
                ServerEvent::Ownership { entity, epoch, yours } => self.set_ownership(entity, epoch, yours),
                // Server figures are for load tests, and events of later features (payments, seats) are handled by
                // the code that adds them.
                _ => {}
            }
        }
    }

    /// A join starts a session over. After a reconnect the new session owns nothing for this client and its
    /// snapshots may not hold what the old one showed, so everything of the old session goes; what the new one still
    /// sends is spawned again on its next snapshot.
    fn joined(&mut self, entity: u32, tick_hz: u16, world: &mut World<'_>) {
        for id in std::mem::take(&mut self.shown).into_keys() {
            let result = world.despawn(id);
            self.count(result);
        }
        self.owned.clear();
        self.samples.clear();
        self.last_local = None;
        self.client = Some(Client::new(entity, tick_hz, self.config));
    }

    fn corrected(&mut self, state: &EntityState, world: &mut World<'_>) {
        let Some(client) = self.client.as_mut() else {
            return;
        };
        if state.id == client.entity() {
            let Some(local) = world.local_player_pose() else {
                return;
            };
            // A snap moves the player now; a smoothed correction comes through `Client::frame` over the next
            // frames.
            if let Some(Correction::Snap(pose)) = client.corrected(state, &net_pose(&local, [0.0; 3])) {
                let result = world.set_local_player_pose(game_pose(&pose));
                self.count(result);
            }
        } else if self.owned.contains_key(&state.id) {
            // Corrected carries no ownership epoch, so it is taken only for an entity this client still owns.
            let result = world.set_pose(state.id, game_pose(&NetPose::from_state(state)));
            self.count(result);
        }
    }

    /// Queues a shot for the server, stamped with the newest snapshot tick, which is what the player saw the targets
    /// at. A shot before the join, or fired from off the wire ranges, is dropped; a hit off the ranges is left out.
    fn report_shot(&mut self, report: &ShotReport) {
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let Ok(from) = Position::from_metres(report.from) else {
            return;
        };
        let mut shot = Shot::new(client.ack(), report.weapon, from);
        for hit in &report.hits {
            let Ok(point) = Position::from_metres(hit.point) else {
                continue;
            };
            let hit = Hit {
                target: hit.target,
                zone: hit.zone,
                point,
            };
            // The game resolves a pull of the trigger into at most a shotgun's pellets; more are left out.
            if !shot.push(hit) {
                break;
            }
        }
        self.client_events.push(ClientEvent::Shot(Box::new(shot)));
    }

    fn count<T, E>(&mut self, result: Result<T, E>) -> Option<T> {
        result.map_err(|_| self.world_errors += 1).ok()
    }

    fn show_remote(&mut self, dt: f32, world: &mut World<'_>) {
        let Some(client) = self.client.as_mut() else {
            // Nothing to show before the join; the snapshots that came early are of no use without it.
            self.inbox.take();
            return;
        };
        // Datagrams are stamped with the frame that takes them, up to a frame later than they arrived.
        for datagram in self.inbox.take() {
            let _ = client.receive(&datagram, self.now);
        }
        client.remote(self.now, &mut self.samples);
        let removed = client.removed().to_vec();
        for id in removed {
            if self.shown.remove(&id).is_some() {
                let result = world.despawn(id);
                self.count(result);
            }
            self.owned.remove(&id);
        }
        let samples = std::mem::take(&mut self.samples);
        for sample in &samples {
            let pose = game_pose(&sample.pose);
            let shown = self.shown.get(&sample.id);
            let new_model = shown.is_some_and(|i| i.kind != sample.info.kind || i.model != sample.info.model);
            if sample.event == Some(Event::InfoChanged) && new_model {
                // The game cannot swap a model in place, so the entity is spawned again below. A change of block
                // fields alone (health, damage, colours) keeps it; applying them waits for the blocks task.
                self.shown.remove(&sample.id);
                let result = world.despawn(sample.id);
                self.count(result);
            } else if let Some(info) = self.shown.get_mut(&sample.id) {
                *info = sample.info;
            }
            if let Some(owned) = self.owned.get_mut(&sample.id) {
                // The game simulates it: measure it, and spawn it only if it is not there yet.
                if let Some(now) = world.pose(sample.id) {
                    let previous = owned.pose.map(|p| p.position);
                    owned.pose = Some(net_pose(&now, velocity(previous, now.position, dt)));
                    continue;
                }
            }
            if self.shown.contains_key(&sample.id) {
                let result = world.set_pose(sample.id, pose);
                self.count(result);
            } else {
                // Not only on `Event::Appeared`: an entity the game removed by itself, or whose spawn failed, is
                // spawned again on a later frame.
                let info = sample.info;
                let result = world.spawn(sample.id, game_kind(info.kind), info.model, pose);
                if self.count(result).is_some() {
                    self.shown.insert(sample.id, info);
                }
            }
        }
        let focus: Vec<[f32; 3]> = samples.iter().map(|s| s.pose.position).collect();
        world.set_streaming_focus(&focus);
        self.samples = samples;
    }

    fn report_local(&mut self, dt: f32, world: &mut World<'_>) {
        let Some(client) = self.client.as_mut() else {
            return;
        };
        let Some(mut local) = world.local_player_pose() else {
            self.last_local = None;
            return;
        };
        if let Some(offset) = client.frame(dt) {
            for (p, o) in local.position.iter_mut().zip(offset) {
                *p += o;
            }
            if world.set_local_player_pose(local).is_err() {
                self.world_errors += 1;
            }
        }
        // Measured after the correction offset, so a correction shows as velocity for its frames; corrections are
        // smoothed over several frames, which keeps that small.
        let measured = velocity(self.last_local, local.position, dt);
        self.last_local = Some(local.position);
        // Every owned entity the game has spawned and measured, in id order; `outgoing_with` rotates them across
        // datagrams when more are owned than one carries.
        self.owned_states.clear();
        for (&id, owned) in &self.owned {
            let (Some(pose), Some(info)) = (owned.pose, self.shown.get(&id)) else {
                continue;
            };
            // ASSUMPTION: until the blocks task reads vehicle and NPC blocks from the game, the server's own block
            // is sent back, so its merge (ADR 0006) keeps the block as it is and takes only the kinematics.
            if let Ok(state) = pose.to_owned_state(id, owned.epoch, info.fields()) {
                self.owned_states.push(state);
            }
        }
        // The weapon in hand comes from the game, since the server checks each shot against it (SRV-16).
        // ASSUMPTION: the rest of the ped block is a standing ped until the blocks task reads it from the game.
        // Health, armour, vehicle and seat are the server's, so this cannot change them.
        let ped = PedBlock {
            weapon: world.local_player_weapon(),
            ..PedBlock::default()
        };
        // An error means the player's pose is off the wire ranges; the slot is spent and the server corrects.
        if let Ok(Some(datagram)) =
            client.outgoing_with(self.now, &net_pose(&local, measured), &ped, &self.owned_states)
        {
            self.outbox.push(datagram);
        }
    }
}

impl NetCore for NetLink {
    fn on_event(&mut self, event: &CoreEvent) {
        // An entity the game removed by itself is spawned again on the next frame it is still in the snapshot.
        match event {
            CoreEvent::EntityLost(id) => {
                self.shown.remove(id);
            }
            CoreEvent::Shot(report) => self.report_shot(report),
            CoreEvent::Input(_) | CoreEvent::Camera(_) => {}
        }
    }

    fn on_frame(&mut self, dt: f32, world: &mut World<'_>) {
        self.now += f64::from(dt);
        self.handle_events(world);
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
