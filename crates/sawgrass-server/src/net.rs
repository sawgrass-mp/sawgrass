//! Players over the network: an in-process gateway feeds the tick loop and the tick loop sends snapshots
//! (`docs/architecture.md`, "Gateways", stage 1: one process for everything).
//!
//! The gateway runs on a tokio runtime of its own. Each connection gets a task that reads the player's
//! datagrams and events and forwards them to the tick thread over a channel; nothing on that path waits for the
//! tick. Once per tick [`Players::run_tick`] applies what arrived (joins, states, leaves) to the ECS world,
//! updates the spatial grid, recomputes each player's visible set and sends each player a whole snapshot of
//! the entities in it, split into datagrams of at most [`SNAPSHOT_MAX_ENTITIES`] states.
//!
//! A player's entity is spawned with a [`NetId`] and a [`PlayerState`]; the state is the last one the client
//! sent, applied as is. Validating movement (speed limits, teleports) belongs to the authority work and is not
//! done here.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use hecs::{Entity, World};
use sawgrass_gateway::{Datagrams, Gateway, Session};
use sawgrass_protocol::game::{
    ClientEvent, ClientState, SNAPSHOT_MAX_ENTITIES, ServerEvent, ServerStats, encode_snapshot,
};
use sawgrass_protocol::{EntityState, PacketHeader, PacketKind};
use tokio::sync::mpsc as tokio_mpsc;

use crate::TickInfo;
use crate::interest::{EntityId, Point, SpatialGrid, VisibilityChanges, VisibilityConfig, VisibleSet};

/// The network id of a player's entity, the `id` in snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetId(pub u32);

/// The last state a player's client sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerState(pub ClientState);

/// What a connection task tells the tick thread.
#[derive(Debug)]
enum Inbound {
    Join {
        id: u32,
        sender: Datagrams,
        events: tokio_mpsc::UnboundedSender<Vec<u8>>,
    },
    State {
        id: u32,
        state: ClientState,
    },
    WantsStats {
        id: u32,
    },
    Leave {
        id: u32,
    },
}

/// Traffic counters shared between the connection tasks and the tick thread.
#[derive(Debug, Default)]
pub struct Traffic {
    /// Datagram bytes sent to players, headers included.
    pub sent_bytes: AtomicU64,
    /// Datagram bytes received from players.
    pub received_bytes: AtomicU64,
    /// Snapshot datagrams quinn refused to queue (connection closing, datagram too large).
    pub send_errors: AtomicU64,
}

/// Starts accepting players on `gateway`, on the current tokio runtime, and returns the tick-side half.
///
/// `stats_to_clients` lets clients ask for [`ServerStats`] (load tests); otherwise the request is ignored.
pub fn listen(gateway: Gateway, stats_to_clients: bool) -> Players {
    let (inbound_tx, inbound_rx) = mpsc::channel();
    let traffic = Arc::new(Traffic::default());
    let next_id = Arc::new(AtomicU32::new(1));
    let accept_traffic = traffic.clone();
    tokio::spawn(async move {
        while let Some(incoming) = gateway.accept().await {
            let inbound = inbound_tx.clone();
            let traffic = accept_traffic.clone();
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                if let Ok(session) = incoming.handshake().await {
                    serve(session, id, inbound, traffic, stats_to_clients).await;
                }
            });
        }
    });
    Players {
        inbound: inbound_rx,
        traffic,
        players: Vec::new(),
        slots: std::collections::HashMap::new(),
        grid: SpatialGrid::default(),
        visibility: VisibilityConfig::default(),
        changes: VisibilityChanges::default(),
        states: std::collections::HashMap::new(),
        scratch: Vec::new(),
        pending_stats: Arc::new(Mutex::new(None)),
    }
}

