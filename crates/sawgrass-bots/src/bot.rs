//! One bot: connects through the gateway like a player, sends its state every client tick and reads snapshots.
//!
//! Snapshots are delta-encoded (NET-14): the bot reassembles each tick's fragments, decodes it against the
//! baseline it names from the ticks it decoded lately, and acknowledges the newest one in the tick field of its
//! client states' headers, as a game client does.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use sawgrass_gateway::{Datagrams, RootCertStore, connect};
use sawgrass_protocol::EntityState;
use sawgrass_protocol::delta::{Baseline, Reassembler, baseline_age, decode_delta};
use sawgrass_protocol::game::{ClientEvent, ClientState, ServerEvent};
use sawgrass_protocol::ped::{Motion, PedBlock};
use sawgrass_protocol::{PROTOCOL_VERSION, PacketHeader, PacketKind, Position, Rotation, Velocity};
use tokio::time::{Instant, MissedTickBehavior};

use crate::record::Recorder;
use crate::stats::Counters;
use crate::walker::{Area, Walker};

/// What every bot of a run shares.
#[derive(Debug, Clone)]
pub struct BotConfig {
    /// The gateway's address.
    pub server: SocketAddr,
    /// The name in the gateway's certificate.
    pub server_name: String,
    /// The certificates the bot trusts.
    pub roots: RootCertStore,
    /// State packets per second, like a game client's send rate.
    pub send_hz: u32,
    /// Where the bots move.
    pub area: Area,
    /// Records the snapshots of the first few bots, if set.
    pub record: Option<Arc<Recorder>>,
}

/// What one bot learned from the server, for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Joined {
    /// The bot's entity id.
    pub entity: u32,
    /// Server ticks per second.
    pub tick_hz: u16,
}

/// Connects bot number `index`, then plays until `stop` is reached or the connection ends. Bot 0 asks the server
/// for its stats when `want_stats` is set; one asking is enough. Returns what the server sent on joining, or
/// `None` if the connection failed.
pub async fn run(
    config: Arc<BotConfig>,
    index: u64,
    counters: Arc<Counters>,
    want_stats: bool,
    stop: Instant,
    joined: Option<tokio::sync::oneshot::Sender<Joined>>,
) {
    let session = match connect(
        config.server,
        &config.server_name,
        config.roots.clone(),
        PROTOCOL_VERSION,
    )
    .await
    {
        Ok(session) => session,
        Err(e) => {
            Counters::add(&counters.failed, 1);
            counters.error(e.to_string());
            return;
        }
    };
    Counters::add(&counters.connected, 1);
    let (datagrams, mut event_sender, mut event_receiver) = session.split();

    if want_stats {
        let mut request = header(PacketKind::Event, 0, 0);
        ClientEvent::RequestStats.encode(&mut request);
        let _ = event_sender.send(&request).await;
    }

    let events_counters = counters.clone();
    tokio::spawn(async move {
        let mut joined = joined;
        while let Ok((_, packet)) = event_receiver.recv().await {
            match ServerEvent::decode(&packet[PacketHeader::SIZE..]) {
                Ok(ServerEvent::Joined { entity, tick_hz }) => {
                    if let Some(tx) = joined.take() {
                        let _ = tx.send(Joined { entity, tick_hz });
                    }
                }
                Ok(ServerEvent::Stats(stats)) => {
                    if let Ok(mut slot) = events_counters.server.lock() {
                        *slot = Some(stats);
                    }
                }
                Err(_) => {}
            }
        }
    });

    let recorder = config.record.clone().filter(|r| r.records(index));
    let ack = Arc::new(AtomicU32::new(0));
    let reader = tokio::spawn(read_snapshots(
        datagrams.clone(),
        ack.clone(),
        counters.clone(),
        recorder.map(|r| (r, index)),
    ));
    let ended_early = send_states(&config, index, &datagrams, &ack, &counters, stop).await;
    datagrams.close();
    reader.abort();
    if ended_early {
        Counters::add(&counters.dropped, 1);
    }
}

