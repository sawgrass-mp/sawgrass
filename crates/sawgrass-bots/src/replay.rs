//! Plays a session recording (task SRV-21) against a server: one connection per recorded connection, joining,
//! sending and leaving at the recorded times.
//!
//! Datagrams and events are sent as recorded, except the tick in a client state's header, which acknowledges the
//! newest snapshot the client received: the replaying connection puts its own newest one there, so the server's
//! delta snapshots stay against baselines it holds. Entity ids inside events (a payment's payee, for example)
//! are the recorded server's and may name someone else on the new one.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use sawgrass_gateway::{Connector, Session};
use sawgrass_protocol::recording::{FILE_HEADER, Record, RecordError, RecordKind, read_file_header};
use sawgrass_protocol::{PROTOCOL_VERSION, PacketHeader};
use tokio::time::Instant;

/// How long a connection whose leave was not recorded stays after its last send.
const UNRECORDED_LEAVE: Duration = Duration::from_secs(1);

/// One recorded connection: what it sent, with microseconds since the recording started.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Script {
    /// When it joined, if the recording saw the join.
    pub join: Option<u64>,
    /// Datagrams and events, in order.
    pub sends: Vec<(u64, RecordKind, Vec<u8>)>,
    /// When it left, if it did before the recording ended.
    pub leave: Option<u64>,
}

/// Reads a recording into one [`Script`] per connection, by connection id.
///
/// # Errors
///
/// If `file` is not a recording, was made by another protocol version, or has a damaged record. A recording cut
/// off inside its last record, as a killed server leaves it, is read up to that record.
pub fn parse(file: &[u8]) -> Result<BTreeMap<u32, Script>, String> {
    let version = read_file_header(file).ok_or("not a session recording")?;
    if version != PROTOCOL_VERSION {
        return Err(format!(
            "the recording speaks protocol {version}, these bots {PROTOCOL_VERSION}"
        ));
    }
    let mut scripts = BTreeMap::<u32, Script>::new();
    let mut rest = &file[FILE_HEADER..];
    while !rest.is_empty() {
        let (record, after) = match Record::decode(rest) {
            Ok(read) => read,
            Err(RecordError::Truncated) => break,
            Err(RecordError::UnknownKind(kind)) => return Err(format!("a record of unknown kind {kind}")),
        };
        let script = scripts.entry(record.connection).or_default();
        match record.kind {
            RecordKind::Join => script.join = Some(record.micros),
            RecordKind::Leave => script.leave = Some(record.micros),
            kind => script.sends.push((record.micros, kind, record.payload.to_vec())),
        }
        rest = after;
    }
    Ok(scripts)
}

/// What a replay did.
#[derive(Debug, Default)]
pub struct Replayed {
    /// Connections opened.
    pub connected: AtomicU64,
    /// Connections that could not be opened.
    pub failed: AtomicU64,
    /// Datagrams sent.
    pub datagrams: AtomicU64,
    /// Events sent.
    pub events: AtomicU64,
}

/// Plays `scripts` against `server`, `speed` times as fast as recorded, and returns when every connection has
/// left or ended. A connection whose join was not recorded joins at its first send.
pub async fn replay(
    scripts: BTreeMap<u32, Script>,
    server: SocketAddr,
    server_name: &str,
    connector: Arc<Connector>,
    speed: f64,
) -> Arc<Replayed> {
    let replayed = Arc::new(Replayed::default());
    let start = Instant::now();
    let at = move |micros: u64| start + Duration::from_secs_f64(micros as f64 / 1e6 / speed.max(1e-3));
    let mut tasks = Vec::new();
    for script in scripts.into_values() {
        let Some(join) = script.join.or(script.sends.first().map(|s| s.0)) else {
            continue;
        };
        let (connector, name, replayed) = (connector.clone(), server_name.to_owned(), replayed.clone());
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep_until(at(join)).await;
            let Ok(session) = connector.connect(server, &name, PROTOCOL_VERSION).await else {
                replayed.failed.fetch_add(1, Ordering::Relaxed);
                return;
            };
            replayed.connected.fetch_add(1, Ordering::Relaxed);
            play(session, script, at, &replayed).await;
        }));
    }
    for task in tasks {
        let _ = task.await;
    }
    replayed
}

async fn play(session: Session, script: Script, at: impl Fn(u64) -> Instant, replayed: &Replayed) {
    let (datagrams, mut events, mut incoming) = session.split();
    // The newest snapshot tick this connection received, for the acknowledgement in each state it sends.
    let newest = Arc::new(AtomicU32::new(0));
    let (reader, seen) = (datagrams.clone(), newest.clone());
    let read = tokio::spawn(async move {
        while let Ok((header, _)) = reader.recv().await {
            seen.fetch_max(header.tick, Ordering::Relaxed);
        }
    });
    // Events from the server are read and dropped, so their stream never fills.
    let drain = tokio::spawn(async move { while incoming.recv().await.is_ok() {} });
    for (micros, kind, mut payload) in script.sends {
        tokio::time::sleep_until(at(micros)).await;
        if kind == RecordKind::Datagram {
            if let Ok((mut header, _)) = PacketHeader::decode(&payload) {
                header.tick = newest.load(Ordering::Relaxed);
                let _ = header.encode(&mut payload);
            }
            if datagrams.send(Bytes::from(payload)).is_err() {
                break;
            }
            replayed.datagrams.fetch_add(1, Ordering::Relaxed);
        } else {
            if events.send(&payload).await.is_err() {
                break;
            }
            replayed.events.fetch_add(1, Ordering::Relaxed);
        }
    }
    match script.leave {
        Some(leave) => tokio::time::sleep_until(at(leave)).await,
        // The recording ended with the connection open: stay a moment, so the server applies the last state.
        None => tokio::time::sleep(UNRECORDED_LEAVE).await,
    }
    datagrams.close();
    read.abort();
    drain.abort();
}