/// Serves one player's connection until it ends: this task reads datagrams, one more reads events and one
/// writes the events the tick thread queues.
async fn serve(
    session: Session,
    id: u32,
    inbound: mpsc::Sender<Inbound>,
    traffic: Arc<Traffic>,
    stats_to_clients: bool,
) {
    let (datagrams, mut event_sender, mut event_receiver) = session.split();
    let (events_tx, mut events_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();
    let join = Inbound::Join {
        id,
        sender: datagrams.clone(),
        events: events_tx,
    };
    if inbound.send(join).is_err() {
        return;
    }
    tokio::spawn(async move {
        while let Some(packet) = events_rx.recv().await {
            if event_sender.send(&packet).await.is_err() {
                break;
            }
        }
    });
    let events_inbound = inbound.clone();
    tokio::spawn(async move {
        while let Ok((_, packet)) = event_receiver.recv().await {
            if stats_to_clients
                && ClientEvent::decode(&packet[PacketHeader::SIZE..]) == Ok(ClientEvent::RequestStats)
                && events_inbound.send(Inbound::WantsStats { id }).is_err()
            {
                break;
            }
        }
    });
    while let Ok((header, packet)) = datagrams.recv().await {
        traffic
            .received_bytes
            .fetch_add(packet.len() as u64, Ordering::Relaxed);
        if header.kind != PacketKind::ClientState {
            continue;
        }
        if let Ok((state, _)) = ClientState::decode(&packet[PacketHeader::SIZE..])
            && inbound.send(Inbound::State { id, state }).is_err()
        {
            break;
        }
    }
    datagrams.close();
    let _ = inbound.send(Inbound::Leave { id });
}

/// One connected player, as the tick thread sees it.
#[derive(Debug)]
struct Player {
    id: u32,
    entity: Option<Entity>,
    sender: Datagrams,
    events: tokio_mpsc::UnboundedSender<Vec<u8>>,
    visible: VisibleSet,
    sequence: u16,
    wants_stats: bool,
}

/// The tick-side half of the network: applies what players sent and sends them snapshots.
#[derive(Debug)]
pub struct Players {
    inbound: mpsc::Receiver<Inbound>,
    traffic: Arc<Traffic>,
    players: Vec<Player>,
    /// Index into `players` by id.
    slots: std::collections::HashMap<u32, usize>,
    grid: SpatialGrid,
    visibility: VisibilityConfig,
    changes: VisibilityChanges,
    /// This tick's wire state of every placed player, by id.
    states: std::collections::HashMap<u32, EntityState>,
    scratch: Vec<EntityState>,
    pending_stats: Arc<Mutex<Option<ServerStats>>>,
}

impl Players {
    /// Shared traffic counters.
    #[must_use]
    pub fn traffic(&self) -> Arc<Traffic> {
        self.traffic.clone()
    }

    /// Where to put [`ServerStats`] for the next tick to send to the players that asked for them.
    #[must_use]
    pub fn stats_slot(&self) -> Arc<Mutex<Option<ServerStats>>> {
        self.pending_stats.clone()
    }

    /// Players connected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.players.len()
    }

    /// Whether no player is connected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.players.is_empty()
    }

    /// Applies what arrived since the last tick, then sends every player its snapshot for `info.tick`.
    pub fn run_tick(&mut self, world: &mut World, info: &TickInfo) {
        self.apply_inbound(world, info);
        self.send_snapshots(info.tick as u32);
        self.send_stats();
    }

    fn apply_inbound(&mut self, world: &mut World, info: &TickInfo) {
        while let Ok(message) = self.inbound.try_recv() {
            match message {
                Inbound::Join { id, sender, events } => {
                    let tick_hz =
                        (std::time::Duration::from_secs(1).as_nanos() / info.dt.as_nanos().max(1)) as u16;
                    let _ = events.send(event_packet(
                        &ServerEvent::Joined { entity: id, tick_hz },
                        info.tick,
                    ));
                    self.slots.insert(id, self.players.len());
                    self.players.push(Player {
                        id,
                        entity: None,
                        sender,
                        events,
                        visible: VisibleSet::new(),
                        sequence: 0,
                        wants_stats: false,
                    });
                }
                Inbound::State { id, state } => {
                    let Some(&slot) = self.slots.get(&id) else {
                        continue;
                    };
                    let player = &mut self.players[slot];
                    match player.entity {
                        Some(entity) => {
                            if let Ok(mut current) = world.get::<&mut PlayerState>(entity) {
                                current.0 = state;
                            }
                        }
                        None => player.entity = Some(world.spawn((NetId(id), PlayerState(state)))),
                    }
                    let [x, y, _] = state.position.to_metres();
                    self.grid.upsert(EntityId(id), Point::new(x, y));
                    self.states.insert(
                        id,
                        EntityState {
                            id,
                            position: state.position,
                            velocity: state.velocity,
                            rotation: state.rotation,
                        },
                    );
                }
                Inbound::WantsStats { id } => {
                    if let Some(&slot) = self.slots.get(&id) {
                        self.players[slot].wants_stats = true;
                    }
                }
                Inbound::Leave { id } => {
                    let Some(slot) = self.slots.remove(&id) else {
                        continue;
                    };
                    let player = self.players.swap_remove(slot);
                    if let Some(moved) = self.players.get(slot) {
                        self.slots.insert(moved.id, slot);
                    }
                    if let Some(entity) = player.entity {
                        let _ = world.despawn(entity);
                    }
                    self.grid.remove(EntityId(id));
                    self.states.remove(&id);
                }
            }
        }
    }

    fn send_snapshots(&mut self, tick: u32) {
        let mut sent = 0u64;
        let mut errors = 0u64;
        for player in &mut self.players {
            let Some(own) = self.states.get(&player.id) else {
                continue;
            };
            let [x, y, _] = own.position.to_metres();
            player.visible.update(
                &self.grid,
                Some(EntityId(player.id)),
                Point::new(x, y),
                &self.visibility,
                &mut self.changes,
            );
            self.scratch.clear();
            self.scratch.extend(
                player
                    .visible
                    .members()
                    .iter()
                    .filter_map(|id| self.states.get(&id.0)),
            );
            // At least one datagram: an empty snapshot still tells the client the tick advanced.
            let datagrams = self.scratch.len().div_ceil(SNAPSHOT_MAX_ENTITIES).max(1);
            for i in 0..datagrams {
                let start = i * SNAPSHOT_MAX_ENTITIES;
                let chunk = &self.scratch[start..(start + SNAPSHOT_MAX_ENTITIES).min(self.scratch.len())];
                let mut packet = header(PacketKind::Snapshot, player.sequence, tick);
                player.sequence = player.sequence.wrapping_add(1);
                if encode_snapshot(chunk, &mut packet).is_err() {
                    continue;
                }
                let len = packet.len() as u64;
                match player.sender.send(Bytes::from(packet)) {
                    Ok(()) => sent += len,
                    Err(_) => errors += 1,
                }
            }
        }
        self.traffic.sent_bytes.fetch_add(sent, Ordering::Relaxed);
        self.traffic.send_errors.fetch_add(errors, Ordering::Relaxed);
    }

    fn send_stats(&mut self) {
        let Some(stats) = self.pending_stats.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        let packet = event_packet(&ServerEvent::Stats(stats), 0);
        for player in self.players.iter().filter(|p| p.wants_stats) {
            let _ = player.events.send(packet.clone());
        }
    }
}

fn header(kind: PacketKind, sequence: u16, tick: u32) -> Vec<u8> {
    let mut out = vec![0; PacketHeader::SIZE];
    let header = PacketHeader { kind, sequence, tick };
    // The buffer is exactly the header size, so encoding cannot fail.
    let _ = header.encode(&mut out);
    out
}

fn event_packet(event: &ServerEvent, tick: u64) -> Vec<u8> {
    let mut packet = header(PacketKind::Event, 0, tick as u32);
    event.encode(&mut packet);
    packet
}

/// Binds a gateway on `addr` with a DER certificate and PKCS#8 DER key read from files.
pub fn bind_gateway(
    addr: SocketAddr,
    cert: &std::path::Path,
    key: &std::path::Path,
) -> Result<Gateway, String> {
    use sawgrass_gateway::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cert = std::fs::read(cert).map_err(|e| format!("{}: {e}", cert.display()))?;
    let key = std::fs::read(key).map_err(|e| format!("{}: {e}", key.display()))?;
    Gateway::bind(
        addr,
        vec![CertificateDer::from(cert)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
    )
    .map_err(|e| e.to_string())
}
