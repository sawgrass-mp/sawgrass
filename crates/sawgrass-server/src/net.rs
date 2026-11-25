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
//! That is the overload policy for sending (task SRV-10): a slow client loses snapshots, never the tick. When a
//! connection's 64 KiB datagram send buffer is full, quinn discards that connection's oldest unsent datagrams to
//! make room for the new ones, so the client gets the newest state and the tick never waits; when the gateway as a
//! whole falls behind, whole ticks are dropped and counted in [`Traffic::dropped_ticks`].
//!
//! Every player takes a dense [`Slot`] when the tick thread sees it join (task IM-4). The slot indexes the
//! per-player arrays here (players, wire states) and the ids in the grid and the visible sets; its
//! [`Slot::wire_id`] is the player's entity id in `Joined` and in snapshots. A freed slot is reused only after
//! the tick that removed it has recomputed every visible set (see [`crate::slots`]).
//!
//! A player's entity is spawned with a [`NetId`] and a [`PlayerState`]; the state is the last one the client
//! sent, applied as is. Validating movement (speed limits, teleports) belongs to the authority work and is not
//! done here.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use hecs::{Entity, World};
use rayon::prelude::*;
use sawgrass_gateway::{Datagrams, Gateway, Session};
use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
use sawgrass_protocol::game::{ClientEvent, ClientState, ServerEvent, ServerStats};
use sawgrass_protocol::ped::{Motion, PedBlock};
use sawgrass_protocol::{EntityState, PacketHeader, PacketKind};
use tokio::sync::mpsc as tokio_mpsc;

use crate::TickInfo;
use crate::interest::{DenseGrid, EntityId, Point, VisibilityConfig};
use crate::limits::{Admit, Bucket, Limits};
use crate::movement::{Movement, MovementRules, Violation};
use crate::slots::{Slot, Slots};
use crate::snapshot::{History, View};

/// The network id of a player's entity, the `id` in snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetId(pub u32);

/// A player who joined or left during a tick, by entity id ([`Slot::wire_id`]), for systems after `net` such as
/// scripts. See [`Players::roster`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Roster {
    /// The player was given an entity id.
    Joined(u32),
    /// The player left; the id may be given to someone else from the next tick.
    Left(u32),
}

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
    state: Mutex<Option<(ClientState, Option<PedBlock>)>>,
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListenConfig {
    /// Let clients ask for [`ServerStats`] (load tests); otherwise the request is ignored.
    pub stats_to_clients: bool,
    /// Connections, handshakes included, above which new ones are refused.
    pub max_players: usize,
    /// Threads that build players' snapshots in parallel. 1 builds them on the tick thread.
    pub tick_threads: usize,
    /// Rates, handshakes and connections per address each connection is held to.
    pub limits: Limits,
    /// What a client's movement is checked against.
    pub movement: MovementRules,
}

impl Default for ListenConfig {
    /// No stats for clients, and the architecture's target of 4096 players.
    fn default() -> Self {
        Self {
            stats_to_clients: false,
            max_players: 4096,
            tick_threads: 1,
            limits: Limits::default(),
            movement: MovementRules::default(),
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
    /// Client datagrams dropped undecoded because the connection sent more than its rate.
    pub dropped_states: AtomicU64,
    /// Client events dropped because the connection sent more than its rate.
    pub dropped_events: AtomicU64,
    /// Connections closed for a sustained flood of datagrams or events.
    pub closed_floods: AtomicU64,
    /// Connections closed for an event that is not a known [`ClientEvent`].
    pub closed_bad_events: AtomicU64,
    /// Connection attempts from an address QUIC had not validated yet, answered with a Retry.
    pub retried_handshakes: AtomicU64,
    /// Connection attempts refused because too many handshakes were in progress.
    pub refused_handshakes: AtomicU64,
    /// Connection attempts refused because their address had too many connections.
    pub refused_per_address: AtomicU64,
    /// Client states refused by [`Movement::check`], all players together: outside the map.
    pub refused_bounds: AtomicU64,
    /// Client states refused for a velocity above the speed limit.
    pub refused_speed: AtomicU64,
    /// Client states refused as a move too long for the time since the last accepted one.
    pub refused_teleport: AtomicU64,
}

/// The address a connection counts against: an IPv4 address, or the /64 network of an IPv6 one, since one IPv6 host
/// usually holds a whole /64.
fn address_group(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let bits = u128::from(v6) & !((1u128 << 64) - 1);
            IpAddr::V6(bits.into())
        }
        v4 => v4,
    }
}

/// Connections, handshakes included, by address group ([`address_group`]).
type PerAddress = Arc<Mutex<HashMap<IpAddr, usize>>>;

/// One connection counted against its address; dropping it takes the count back.
struct AddressGuard {
    counts: PerAddress,
    ip: IpAddr,
}

