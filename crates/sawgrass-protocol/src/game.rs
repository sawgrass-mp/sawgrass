//! Payloads that follow the [`PacketHeader`](crate::PacketHeader) of each packet kind.
//!
//! | Kind | Payload |
//! |---|---|
//! | [`ClientState`](crate::PacketKind::ClientState) | [`ClientState`]: position, velocity, rotation, 19 bytes; then, optionally, the player's [`PedBlock`]: field count and fields as varints; then, optionally, the [`OwnedState`]s: a count, then per entity its id, epoch byte, 19 bytes of kinematics, field count and fields |
//! | [`Snapshot`](crate::PacketKind::Snapshot) | count (`u8`), then that many [`EntityState`]s |
//! | [`DeltaSnapshot`](crate::PacketKind::DeltaSnapshot) | a fragment byte and part of a delta payload ([`crate::delta`]) |
//! | [`Event`](crate::PacketKind::Event) | [`ServerEvent`] or [`ClientEvent`]: a code byte, then its fields |
//!
//! The server sends delta snapshots since protocol version 3; the whole [`Snapshot`](crate::PacketKind::Snapshot)
//! format stays for tools and tests that read older recordings. A whole snapshot that holds more than
//! [`SNAPSHOT_MAX_ENTITIES`] entities is split over several datagrams with the same tick, so each one fits in
//! [`MAX_DATAGRAM`] bytes on any path quinn accepts.

use crate::delta::{put_varint, take_varint};
use crate::entity::MAX_FIELDS;
use crate::ped::PedBlock;
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

    /// Appends the state and then the player's ped block to `out`.
    pub fn encode_with_ped(&self, ped: &PedBlock, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        let mut buf = [0; Self::SIZE];
        self.encode(&mut buf)?;
        out.extend_from_slice(&buf);
        put_fields(&ped.to_fields(), out);
        Ok(())
    }

    /// Appends the state, the player's ped block and the states of the entities the client simulates to `out`.
    /// Fails with [`EncodeError::OutOfRange`] for more than [`MAX_OWNED`] entities and with
    /// [`EncodeError::BufferTooSmall`] if the payload would not fit one datagram after its header; `out` is then
    /// as it was.
    pub fn encode_with_owned(
        &self,
        ped: &PedBlock,
        owned: &[OwnedState],
        out: &mut Vec<u8>,
    ) -> Result<(), EncodeError> {
        if owned.len() > MAX_OWNED {
            return Err(EncodeError::OutOfRange);
        }
        let start = out.len();
        self.encode_with_ped(ped, out)?;
        put_varint(out, owned.len() as u64);
        let mut buf = [0; Self::SIZE];
        for o in owned {
            put_varint(out, u64::from(o.id));
            out.push(o.epoch);
            o.state.encode(&mut buf)?;
            out.extend_from_slice(&buf);
            put_fields(o.fields(), out);
        }
        if out.len() - start > MAX_DATAGRAM - PacketHeader::SIZE {
            out.truncate(start);
            return Err(EncodeError::BufferTooSmall);
        }
        Ok(())
    }

    /// Reads a whole payload, with or without a ped block and owned entities, and returns the state. Anything
    /// after the state but a valid ped block and owned list is an error: one encoding per packet (NET-4).
    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        Self::decode_with_ped(input).map(|(state, _)| state)
    }

    /// Reads a whole payload: the state, and the ped block if one follows. Owned entities after it are checked
    /// and dropped. A block with more than [`MAX_FIELDS`] fields, or fields [`PedBlock::from_fields`] refuses, is
    /// an error.
    pub fn decode_with_ped(input: &[u8]) -> Result<(Self, Option<PedBlock>), DecodeError> {
        Self::decode_parts(input, None)
    }

    /// As [`ClientState::decode_with_ped`], and fills `owned` (cleared first) with the owned entities' states.
    pub fn decode_with_owned(
        input: &[u8],
        owned: &mut Vec<OwnedState>,
    ) -> Result<(Self, Option<PedBlock>), DecodeError> {
        Self::decode_parts(input, Some(owned))
    }

    fn decode_parts(
        input: &[u8],
        mut owned: Option<&mut Vec<OwnedState>>,
    ) -> Result<(Self, Option<PedBlock>), DecodeError> {
        if let Some(owned) = owned.as_deref_mut() {
            owned.clear();
        }
        let mut r = Reader { input };
        let state = Self::read(&mut r)?;
        let mut rest = r.input;
        if rest.is_empty() {
            return Ok((state, None));
        }
        let (fields, n) = take_fields(&mut rest)?;
        let ped = PedBlock::from_fields(&fields[..n])?;
        if !rest.is_empty() {
            let count = usize::try_from(take_varint(&mut rest)?).map_err(|_| DecodeError::OutOfRange)?;
            if count > MAX_OWNED {
                return Err(DecodeError::OutOfRange);
            }
            for _ in 0..count {
                let id = u32::try_from(take_varint(&mut rest)?).map_err(|_| DecodeError::OutOfRange)?;
                let mut r = Reader { input: rest };
                let [epoch] = r.array::<1>()?;
                let kinematics = Self::read(&mut r)?;
                rest = r.input;
                let (fields, n) = take_fields(&mut rest)?;
                if let Some(owned) = owned.as_deref_mut() {
                    owned.push(OwnedState {
                        id,
                        epoch,
                        state: kinematics,
                        len: n as u8,
                        fields,
                    });
                }
            }
        }
        Reader { input: rest }.finish()?;
        Ok((state, Some(ped)))
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            position: Position::read(r)?,
            velocity: Velocity::read(r)?,
            rotation: Rotation::read(r)?,
        })
    }
}

