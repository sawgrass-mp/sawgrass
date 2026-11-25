//! Bots that misbehave on purpose (task BOT-4), so the server's defences are tested under load: the movement
//! checks (SRV-9), the per-connection limits (NET-12) and the overload policy (SRV-10).
//!
//! Each kind keeps its own [`KindCounters`], apart from the honest bots' [`crate::stats::Counters`], so a run's
//! honest figures still describe honest players.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use sawgrass_gateway::{Datagrams, Session, connect};
use sawgrass_protocol::game::ClientState;
use sawgrass_protocol::{PROTOCOL_VERSION, PacketKind, Position, Rotation, Velocity};
use tokio::time::Instant;

use crate::bot::{BotConfig, header};
use crate::walker::Walker;

/// How a bot misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Sends client states at [`FLOOD_HZ`]; the server should close it. Reconnects when it does.
    Flood,
    /// Sends a state 1 km from the last every tick, at velocity 0; the server should refuse the jumps.
    Teleport,
    /// Sends an event that is not a known `ClientEvent`; the server should close it. Reconnects when it does.
    BadEvent,
    /// Connects and closes at once, [`RECONNECT_HZ`] times a second.
    Reconnect,
}

/// Client states a second a flood bot sends.
pub const FLOOD_HZ: u32 = 1000;
/// Connections a second a reconnecting bot makes.
pub const RECONNECT_HZ: u32 = 5;

impl Kind {
    /// Every kind, in report order.
    pub const ALL: [Kind; 4] = [Kind::Flood, Kind::Teleport, Kind::BadEvent, Kind::Reconnect];

    /// The name on the command line and in the report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Kind::Flood => "flood",
            Kind::Teleport => "teleport",
            Kind::BadEvent => "bad-event",
            Kind::Reconnect => "reconnect",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Parses `flood=20,teleport=10`: how many bots of each kind. Kinds not named get none.
///
/// # Errors
///
/// An unknown kind or a count that is not a number.
pub fn parse(spec: &str) -> Result<[u64; 4], String> {
    let mut counts = [0; 4];
    for part in spec.split(',').filter(|p| !p.is_empty()) {
        let (name, count) = part
            .split_once('=')
            .ok_or_else(|| format!("--misbehave: {part:?} is not KIND=N"))?;
        let kind = Kind::ALL
            .into_iter()
            .find(|k| k.name() == name)
            .ok_or_else(|| format!("--misbehave: unknown kind {name:?}"))?;
        counts[kind.index()] = count.parse().map_err(|e| format!("--misbehave: {part:?}: {e}"))?;
    }
    Ok(counts)
}

/// What the bots of one kind did, summed.
#[derive(Debug, Default)]
pub struct KindCounters {
    /// Handshakes that completed.
    pub connects: AtomicU64,
    /// Connection attempts that failed.
    pub failed: AtomicU64,
    /// Connections the server closed while the bot still used them.
    pub closed: AtomicU64,
    /// Datagrams and events sent.
    pub sent: AtomicU64,
}

/// The counters of every kind.
#[derive(Debug, Default)]
pub struct Misbehaving {
    kinds: [KindCounters; 4],
}

impl Misbehaving {
    /// The counters of `kind`.
    #[must_use]
    pub fn of(&self, kind: Kind) -> &KindCounters {
        &self.kinds[kind.index()]
    }

    /// One line per kind that has bots in `counts`, for the run's report.
    #[must_use]
    pub fn render(&self, counts: &[u64; 4]) -> String {
        let mut out = String::new();
        for kind in Kind::ALL {
            let bots = counts[kind.index()];
            if bots == 0 {
                continue;
            }
            let c = self.of(kind);
            let load = |n: &AtomicU64| n.load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "misbehaving {}: {bots} bots, {} connects, {} failed, {} closed by the server, {} sent",
                kind.name(),
                load(&c.connects),
                load(&c.failed),
                load(&c.closed),
                load(&c.sent)
            );
        }
        out
    }
}

