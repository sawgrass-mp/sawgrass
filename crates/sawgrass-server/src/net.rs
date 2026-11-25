//! Players over the network: an in-process gateway feeds the tick loop and the tick loop sends snapshots
//! (`docs/architecture.md`, "Gateways", stage 1: one process for everything).
//!
//! The gateway runs on a tokio runtime of its own. Each connection gets a task that reads the player's
//! datagrams into a per-player inbox holding only the latest state, so a client that floods states overwrites
//! one slot instead of growing a queue; only joins and leaves, once per connection, go over a channel. Nothing
//! on that path waits for the tick. Once per tick [`Players::run_tick`] applies joins, latest states and leaves
//! to the ECS world,
//! rebuilds the spatial grid, recomputes each player's visible set and sends each player a snapshot of the
//! entities in it, in as many datagrams as it needs.
//!
//! Snapshots are delta-encoded against the newest one each client acknowledged (task NET-14, see
//! [`crate::snapshot`]). Each player's visible set and datagrams are built by its own [`View`], on a rayon pool of
//! [`ListenConfig::tick_threads`] threads (task SRV-7). The tick thread does not hand datagrams to quinn itself
//! (task NET-9): each view cuts its datagrams of a tick from one buffer sized to exactly those bytes, and the tick
//! passes all of them to one task on the gateway runtime in a single [`Batch`]. That task hands each datagram to
//! its connection. So the tick wakes the gateway once per tick instead of once per datagram (BOT-3 measured 22% of
//! the tick thread in those wake-ups) and allocates once per player instead of once per datagram. A datagram keeps
//! only its own player's buffer of that tick alive, so a client that stops acknowledging pins no more than the
//! bytes quinn queues for it (the gateway's 64 KiB datagram send buffer), plus one allocation's overhead per queued
//! tick. At most [`OUTBOX_TICKS`] batches wait for the gateway; if it falls further behind, the tick's snapshots
//! are dropped and counted rather than queued.
//!
//! Every player takes a dense [`Slot`] when the tick thread sees it join (task IM-4). The slot indexes the
//! per-player arrays here (players, wire states) and the ids in the grid and the visible sets; its
//! [`Slot::wire_id`] is the player's entity id in `Joined` and in snapshots. A freed slot is reused only after
//! the tick that removed it has recomputed every visible set (see [`crate::slots`]).
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
use rayon::prelude::*;
use sawgrass_gateway::{Datagrams, Gateway, Session};
use sawgrass_protocol::game::{ClientEvent, ClientState, ServerEvent, ServerStats};
use sawgrass_protocol::{EntityState, PacketHeader, PacketKind};
use tokio::sync::mpsc as tokio_mpsc;

use crate::TickInfo;
use crate::interest::{DenseGrid, EntityId, Point, VisibilityConfig};
use crate::slots::{Slot, Slots};
use crate::snapshot::{History, View};

/// The network id of a player's entity, the `id` in snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetId(pub u32);

/// The last state a player's client sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerState(pub ClientState);

/// Events queued for one player before the oldest new one is dropped. The server sends only `Joined` and a
/// `Stats` every few seconds, so a client that reads its stream never comes near it.
const EVENT_QUEUE: usize = 16;

/// Ticks of snapshots that may wait for the gateway's send task before a tick's snapshots are dropped.
const OUTBOX_TICKS: usize = 2;

/// One tick's datagrams for the gateway's send task: each packet with the index of its connection in `senders`.
#[derive(Debug, Default)]
struct Batch {
    senders: Vec<Datagrams>,
    packets: Vec<(u32, Bytes)>,
}

/// What one player's connection tasks leave for the tick thread. Each field holds the latest value only.
#[derive(Debug, Default)]
struct Inbox {
    state: Mutex<Option<ClientState>>,
    wants_stats: AtomicBool,
    /// The newest snapshot tick the client decoded completely, from the tick field of its client states' headers
    /// (0: none). The server uses it as the delta baseline only if it sent that tick to this player.
    ack: AtomicU32,
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
    /// Threads that build players' snapshots in parallel. 1 builds them on the tick thread.
    pub tick_threads: usize,
}