/// Most owned entities in one client state.
pub const MAX_OWNED: usize = 16;

/// The state of an entity a client simulates, sent after its own (ADR 0006, section 4): the entity's id, the
/// ownership epoch the server gave the client for it, its kinematics and its block fields (a [`VehicleBlock`] for a
/// vehicle, a [`PedBlock`] for an NPC).
///
/// [`VehicleBlock`]: crate::vehicle::VehicleBlock
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OwnedState {
    /// The entity's id.
    pub id: u32,
    /// The epoch of the client's ownership; a state with another epoch is dropped by the server.
    pub epoch: u8,
    /// Its position, velocity and rotation.
    pub state: ClientState,
    len: u8,
    fields: [u32; MAX_FIELDS],
}

impl OwnedState {
    /// A state with these block fields. Fails with [`EncodeError::OutOfRange`] for more than [`MAX_FIELDS`].
    pub fn new(id: u32, epoch: u8, state: ClientState, fields: &[u32]) -> Result<Self, EncodeError> {
        let mut all = [0; MAX_FIELDS];
        all.get_mut(..fields.len())
            .ok_or(EncodeError::OutOfRange)?
            .copy_from_slice(fields);
        Ok(Self {
            id,
            epoch,
            state,
            len: fields.len() as u8,
            fields: all,
        })
    }

    /// The block fields.
    #[must_use]
    pub fn fields(&self) -> &[u32] {
        &self.fields[..usize::from(self.len)]
    }
}

fn put_fields(fields: &[u32], out: &mut Vec<u8>) {
    put_varint(out, fields.len() as u64);
    for &v in fields {
        put_varint(out, u64::from(v));
    }
}

/// Reads a field count, at most [`MAX_FIELDS`], and that many fields.
fn take_fields(rest: &mut &[u8]) -> Result<([u32; MAX_FIELDS], usize), DecodeError> {
    let n = usize::try_from(take_varint(rest)?).map_err(|_| DecodeError::OutOfRange)?;
    if n > MAX_FIELDS {
        return Err(DecodeError::OutOfRange);
    }
    let mut fields = [0u32; MAX_FIELDS];
    for v in &mut fields[..n] {
        *v = u32::try_from(take_varint(rest)?).map_err(|_| DecodeError::OutOfRange)?;
    }
    Ok((fields, n))
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
const PAID: u8 = 3;
const OWNERSHIP: u8 = 4;
const CORRECTED: u8 = 5;
const REQUEST_STATS: u8 = 128;
const PAY: u8 = 129;

/// How the server answered a [`ClientEvent::Pay`]. The values are part of the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PayResult {
    /// The money moved, and is durable.
    Done = 0,
    /// The payer's balance is below the amount.
    InsufficientFunds = 1,
    /// The payee is not a player with a wallet.
    UnknownPayee = 2,
    /// A zero amount, a payment to oneself, or a key reused with other parameters.
    Invalid = 3,
    /// The server could not store it; nothing moved.
    Failed = 4,
    /// The server takes no payments from clients.
    Refused = 5,
}

impl PayResult {
    fn from_code(code: u8) -> Result<Self, DecodeError> {
        Ok(match code {
            0 => Self::Done,
            1 => Self::InsufficientFunds,
            2 => Self::UnknownPayee,
            3 => Self::Invalid,
            4 => Self::Failed,
            5 => Self::Refused,
            _ => return Err(DecodeError::OutOfRange),
        })
    }
}