/// Runs misbehaving bot number `index` of `kind` until `stop`.
pub async fn run(config: Arc<BotConfig>, kind: Kind, index: u64, counters: Arc<Misbehaving>, stop: Instant) {
    let c = counters.of(kind);
    while Instant::now() < stop {
        let Some(session) = connect_once(&config, c).await else {
            // Refused or timed out: wait a little, as a real client would, before the next attempt.
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        match kind {
            Kind::Flood => flood(session, &config, index, c, stop).await,
            Kind::Teleport => teleport(session, &config, index, c, stop).await,
            Kind::BadEvent => bad_event(session, c, stop).await,
            Kind::Reconnect => {
                session.close();
                tokio::time::sleep(Duration::from_secs(1) / RECONNECT_HZ).await;
            }
        }
    }
}

async fn connect_once(config: &BotConfig, c: &KindCounters) -> Option<Session> {
    match connect(
        config.server,
        &config.server_name,
        config.roots.clone(),
        PROTOCOL_VERSION,
    )
    .await
    {
        Ok(session) => {
            c.connects.fetch_add(1, Ordering::Relaxed);
            Some(session)
        }
        Err(_) => {
            c.failed.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// Waits until the server closes `datagrams`, reading and discarding what it sends.
async fn closed(datagrams: Datagrams) {
    while datagrams.recv().await.is_ok() {}
}

/// Sends `state` with the right header; `false` once the connection is gone.
fn send_state(datagrams: &Datagrams, state: &ClientState, c: &KindCounters) -> bool {
    let mut packet = header(PacketKind::ClientState, 0, 0);
    let mut buf = [0; ClientState::SIZE];
    if state.encode(&mut buf).is_err() {
        return true;
    }
    packet.extend_from_slice(&buf);
    if datagrams.send(Bytes::from(packet)).is_err() {
        return false;
    }
    c.sent.fetch_add(1, Ordering::Relaxed);
    true
}

fn standing(position: [f32; 3]) -> Option<ClientState> {
    Some(ClientState {
        position: Position::from_metres(position).ok()?,
        velocity: Velocity::from_metres_per_second([0.0; 3]).ok()?,
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).ok()?,
    })
}

async fn flood(session: Session, config: &BotConfig, index: u64, c: &KindCounters, stop: Instant) {
    let (datagrams, _events_out, _events_in) = session.split();
    let Some(state) = standing(Walker::new(config.area, index).position()) else {
        return;
    };
    let gone = tokio::spawn(closed(datagrams.clone()));
    let mut interval = tokio::time::interval(Duration::from_secs(1) / FLOOD_HZ);
    while Instant::now() < stop && !gone.is_finished() {
        interval.tick().await;
        if !send_state(&datagrams, &state, c) {
            break;
        }
    }
    finish(&datagrams, gone, c, stop).await;
}

async fn teleport(session: Session, config: &BotConfig, index: u64, c: &KindCounters, stop: Instant) {
    let (datagrams, _events_out, _events_in) = session.split();
    let [x, y, z] = Walker::new(config.area, index).position();
    // Jump towards the map's centre, so every position stays inside it.
    let step = if x < 0.0 { 1000.0 } else { -1000.0 };
    let gone = tokio::spawn(closed(datagrams.clone()));
    let mut interval = tokio::time::interval(Duration::from_secs(1) / config.send_hz.max(1));
    let mut jumps = 0u32;
    while Instant::now() < stop && !gone.is_finished() {
        interval.tick().await;
        // 0, 1, ... 7 km from the start and back to it: every move but the return is a jump.
        let Some(state) = standing([x + step * (jumps % 8) as f32, y, z]) else {
            break;
        };
        jumps = jumps.wrapping_add(1);
        if !send_state(&datagrams, &state, c) {
            break;
        }
    }
    finish(&datagrams, gone, c, stop).await;
}

async fn bad_event(session: Session, c: &KindCounters, stop: Instant) {
    let (datagrams, mut events, _events_in) = session.split();
    let gone = tokio::spawn(closed(datagrams.clone()));
    let mut packet = header(PacketKind::Event, 0, 0);
    // No event has this tag.
    packet.extend_from_slice(&[0xEE, 1, 2, 3]);
    while Instant::now() < stop && !gone.is_finished() {
        if events.send(&packet).await.is_err() {
            break;
        }
        c.sent.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    finish(&datagrams, gone, c, stop).await;
}

/// Counts the connection as closed by the server if it ended before `stop`, and closes it otherwise.
async fn finish(datagrams: &Datagrams, gone: tokio::task::JoinHandle<()>, c: &KindCounters, stop: Instant) {
    if Instant::now() < stop {
        // Give the close a moment to arrive after a failed send.
        if tokio::time::timeout(Duration::from_millis(500), gone)
            .await
            .is_ok()
        {
            c.closed.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        gone.abort();
    }
    datagrams.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_names_counts_by_kind() {
        assert_eq!(parse("flood=3,reconnect=2"), Ok([3, 0, 0, 2]));
        assert_eq!(parse(""), Ok([0; 4]));
        assert!(parse("flood").is_err());
        assert!(parse("speed=1").is_err());
        assert!(parse("teleport=x").is_err());
    }
}
