//! Payloads that follow the [`PacketHeader`](crate::PacketHeader) of each packet kind.
//!
//! | Kind | Payload |
//! |---|---|
//! | [`ClientState`](crate::PacketKind::ClientState) | [`ClientState`]: position, velocity, rotation, 19 bytes |
//! | [`Snapshot`](crate::PacketKind::Snapshot) | count (`u8`), then that many [`EntityState`]s |
//! | [`DeltaSnapshot`](crate::PacketKind::DeltaSnapshot) | a fragment byte and part of a delta payload ([`crate::delta`]) |
//! | [`Event`](crate::PacketKind::Event) | [`ServerEvent`] or [`ClientEvent`]: a code byte, then its fields |
//!
//! The server sends delta snapshots since protocol version 3; the whole [`Snapshot`](crate::PacketKind::Snapshot)
//! format stays for tools and tests that read older recordings. A whole snapshot that holds more than
//! [`SNAPSHOT_MAX_ENTITIES`] entities is split over several datagrams with the same tick, so each one fits in
//! [`MAX_DATAGRAM`] bytes on any path quinn accepts.

use crate::{
    DecodeError, EncodeError, EntityState, PacketHeader, Position, Reader, Rotation, Velocity, Writer,
};

/// Largest datagram either side sends, header included. QUIC guarantees 1,200-byte UDP payloads; this leaves
/// room for the QUIC packet and datagram frame overhead.
pub const MAX_DATAGRAM: usize = 1100;

/// Most entities in one snapshot datagram.
pub const SNAPSHOT_MAX_ENTITIES: usize = (MAX_DATAGRAM - PacketHeader::SIZE - 1) / EntityState::SIZE;

/// A player's own state, sent by the client every client tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientState {
    /// Where the player is.
    pub position: Position,
    /// How fast it moves.
    pub velocity: Velocity,
    /// Which way it faces.
    pub rotation: Rotation,
}

impl ClientState {
    /// Wire size in bytes.
    pub const SIZE: usize = Position::SIZE + Velocity::SIZE + Rotation::SIZE;

    /// Writes the state to the start of `out` and returns the number of bytes written.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let mut w = Writer::new(out, Self::SIZE)?;
        self.position.write(&mut w);
        self.velocity.write(&mut w);
        self.rotation.write(&mut w);
        Ok(w.pos)
    }

    /// Reads a whole payload. Bytes after the state are an error: one encoding per packet (NET-4).
    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader { input };
        let state = Self {
            position: Position::read(&mut r)?,
            velocity: Velocity::read(&mut r)?,
            rotation: Rotation::read(&mut r)?,
        };
        r.finish()?;
        Ok(state)
    }
}

/// Appends a snapshot payload (count, then the states) to `out`. Fails if there are more than
/// [`SNAPSHOT_MAX_ENTITIES`] states.
pub fn encode_snapshot(states: &[EntityState], out: &mut Vec<u8>) -> Result<(), EncodeError> {
    if states.len() > SNAPSHOT_MAX_ENTITIES {
        return Err(EncodeError::OutOfRange);
    }
    // At most SNAPSHOT_MAX_ENTITIES (47), so it fits a byte.
    out.push(states.len() as u8);
    let mut buf = [0; EntityState::SIZE];
    for state in states {
        state.encode(&mut buf)?;
        out.extend_from_slice(&buf);
    }
    Ok(())
}

/// Reads a whole snapshot payload and calls `visit` for each state. Returns the number of states.
///
/// The length is checked first: a payload that is cut short, longer than its count says, or counts more than
/// [`SNAPSHOT_MAX_ENTITIES`] is rejected before `visit` sees any state.
pub fn decode_snapshot(input: &[u8], mut visit: impl FnMut(EntityState)) -> Result<usize, DecodeError> {
    let mut r = Reader { input };
    let [count] = r.array::<1>()?;
    let count = usize::from(count);
    if count > SNAPSHOT_MAX_ENTITIES {
        return Err(DecodeError::OutOfRange);
    }
    match r.input.len().cmp(&(count * EntityState::SIZE)) {
        std::cmp::Ordering::Less => return Err(DecodeError::Truncated),
        std::cmp::Ordering::Greater => return Err(DecodeError::TrailingBytes),
        std::cmp::Ordering::Equal => {}
    }
    let mut rest = r.input;
    for _ in 0..count {
        let (state, tail) = EntityState::decode(rest)?;
        visit(state);
        rest = tail;
    }
    Ok(count)
}