impl AddressGuard {
    /// Counts a connection from `ip`, or returns `None` if the address already has `max`.
    fn take(counts: &PerAddress, ip: IpAddr, max: usize) -> Option<Self> {
        let mut map = counts.lock().ok()?;
        let count = map.entry(ip).or_insert(0);
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(Self {
            counts: counts.clone(),
            ip,
        })
    }
}

impl Drop for AddressGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.counts.lock()
            && let Some(count) = map.get_mut(&self.ip)
        {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

/// Starts accepting players on `gateway`, on the current tokio runtime, and returns the tick-side half.
pub fn listen(gateway: Gateway, config: ListenConfig) -> Players {
    let (inbound_tx, inbound_rx) = mpsc::channel();
    let traffic = Arc::new(Traffic::default());
    let next_id = AtomicU32::new(1);
    let active = Arc::new(AtomicUsize::new(0));
    let handshakes = Arc::new(AtomicUsize::new(0));
    let per_address: PerAddress = Arc::default();
    let limits = config.limits;
    let accept_traffic = traffic.clone();
    let (outbox, recycle) = spawn_sender(traffic.clone());
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let close = shutdown.clone();
    tokio::spawn(async move {
        loop {
            let incoming = tokio::select! {
                incoming = gateway.accept() => incoming,
                () = close.notified() => {
                    gateway.close();
                    break;
                }
            };
            let Some(incoming) = incoming else {
                break;
            };
            if !incoming.address_validated() {
                // Every unproven address gets a Retry first, a stateless packet, so a spoofed attempt takes no
                // place below; a real client comes back as a validated attempt one round trip later.
                accept_traffic.retried_handshakes.fetch_add(1, Ordering::Relaxed);
                incoming.retry();
                continue;
            }
            if active.load(Ordering::Relaxed) >= config.max_players {
                incoming.refuse();
                continue;
            }
            if handshakes.load(Ordering::Relaxed) >= limits.max_handshakes {
                accept_traffic.refused_handshakes.fetch_add(1, Ordering::Relaxed);
                incoming.refuse();
                continue;
            }
            let ip = address_group(incoming.remote_address().ip());
            let Some(guard) = AddressGuard::take(&per_address, ip, limits.max_per_address) else {
                accept_traffic.refused_per_address.fetch_add(1, Ordering::Relaxed);
                incoming.refuse();
                continue;
            };
            active.fetch_add(1, Ordering::Relaxed);
            handshakes.fetch_add(1, Ordering::Relaxed);
            let inbound = inbound_tx.clone();
            let traffic = accept_traffic.clone();
            let active = active.clone();
            let handshakes = handshakes.clone();
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let session = incoming.handshake().await;
                handshakes.fetch_sub(1, Ordering::Relaxed);
                if let Ok(session) = session {
                    serve(session, id, inbound, traffic, config.stats_to_clients, limits).await;
                }
                drop(guard);
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
        movement: config.movement,
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
        peds: Vec::new(),
        infos: Vec::new(),
        info_ticks: Vec::new(),
        history: History::new(),
        outbox,
        recycle,
        pending_stats: Arc::new(Mutex::new(None)),
        shutdown: Shutdown(shutdown),
        roster: None,
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
    limits: Limits,
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
    let events_traffic = traffic.clone();
    let events_connection = datagrams.clone();
    tokio::spawn(async move {
        let mut bucket = Bucket::for_events(&limits, Instant::now());
        while let Ok((_, packet)) = event_receiver.recv().await {
            match bucket.admit(Instant::now()) {
                Admit::Pass => {}
                Admit::Drop => {
                    events_traffic.dropped_events.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Admit::Close => {
                    events_traffic.closed_floods.fetch_add(1, Ordering::Relaxed);
                    events_connection.close();
                    break;
                }
            }
            match ClientEvent::decode(&packet[PacketHeader::SIZE..]) {
                Ok(ClientEvent::RequestStats) if stats_to_clients => {
                    events_inbox.wants_stats.store(true, Ordering::Relaxed);
                }
                Ok(_) => {}
                Err(_) => {
                    // No valid client sends one, so whatever sent it is not a client to keep.
                    events_traffic.closed_bad_events.fetch_add(1, Ordering::Relaxed);
                    events_connection.close();
                    break;
                }
            }
        }
    });
    let mut bucket = Bucket::for_states(&limits, Instant::now());
    while let Ok((header, packet)) = datagrams.recv().await {
        traffic
            .received_bytes
            .fetch_add(packet.len() as u64, Ordering::Relaxed);
        match bucket.admit(Instant::now()) {
            Admit::Pass => {}
            Admit::Drop => {
                traffic.dropped_states.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Admit::Close => {
                traffic.closed_floods.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
        if header.kind != PacketKind::ClientState {
            continue;
        }
        if let Ok(state) = ClientState::decode_with_ped(&packet[PacketHeader::SIZE..])
            && let Ok(mut slot) = inbox.state.lock()
        {
            *slot = Some(state);
            // The newest acknowledgement wins: a reordered datagram does not move it back.
            inbox.ack.fetch_max(header.tick, Ordering::Relaxed);
        }
    }
    datagrams.close();
    let _ = inbound.send(Inbound::Leave { id });
}

/// Health a player spawns with; a placeholder until the roleplay base sets it (ADR 0006, section 5).
const SPAWN_HEALTH: u8 = 100;

/// The info a player's ped block is sent as.
fn ped_info(ped: &PedBlock) -> Info {
    // PED_FIELDS is below MAX_FIELDS, so this cannot fail.
    Info::with_fields(EntityKind::Player, AssetId::NONE, &ped.to_fields()).unwrap_or(Info::PLAYER)
}

/// The server's ped block after a client sent `client` (ADR 0006, section 3): the client's motion, flags, aim
/// and actions are presentation and taken as sent; its weapon is taken until the server checks it against what the
/// character holds (SRV-16). Vehicle, seat, health and armour are the server's, and so are the motions "in
/// vehicle" and "dead": a client cannot enter or leave them, and while the server holds one, it keeps it.
fn merge_ped(server: &PedBlock, client: &PedBlock) -> PedBlock {
    let held = matches!(server.motion, Motion::InVehicle | Motion::Dead);
    let motion = match client.motion {
        _ if held => server.motion,
        Motion::InVehicle | Motion::Dead => Motion::Idle,
        m => m,
    };
    PedBlock {
        motion,
        flags: client.flags,
        weapon: client.weapon,
        aim_yaw: client.aim_yaw,
        aim_pitch: client.aim_pitch,
        full_body: client.full_body,
        upper_body: client.upper_body,
        ..*server
    }
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
    movement: Movement,
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
    /// What every player's movement is checked against.
    movement: MovementRules,
    /// Builds the views in parallel; `None` for one thread.
    pool: Option<rayon::ThreadPool>,
    /// The wire state of every placed player, by slot index.
    states: Vec<Option<EntityState>>,
    /// Every player's ped block, by slot index, as the server holds it.
    peds: Vec<PedBlock>,
    /// The info each slot sends, built from `peds`, and the tick it last changed at.
    infos: Vec<Info>,
    info_ticks: Vec<u32>,
    /// The wire states of the last ticks, for delta baselines.
    history: History,
    outbox: tokio_mpsc::Sender<Batch>,
    /// Batches the send task has emptied, to be filled again.
    recycle: mpsc::Receiver<Batch>,
    pending_stats: Arc<Mutex<Option<ServerStats>>>,
    shutdown: Shutdown,
    roster: Option<mpsc::Sender<Roster>>,
}

/// Closes the gateway from outside the tick: every connection, and no new ones.
#[derive(Debug, Clone)]
pub struct Shutdown(Arc<tokio::sync::Notify>);

impl Shutdown {
    /// Closes every connection and stops accepting new ones. The close goes out on the gateway's runtime, which
    /// must keep running for a moment after this call for clients to hear it.
    pub fn close(&self) {
        self.0.notify_one();
    }
}

impl Players {
    /// Shared traffic counters.
    #[must_use]
    pub fn traffic(&self) -> Arc<Traffic> {
        self.traffic.clone()
    }

    /// A handle that closes the gateway, for stopping the server when the tick cannot go on.
    #[must_use]
    pub fn shutdown(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Where to put [`ServerStats`] for the next tick to send to the players that asked for them.
    #[must_use]
    pub fn stats_slot(&self) -> Arc<Mutex<Option<ServerStats>>> {
        self.pending_stats.clone()
    }

    /// Joins and leaves from now on, in the order the ticks apply them.
    pub fn roster(&mut self) -> mpsc::Receiver<Roster> {
        let (tx, rx) = mpsc::channel();
        self.roster = Some(tx);
        rx
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
        self.apply_states(world, info.tick as u32);
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
                        self.peds.resize(index + 1, PedBlock::default());
                        self.infos.resize(index + 1, Info::PLAYER);
                        self.info_ticks.resize(index + 1, 0);
                    }
                    self.by_connection.insert(id, slot);
                    if let Some(roster) = &self.roster {
                        let _ = roster.send(Roster::Joined(slot.wire_id()));
                    }
                    self.peds[index] = PedBlock {
                        health: SPAWN_HEALTH,
                        ..PedBlock::default()
                    };
                    self.infos[index] = ped_info(&self.peds[index]);
                    self.info_ticks[index] = info.tick as u32;
                    self.players[index] = Some(Player {
                        slot,
                        entity: None,
                        sender,
                        events,
                        inbox,
                        view: View::new(),
                        movement: Movement::new(),
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
                    if let Some(roster) = &self.roster {
                        let _ = roster.send(Roster::Left(slot.wire_id()));
                    }
                    self.slots.free(slot);
                }
            }
        }
    }

    /// Takes each player's latest state, if a new one arrived and passes the movement checks, into the world, the
    /// grid and the wire states. A refused state is counted and dropped: the player stays where it was.
    fn apply_states(&mut self, world: &mut World, tick: u32) {
        let rules = &self.movement;
        let now = std::time::Instant::now();
        for player in self.players.iter_mut().flatten() {
            let Some((state, ped)) = player.inbox.state.lock().ok().and_then(|mut s| s.take()) else {
                continue;
            };
            if let Err(violation) = player.movement.check(rules, &state, now) {
                let counter = match violation {
                    Violation::Bounds => &self.traffic.refused_bounds,
                    Violation::Speed => &self.traffic.refused_speed,
                    Violation::Teleport => &self.traffic.refused_teleport,
                };
                counter.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let id = player.slot.wire_id();
            match player.entity {
                Some(entity) => {
                    if let Ok(mut current) = world.get::<&mut PlayerState>(entity) {
                        current.0 = state;
                    }
                }
                None => player.entity = Some(world.spawn((NetId(id), PlayerState(state)))),
            }
            let index = player.slot.index as usize;
            if let Some(ped) = ped {
                let merged = merge_ped(&self.peds[index], &ped);
                if merged != self.peds[index] {
                    self.peds[index] = merged;
                    self.infos[index] = ped_info(&merged);
                    self.info_ticks[index] = tick;
                }
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
        let (grid, config) = (&self.grid, &self.visibility);
        let (history, states, players) = (&mut self.history, &self.states, &mut self.players);
        let (infos, info_ticks) = (&self.infos[..], &self.info_ticks[..]);
        // The baselines the players acknowledged, so the changes against each are encoded once for all of them.
        let acks = |players: &[Option<Player>]| -> Vec<u32> {
            players
                .iter()
                .flatten()
                .map(|p| p.inbox.ack.load(Ordering::Relaxed))
                .collect()
        };
        let build = |history: &History, player: &mut Player| {
            let ack = player.inbox.ack.load(Ordering::Relaxed);
            player
                .view
                .build(player.slot.index as usize, grid, history, config, tick, ack);
        };
        match &self.pool {
            Some(pool) => pool.install(|| {
                history.record(tick, states, infos, info_ticks, acks(players));
                let history = &*history;
                players
                    .par_iter_mut()
                    .filter_map(Option::as_mut)
                    .for_each(|player| build(history, player));
            }),
            None => {
                history.record(tick, states, infos, info_ticks, acks(players));
                players
                    .iter_mut()
                    .flatten()
                    .for_each(|player| build(history, player));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_counts_per_64_and_ipv4_per_address() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().expect("v6");
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().expect("v6");
        let other: IpAddr = "2001:db8:1:3::1".parse().expect("v6");
        assert_eq!(address_group(a), address_group(b));
        assert_ne!(address_group(a), address_group(other));
        let v4: IpAddr = "192.0.2.7".parse().expect("v4");
        let mapped: IpAddr = "::ffff:192.0.2.7".parse().expect("mapped");
        assert_eq!(address_group(v4), v4);
        assert_eq!(address_group(mapped), v4);
    }

    #[test]
    fn a_client_sets_its_presentation_but_not_the_servers_fields() {
        use sawgrass_protocol::ped::Action;
        let server = PedBlock {
            health: 80,
            armour: 20,
            ..PedBlock::default()
        };
        let client = PedBlock {
            motion: Motion::Run,
            flags: PedBlock::AIMING,
            weapon: AssetId(9),
            aim_yaw: 3,
            aim_pitch: 130,
            vehicle: 77,
            seat: 1,
            full_body: Action {
                action: AssetId(5),
                start_tick: 40,
                flags: Action::LOOP,
            },
            health: 255,
            armour: 255,
            ..PedBlock::default()
        };
        let merged = merge_ped(&server, &client);
        assert_eq!(
            merged,
            PedBlock {
                vehicle: 0,
                seat: 0,
                health: 80,
                armour: 20,
                ..client
            }
        );
        // A client cannot claim a seat or death, nor leave either while the server holds it.
        for claimed in [Motion::InVehicle, Motion::Dead] {
            let m = merge_ped(
                &server,
                &PedBlock {
                    motion: claimed,
                    ..client
                },
            );
            assert_eq!(m.motion, Motion::Idle);
        }
        let dead = PedBlock {
            motion: Motion::Dead,
            ..server
        };
        assert_eq!(merge_ped(&dead, &client).motion, Motion::Dead);
    }
}
