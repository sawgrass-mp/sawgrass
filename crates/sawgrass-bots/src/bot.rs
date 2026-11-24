//! One bot: connects through the gateway like a player, sends its state every client tick and reads snapshots.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use sawgrass_gateway::{Datagrams, RootCertStore, connect};
use sawgrass_protocol::game::{ClientEvent, ClientState, ServerEvent, decode_snapshot};
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
    let reader = tokio::spawn(read_snapshots(
        datagrams.clone(),
        counters.clone(),
        recorder.map(|r| (r, index)),
    ));
    let ended_early = send_states(&config, index, &datagrams, &counters, stop).await;
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
        let mut packet = header(PacketKind::ClientState, sequence, 0);
        sequence = sequence.wrapping_add(1);
        let mut buf = [0; ClientState::SIZE];
        if state.encode(&mut buf).is_err() {
            continue;
        }
        packet.extend_from_slice(&buf);
        let len = packet.len() as u64;
        if datagrams.send(Bytes::from(packet)).is_err() {
            return true;
        }
        Counters::add(&counters.sent_datagrams, 1);
        Counters::add(&counters.sent_bytes, len);
    }
}

async fn read_snapshots(datagrams: Datagrams, counters: Arc<Counters>, record: Option<(Arc<Recorder>, u64)>) {
    let mut last_tick = None;
    while let Ok((header, packet)) = datagrams.recv().await {
        if header.kind != PacketKind::Snapshot {
            continue;
        }
        Counters::add(&counters.received_datagrams, 1);
        Counters::add(&counters.received_bytes, packet.len() as u64);
        if let Some((recorder, index)) = &record {
            recorder.write(*index, &packet);
        }
        if let Ok(n) = decode_snapshot(&packet[PacketHeader::SIZE..], |_| {}) {
            Counters::add(&counters.received_entities, n as u64);
        }
        if last_tick != Some(header.tick) {
            last_tick = Some(header.tick);
            Counters::add(&counters.ticks_seen, 1);
        }
    }
}

fn client_state(walker: &Walker) -> Option<ClientState> {
    Some(ClientState {
        position: Position::from_metres(walker.position()).ok()?,
        velocity: Velocity::from_metres_per_second(walker.velocity()).ok()?,
        rotation: Rotation::from_quaternion(walker.rotation()).ok()?,
    })
}

fn header(kind: PacketKind, sequence: u16, tick: u32) -> Vec<u8> {
    let mut out = vec![0; PacketHeader::SIZE];
    let header = PacketHeader { kind, sequence, tick };
    // The buffer is exactly the header size, so encoding cannot fail.
    let _ = header.encode(&mut out);
    out
}