/// Tick-duration and traffic figures the server reports to load-test clients that ask for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ServerStats {
    /// Median tick duration over the server's metrics window, in microseconds.
    pub tick_p50_us: u32,
    /// 99th percentile tick duration, in microseconds.
    pub tick_p99_us: u32,
    /// Longest tick in the window, in microseconds.
    pub tick_max_us: u32,
    /// Tick interval, in microseconds.
    pub tick_interval_us: u32,
    /// Ticks that took longer than the interval, since start.
    pub overruns: u32,
    /// Players connected.
    pub players: u32,
    /// Server process CPU over the last report period, in thousandths of a core; `u32::MAX` if unknown.
    pub cpu_millicores: u32,
    /// CPU of the tick thread alone over the same period; the rest of the process is mostly the network.
    pub tick_thread_millicores: u32,
    /// Datagram bytes (headers included, QUIC overhead not) sent to all players per second over the period.
    pub sent_bytes_per_sec: u64,
    /// Datagram bytes received from all players per second over the period.
    pub received_bytes_per_sec: u64,
}

impl ServerStats {
    const SIZE: usize = 8 * 4 + 2 * 8;
}

/// Event codes. The values are part of the wire format.
const JOINED: u8 = 1;
const STATS: u8 = 2;
const REQUEST_STATS: u8 = 128;

/// An event from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerEvent {
    /// The player's character was spawned with this entity id; snapshots arrive at `tick_hz`.
    Joined {
        /// The player's own entity id.
        entity: u32,
        /// Server ticks per second.
        tick_hz: u16,
    },
    /// Server figures, sent periodically to clients that sent [`ClientEvent::RequestStats`] when the server
    /// allows it.
    Stats(ServerStats),
}

impl ServerEvent {
    /// Appends the payload (code and fields) to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Joined { entity, tick_hz } => {
                out.push(JOINED);
                out.extend_from_slice(&entity.to_le_bytes());
                out.extend_from_slice(&tick_hz.to_le_bytes());
            }
            Self::Stats(s) => {
                out.push(STATS);
                for v in [
                    s.tick_p50_us,
                    s.tick_p99_us,
                    s.tick_max_us,
                    s.tick_interval_us,
                    s.overruns,
                    s.players,
                    s.cpu_millicores,
                    s.tick_thread_millicores,
                ] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(&s.sent_bytes_per_sec.to_le_bytes());
                out.extend_from_slice(&s.received_bytes_per_sec.to_le_bytes());
            }
        }
    }

    /// Reads a whole payload. Bytes after the event are an error.
    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader { input };
        let [code] = r.array::<1>()?;
        let event = match code {
            JOINED => Ok(Self::Joined {
                entity: u32::from_le_bytes(r.array()?),
                tick_hz: u16::from_le_bytes(r.array()?),
            }),
            STATS => {
                if r.input.len() < ServerStats::SIZE {
                    return Err(DecodeError::Truncated);
                }
                let mut u32s = [0u32; 8];
                for v in &mut u32s {
                    *v = u32::from_le_bytes(r.array()?);
                }
                let [
                    tick_p50_us,
                    tick_p99_us,
                    tick_max_us,
                    tick_interval_us,
                    overruns,
                    players,
                    cpu_millicores,
                    tick_thread_millicores,
                ] = u32s;
                Ok(Self::Stats(ServerStats {
                    tick_p50_us,
                    tick_p99_us,
                    tick_max_us,
                    tick_interval_us,
                    overruns,
                    players,
                    cpu_millicores,
                    tick_thread_millicores,
                    sent_bytes_per_sec: u64::from_le_bytes(r.array()?),
                    received_bytes_per_sec: u64::from_le_bytes(r.array()?),
                }))
            }
            other => Err(DecodeError::UnknownEvent(other)),
        }?;
        r.finish()?;
        Ok(event)
    }
}

/// An event from a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientEvent {
    /// Ask for [`ServerEvent::Stats`]. A server started without load-test reporting ignores it.
    RequestStats,
}

impl ClientEvent {
    /// Appends the payload to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::RequestStats => out.push(REQUEST_STATS),
        }
    }

    /// Reads a whole payload. Bytes after the event are an error.
    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader { input };
        let event = match r.array::<1>()? {
            [REQUEST_STATS] => Self::RequestStats,
            [other] => return Err(DecodeError::UnknownEvent(other)),
        };
        r.finish()?;
        Ok(event)
    }
}