impl Default for ListenConfig {
    /// No stats for clients, and the architecture's target of 4096 players.
    fn default() -> Self {
        Self {
            stats_to_clients: false,
            max_players: 4096,
            tick_threads: 1,
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
    /// Ticks whose snapshots were dropped because the gateway had not sent the previous [`OUTBOX_TICKS`] yet.
    pub dropped_ticks: AtomicU64,
}

/// Starts accepting players on `gateway`, on the current tokio runtime, and returns the tick-side half.
pub fn listen(gateway: Gateway, config: ListenConfig) -> Players {
    let (inbound_tx, inbound_rx) = mpsc::channel();
    let traffic = Arc::new(Traffic::default());
    let next_id = AtomicU32::new(1);
    let active = Arc::new(AtomicUsize::new(0));
    let accept_traffic = traffic.clone();
    let (outbox, recycle) = spawn_sender(traffic.clone());
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
        slots: Slots::new(),
        by_connection: std::collections::HashMap::new(),
        grid: DenseGrid::default(),
        visibility: VisibilityConfig::default(),
        pool: (config.tick_threads > 1)
            .then(|| {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(config.tick_threads)
                    .thread_name(|i| format!("tick-{i}"))
                    .build()
                    .ok()
            })
            .flatten(),
        states: Vec::new(),
        history: History::new(),
        outbox,
        recycle,
        pending_stats: Arc::new(Mutex::new(None)),
    }
}

/// Starts the task that hands each tick's datagrams to their connections, and returns the channel to send it
/// batches on and the one it returns them on, emptied, for reuse.
fn spawn_sender(traffic: Arc<Traffic>) -> (tokio_mpsc::Sender<Batch>, mpsc::Receiver<Batch>) {
    let (outbox, mut batches) = tokio_mpsc::channel::<Batch>(OUTBOX_TICKS);
    let (recycle_tx, recycle) = mpsc::channel();
    tokio::spawn(async move {
        while let Some(mut batch) = batches.recv().await {
            let mut sent = 0u64;
            let mut errors = 0u64;
            for (index, packet) in batch.packets.drain(..) {
                let len = packet.len() as u64;
                match batch.senders[index as usize].send(packet) {
                    Ok(()) => sent += len,
                    Err(_) => errors += 1,
                }
            }
            batch.senders.clear();
            traffic.sent_bytes.fetch_add(sent, Ordering::Relaxed);
            traffic.send_errors.fetch_add(errors, Ordering::Relaxed);
            if recycle_tx.send(batch).is_err() {
                break;
            }
        }
    });
    (outbox, recycle)
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
            inbox.ack.store(header.tick, Ordering::Relaxed);
        }
    }
    datagrams.close();
    let _ = inbound.send(Inbound::Leave { id });
}

/// One connected player, as the tick thread sees it.
#[derive(Debug)]
struct Player {
    slot: Slot,
    entity: Option<Entity>,
    sender: Datagrams,
    events: tokio_mpsc::Sender<Vec<u8>>,
    inbox: Arc<Inbox>,
    view: View,
}