/// An event from the server.
///
/// New events are added in later protocol versions, so readers outside this crate need a catch-all arm and
/// ignore what they do not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
    /// The answer to the [`ClientEvent::Pay`] with this `key`, and the payer's balance after it.
    Paid {
        /// The client's key for the payment.
        key: u64,
        /// What happened.
        result: PayResult,
        /// The payer's balance after the payment, or as it stands if nothing moved.
        balance: i64,
    },
    /// The client now simulates `entity` (`yours`), or no longer does (ADR 0006, section 4). Its
    /// [`OwnedState`]s for the entity must carry `epoch`; the server drops any other.
    Ownership {
        /// The entity's id.
        entity: u32,
        /// The entity's epoch from now on.
        epoch: u8,
        /// Whether this client is its owner now.
        yours: bool,
    },
    /// The server refused a state the client sent for this entity (its own player, or an entity it simulates)
    /// and holds it here: the last state it accepted. The client moves the entity back. Sent only to the
    /// entity's current owner, at most every few hundred milliseconds per entity.
    Corrected(EntityState),
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
            Self::Paid { key, result, balance } => {
                out.push(PAID);
                out.extend_from_slice(&key.to_le_bytes());
                out.push(*result as u8);
                out.extend_from_slice(&balance.to_le_bytes());
            }
            Self::Ownership { entity, epoch, yours } => {
                out.push(OWNERSHIP);
                out.extend_from_slice(&entity.to_le_bytes());
                out.push(*epoch);
                out.push(u8::from(*yours));
            }
            Self::Corrected(state) => {
                out.push(CORRECTED);
                let mut buf = [0; EntityState::SIZE];
                // The buffer is exactly one state long, so encoding cannot fail.
                let _ = state.encode(&mut buf);
                out.extend_from_slice(&buf);
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
            PAID => Ok(Self::Paid {
                key: u64::from_le_bytes(r.array()?),
                result: PayResult::from_code(r.array::<1>()?[0])?,
                balance: i64::from_le_bytes(r.array()?),
            }),
            OWNERSHIP => {
                let entity = u32::from_le_bytes(r.array()?);
                let [epoch, yours] = r.array::<2>()?;
                if yours > 1 {
                    return Err(DecodeError::OutOfRange);
                }
                Ok(Self::Ownership {
                    entity,
                    epoch,
                    yours: yours == 1,
                })
            }
            CORRECTED => {
                let (state, rest) = EntityState::decode(r.input)?;
                r.input = rest;
                Ok(Self::Corrected(state))
            }
            other => Err(DecodeError::UnknownEvent(other)),
        }?;
        r.finish()?;
        Ok(event)
    }
}

/// An event from a client.
///
/// New events are added in later protocol versions, so readers outside this crate need a catch-all arm and
/// ignore what they do not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClientEvent {
    /// Ask for [`ServerEvent::Stats`]. A server started without load-test reporting ignores it.
    RequestStats,
    /// Pay `amount` from the player's wallet to the player with entity id `to`; answered by [`ServerEvent::Paid`]
    /// with the same `key`. Sending the same key again is answered without paying twice. Only servers started
    /// for money load tests take it (task DB-8); others answer [`PayResult::Refused`].
    Pay {
        /// The client's key for this payment, unique per payment.
        key: u64,
        /// The payee's entity id.
        to: u32,
        /// The amount, in the smallest unit of money.
        amount: u32,
    },
}

impl ClientEvent {
    /// Appends the payload to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::RequestStats => out.push(REQUEST_STATS),
            Self::Pay { key, to, amount } => {
                out.push(PAY);
                out.extend_from_slice(&key.to_le_bytes());
                out.extend_from_slice(&to.to_le_bytes());
                out.extend_from_slice(&amount.to_le_bytes());
            }
        }
    }

    /// Reads a whole payload. Bytes after the event are an error.
    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader { input };
        let event = match r.array::<1>()? {
            [REQUEST_STATS] => Self::RequestStats,
            [PAY] => Self::Pay {
                key: u64::from_le_bytes(r.array()?),
                to: u32::from_le_bytes(r.array()?),
                amount: u32::from_le_bytes(r.array()?),
            },
            [other] => return Err(DecodeError::UnknownEvent(other)),
        };
        r.finish()?;
        Ok(event)
    }
}