/// Sends the walker's state every client tick until `stop`. Returns whether the connection ended first.
async fn send_states(
    config: &BotConfig,
    index: u64,
    datagrams: &Datagrams,
    ack: &AtomicU32,
    counters: &Counters,
    stop: Instant,
) -> bool {
    let period = Duration::from_secs(1) / config.send_hz.max(1);
    let mut walker = Walker::new(config.area, index);
    let mut interval = tokio::time::interval(period);
    // A late generator must show as lateness, not catch up in bursts that hide it.
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut sequence = 0u16;
    let mut last = Instant::now();
    loop {
        let scheduled = interval.tick().await;
        let now = Instant::now();
        if now >= stop {
            return false;
        }
        counters
            .send_lateness
            .record(now.saturating_duration_since(scheduled));
        walker.step((now - last).as_secs_f32());
        last = now;
        let Some(state) = client_state(&walker) else {
            continue;
        };
        let mut packet = header(PacketKind::ClientState, sequence, ack.load(Ordering::Relaxed));
        sequence = sequence.wrapping_add(1);
        if state.encode_with_ped(&ped_block(&walker), &mut packet).is_err() {
            continue;
        }
        let len = packet.len() as u64;
        if datagrams.send(Bytes::from(packet)).is_err() {
            return true;
        }
        Counters::add(&counters.sent_datagrams, 1);
        Counters::add(&counters.sent_bytes, len);
    }
}

/// Decoded snapshots a bot keeps as baselines, as many as the server keeps history for.
const KEPT_TICKS: usize = 16;

async fn read_snapshots(
    datagrams: Datagrams,
    ack: Arc<AtomicU32>,
    counters: Arc<Counters>,
    record: Option<(Arc<Recorder>, u64)>,
) {
    let mut reassembler = Reassembler::new();
    let mut kept: Vec<(Option<u32>, Vec<EntityState>)> = vec![(None, Vec::new()); KEPT_TICKS];
    let mut decoded = Vec::new();
    while let Ok((header, packet)) = datagrams.recv().await {
        if header.kind != PacketKind::DeltaSnapshot {
            continue;
        }
        Counters::add(&counters.received_datagrams, 1);
        Counters::add(&counters.received_bytes, packet.len() as u64);
        if let Some((recorder, index)) = &record {
            recorder.write(*index, &packet);
        }
        let Ok(Some(payload)) = reassembler.push(header.tick, &packet[PacketHeader::SIZE..]) else {
            continue;
        };
        let tick = header.tick;
        let base = match baseline_age(payload) {
            Ok(0) => None,
            Ok(age) => match tick.checked_sub(age) {
                Some(t) if kept[t as usize % KEPT_TICKS].0 == Some(t) => Some(Baseline {
                    tick: t,
                    entities: &kept[t as usize % KEPT_TICKS].1,
                }),
                // A baseline this bot no longer has: wait for one against a tick it acknowledged since.
                _ => continue,
            },
            Err(_) => continue,
        };
        if decode_delta(payload, tick, base, &mut decoded).is_err() {
            continue;
        }
        Counters::add(&counters.received_entities, decoded.len() as u64);
        Counters::add(&counters.ticks_seen, 1);
        let slot = &mut kept[tick as usize % KEPT_TICKS];
        slot.0 = Some(tick);
        std::mem::swap(&mut slot.1, &mut decoded);
        ack.fetch_max(tick, Ordering::Relaxed);
    }
}

fn client_state(walker: &Walker) -> Option<ClientState> {
    Some(ClientState {
        position: Position::from_metres(walker.position()).ok()?,
        velocity: Velocity::from_metres_per_second(walker.velocity()).ok()?,
        rotation: Rotation::from_quaternion(walker.rotation()).ok()?,
    })
}

/// The bot's ped block: standing while it pauses, otherwise moving by its speed, so the server relays block
/// changes as a crowd of real players would make them.
fn ped_block(walker: &Walker) -> PedBlock {
    let [vx, vy, _] = walker.velocity();
    let speed = vx.hypot(vy);
    let motion = if speed < 0.1 {
        Motion::Idle
    } else if speed < 2.5 {
        Motion::Walk
    } else if speed < 10.0 {
        Motion::Run
    } else {
        // The walker's driving legs; a bot drives no vehicle until BOT-5.
        Motion::Sprint
    };
    PedBlock {
        motion,
        ..PedBlock::default()
    }
}

pub(crate) fn header(kind: PacketKind, sequence: u16, tick: u32) -> Vec<u8> {
    let mut out = vec![0; PacketHeader::SIZE];
    let header = PacketHeader { kind, sequence, tick };
    // The buffer is exactly the header size, so encoding cannot fail.
    let _ = header.encode(&mut out);
    out
}