/// The tick-side half of the network: applies what players sent and sends them snapshots.
#[derive(Debug)]
pub struct Players {
    inbound: mpsc::Receiver<Inbound>,
    traffic: Arc<Traffic>,
    /// Connected players by slot index.
    players: Vec<Option<Player>>,
    slots: Slots,
    /// Slot of each connection, by the connection's id; used only when a connection leaves.
    by_connection: std::collections::HashMap<u32, Slot>,
    grid: DenseGrid,
    visibility: VisibilityConfig,
    /// Builds the views in parallel; `None` for one thread.
    pool: Option<rayon::ThreadPool>,
    /// The wire state of every placed player, by slot index.
    states: Vec<Option<EntityState>>,
    /// The wire states of the last ticks, for delta baselines.
    history: History,
    outbox: tokio_mpsc::Sender<Batch>,
    /// Batches the send task has emptied, to be filled again.
    recycle: mpsc::Receiver<Batch>,
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
        self.by_connection.len()
    }

    /// Whether no player is connected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_connection.is_empty()
    }

    /// Applies what arrived since the last tick, then sends every player its snapshot for `info.tick`.
    pub fn run_tick(&mut self, world: &mut World, info: &TickInfo) {
        self.apply_inbound(world, info);
        self.apply_states(world);
        self.grid.rebuild();
        self.send_snapshots(info.tick as u32);
        // Every visible set has been recomputed without the players who left: their slots may be reused.
        self.slots.release();
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
                    let Some(slot) = self.slots.alloc() else {
                        sender.close();
                        continue;
                    };
                    let tick_hz =
                        (std::time::Duration::from_secs(1).as_nanos() / info.dt.as_nanos().max(1)) as u16;
                    let _ = events.try_send(event_packet(
                        &ServerEvent::Joined {
                            entity: slot.wire_id(),
                            tick_hz,
                        },
                        info.tick,
                    ));
                    let index = slot.index as usize;
                    if self.players.len() <= index {
                        self.players.resize_with(index + 1, || None);
                        self.states.resize(index + 1, None);
                    }
                    self.by_connection.insert(id, slot);
                    self.players[index] = Some(Player {
                        slot,
                        entity: None,
                        sender,
                        events,
                        inbox,
                        view: View::new(),
                    });
                }
                Inbound::Leave { id } => {
                    let Some(slot) = self.by_connection.remove(&id) else {
                        continue;
                    };
                    let index = slot.index as usize;
                    if let Some(entity) = self.players[index].take().and_then(|p| p.entity) {
                        let _ = world.despawn(entity);
                    }
                    self.states[index] = None;
                    self.grid.remove(EntityId(slot.index));
                    self.slots.free(slot);
                }
            }
        }
    }

    /// Takes each player's latest state, if a new one arrived, into the world, the grid and the wire states.
    fn apply_states(&mut self, world: &mut World) {
        for player in self.players.iter_mut().flatten() {
            let Some(state) = player.inbox.state.lock().ok().and_then(|mut s| s.take()) else {
                continue;
            };
            let id = player.slot.wire_id();
            match player.entity {
                Some(entity) => {
                    if let Ok(mut current) = world.get::<&mut PlayerState>(entity) {
                        current.0 = state;
                    }
                }
                None => player.entity = Some(world.spawn((NetId(id), PlayerState(state)))),
            }
            let [x, y, _] = state.position.to_metres();
            self.grid.upsert(EntityId(player.slot.index), Point::new(x, y));
            self.states[player.slot.index as usize] = Some(EntityState {
                id,
                position: state.position,
                velocity: state.velocity,
                rotation: state.rotation,
            });
        }
    }

    fn send_snapshots(&mut self, tick: u32) {
        self.history.record(tick, &self.states);
        let (grid, history, config) = (&self.grid, &self.history, &self.visibility);
        let build = |player: &mut Player| {
            let ack = player.inbox.ack.load(Ordering::Relaxed);
            player
                .view
                .build(player.slot.index as usize, grid, history, config, tick, ack);
        };
        match &self.pool {
            Some(pool) => pool.install(|| {
                self.players
                    .par_iter_mut()
                    .filter_map(Option::as_mut)
                    .for_each(build);
            }),
            None => self.players.iter_mut().flatten().for_each(build),
        }
        let mut batch = self.recycle.try_recv().unwrap_or_default();
        for player in self.players.iter_mut().flatten() {
            let sender = batch.senders.len() as u32;
            let before = batch.packets.len();
            batch
                .packets
                .extend(player.view.out().map(|packet| (sender, packet)));
            if batch.packets.len() > before {
                batch.senders.push(player.sender.clone());
            }
        }
        if self.outbox.try_send(batch).is_err() {
            // The gateway is still sending the last ticks (or is gone): drop this one rather than queue more.
            self.traffic.dropped_ticks.fetch_add(1, Ordering::Relaxed);
        }
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
            .flatten()
            .filter(|p| p.inbox.wants_stats.load(Ordering::Relaxed))
        {
            let _ = player.events.try_send(packet.clone());
        }
    }
}

/// Appends a packet header to `out`.
fn write_header(out: &mut Vec<u8>, kind: PacketKind, sequence: u16, tick: u32) {
    let mut buf = [0; PacketHeader::SIZE];
    let header = PacketHeader { kind, sequence, tick };
    // The buffer is exactly the header size, so encoding cannot fail.
    let _ = header.encode(&mut buf);
    out.extend_from_slice(&buf);
}

fn event_packet(event: &ServerEvent, tick: u64) -> Vec<u8> {
    let mut packet = Vec::new();
    write_header(&mut packet, PacketKind::Event, 0, tick as u32);
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
