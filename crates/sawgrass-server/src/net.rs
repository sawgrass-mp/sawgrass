//! Players over the network: an in-process gateway feeds the tick loop and the tick loop sends snapshots
//! (`docs/architecture.md`, "Gateways", stage 1: one process for everything).
//!
//! The gateway runs on a tokio runtime of its own. Each connection gets a task that reads the player's
//! datagrams into a per-player inbox holding only the latest state, so a client that floods states overwrites
//! one slot instead of growing a queue; only joins and leaves, once per connection, go over a channel. Nothing
//! on that path waits for the tick. Once per tick [`Players::run_tick`] applies joins, latest states and leaves
//! to the ECS world,
//! updates the spatial grid, recomputes each player's visible set and sends each player a whole snapshot of
//! the entities in it, split into datagrams of at most [`SNAPSHOT_MAX_ENTITIES`] states.
//!
//! A player's entity is spawned with a [`NetId`] and a [`PlayerState`]; the state is the last one the client
//! sent, applied as is. Validating movement (speed limits, teleports) belongs to the authority work and is not
//! done here.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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

/// Events queued for one player before the oldest new one is dropped. The server sends only `Joined` and a
/// `Stats` every few seconds, so a client that reads its stream never comes near it.
const EVENT_QUEUE: usize = 16;

/// What one player's connection tasks leave for the tick thread. Each field holds the latest value only.
#[derive(Debug, Default)]
struct Inbox {
    state: Mutex<Option<ClientState>>,
    wants_stats: AtomicBool,
}

/// What a connection task tells the tick thread: once on joining, once on leaving.
#[derive(Debug)]
enum Inbound {
    Join {
        id: u32,
        sender: Datagrams,
        events: tokio_mpsc::Sender<Vec<u8>>,
        inbox: Arc<Inbox>,
    },
    Leave {
        id: u32,
    },
}

/// How the server accepts players.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenConfig {
    /// Let clients ask for [`ServerStats`] (load tests); otherwise the request is ignored.
    pub stats_to_clients: bool,
    /// Connections, handshakes included, above which new ones are refused.
    pub max_players: usize,
}

impl Default for ListenConfig {
    /// No stats for clients, and the architecture's target of 4096 players.
    fn default() -> Self {
        Self {
            stats_to_clients: false,
            max_players: 4096,
        }
    }
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
pub fn listen(gateway: Gateway, config: ListenConfig) -> Players {
    let (inbound_tx, inbound_rx) = mpsc::channel();
    let traffic = Arc::new(Traffic::default());
    let next_id = AtomicU32::new(1);
    let active = Arc::new(AtomicUsize::new(0));
    let accept_traffic = traffic.clone();
    tokio::spawn(async move {
        while let Some(incoming) = gateway.accept().await {
            if active.load(Ordering::Relaxed) >= config.max_players {
                incoming.refuse();
                continue;
            }
            active.fetch_add(1, Ordering::Relaxed);
            let inbound = inbound_tx.clone();
            let traffic = accept_traffic.clone();
            let active = active.clone();
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                if let Ok(session) = incoming.handshake().await {
                    serve(session, id, inbound, traffic, config.stats_to_clients).await;
                }
                active.fetch_sub(1, Ordering::Relaxed);
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
    let (events_tx, mut events_rx) = tokio_mpsc::channel::<Vec<u8>>(EVENT_QUEUE);
    let inbox = Arc::new(Inbox::default());
    let join = Inbound::Join {
        id,
        sender: datagrams.clone(),
        events: events_tx,
        inbox: inbox.clone(),
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
    let events_inbox = inbox.clone();
    tokio::spawn(async move {
        while let Ok((_, packet)) = event_receiver.recv().await {
            if stats_to_clients
                && ClientEvent::decode(&packet[PacketHeader::SIZE..]) == Ok(ClientEvent::RequestStats)
            {
                events_inbox.wants_stats.store(true, Ordering::Relaxed);
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
        if let Ok(state) = ClientState::decode(&packet[PacketHeader::SIZE..])
            && let Ok(mut slot) = inbox.state.lock()
        {
            *slot = Some(state);
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
    events: tokio_mpsc::Sender<Vec<u8>>,
    inbox: Arc<Inbox>,
    visible: VisibleSet,
    sequence: u16,
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
        self.apply_states(world);
        self.send_snapshots(info.tick as u32);
        self.send_stats();
    }

    fn apply_inbound(&mut self, world: &mut World, info: &TickInfo) {
        while let Ok(message) = self.inbound.try_recv() {
            match message {
                Inbound::Join {
                    id,
                    sender,
                    events,
                    inbox,
                } => {
                    let tick_hz =
                        (std::time::Duration::from_secs(1).as_nanos() / info.dt.as_nanos().max(1)) as u16;
                    let _ = events.try_send(event_packet(
                        &ServerEvent::Joined { entity: id, tick_hz },
                        info.tick,
                    ));
                    self.slots.insert(id, self.players.len());
                    self.players.push(Player {
                        id,
                        entity: None,
                        sender,
                        events,
                        inbox,
                        visible: VisibleSet::new(),
                        sequence: 0,
                    });
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

    /// Takes each player's latest state, if a new one arrived, into the world, the grid and the wire states.
    fn apply_states(&mut self, world: &mut World) {
        for player in &mut self.players {
            let Some(state) = player.inbox.state.lock().ok().and_then(|mut s| s.take()) else {
                continue;
            };
            let id = player.id;
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
        // A player whose queue is full has not read the last ones; it misses this one.
        for player in self
            .players
            .iter()
            .filter(|p| p.inbox.wants_stats.load(Ordering::Relaxed))
        {
            let _ = player.events.try_send(packet.clone());
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
