//! Wire format shared by the Sawgrass server, gateways, clients and bots.
//!
//! Every packet starts with a [`PacketHeader`]. Entity state uses the quantized encodings in [`quant`]: positions
//! to 1 cm, velocities to 1 cm/s and rotations in 32 bits; the table in that module lists the wire size, range and
//! worst-case error of each. [`game`] defines what follows the header for each packet kind, and [`delta`] the delta-encoded snapshots. All integers are
//! little-endian. The connection handshake lives in the gateway crate.

pub mod delta;
pub mod entity;
pub mod game;
pub mod ped;
pub mod quant;
pub mod vehicle;

use std::fmt;

pub use quant::{Position, Rotation, Velocity};

/// Bumped on every change that an older peer cannot read. CI compares it with the base branch when the
/// protocol crate changes (task OPS-3).
///
/// Version 1 is the first wire format: the packet header and the quantized encodings. Version 2 adds the
/// payloads in [`game`]. Version 3 sends snapshots delta-encoded ([`delta`], [`PacketKind::DeltaSnapshot`]) and
/// makes the tick in a client state's header the newest snapshot tick the client decoded. Version 4 adds each
/// entity's kind, model and block fields to delta snapshots ([`entity`], ADR 0006). Version 5 lets a client state
/// carry the player's ped block ([`ped`]). Version 6 adds the vehicle block ([`vehicle`]) and the states of the
/// entities a client simulates after its own ([`game::OwnedState`]). Version 7 adds the ownership event
/// ([`game::ServerEvent::Ownership`]).
pub const PROTOCOL_VERSION: u32 = 7;

/// Whether a peer speaking `theirs` can talk to one speaking [`PROTOCOL_VERSION`].
///
/// Until the first release only identical versions are compatible.
#[must_use]
pub const fn is_compatible(theirs: u32) -> bool {
    theirs == PROTOCOL_VERSION
}

/// A value that cannot be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The value is NaN or infinite.
    NotFinite,
    /// The value lies outside the range of its encoding.
    OutOfRange,
    /// The output buffer is too small.
    BufferTooSmall,
}

/// Bytes that are not a valid encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ends before the value does.
    Truncated,
    /// The packet kind byte is not a known [`PacketKind`].
    UnknownKind(u8),
    /// The event code byte is not a known event.
    UnknownEvent(u8),
    /// A whole payload is followed by more bytes.
    TrailingBytes,
    /// A count or value lies outside what the format allows.
    OutOfRange,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotFinite => "value is not finite",
            Self::OutOfRange => "value is out of range",
            Self::BufferTooSmall => "output buffer is too small",
        })
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("input is truncated"),
            Self::UnknownKind(kind) => write!(f, "unknown packet kind {kind}"),
            Self::UnknownEvent(code) => write!(f, "unknown event code {code}"),
            Self::TrailingBytes => f.write_str("bytes after the end of the payload"),
            Self::OutOfRange => f.write_str("value is out of range"),
        }
    }
}

impl std::error::Error for EncodeError {}
impl std::error::Error for DecodeError {}

/// What a packet carries. The values are part of the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PacketKind {
    /// Entity states from the server to a client.
    Snapshot = 1,
    /// A client's own state (and that of entities it owns) to the server.
    ClientState = 2,
    /// A reliable event in either direction.
    Event = 3,
    /// One fragment of a delta-encoded snapshot from the server ([`delta`]).
    DeltaSnapshot = 4,
}

impl TryFrom<u8> for PacketKind {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, DecodeError> {
        match value {
            1 => Ok(Self::Snapshot),
            2 => Ok(Self::ClientState),
            3 => Ok(Self::Event),
            4 => Ok(Self::DeltaSnapshot),
            other => Err(DecodeError::UnknownKind(other)),
        }
    }
}

/// The 7 bytes at the start of every packet: kind (`u8`), sequence (`u16`) and server tick (`u32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PacketHeader {
    /// What the payload is.
    pub kind: PacketKind,
    /// Per-connection packet counter; wraps around.
    pub sequence: u16,
    /// The server tick the payload describes (or, from a client, the last tick it received).
    pub tick: u32,
}

impl PacketHeader {
    /// Wire size in bytes.
    pub const SIZE: usize = 7;

    /// Writes the header to the start of `out` and returns the number of bytes written.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let mut w = Writer::new(out, Self::SIZE)?;
        w.bytes(&[self.kind as u8]);
        w.bytes(&self.sequence.to_le_bytes());
        w.bytes(&self.tick.to_le_bytes());
        Ok(w.pos)
    }

    /// Reads a header from the start of `input` and returns it with the rest of the packet.
    pub fn decode(input: &[u8]) -> Result<(Self, &[u8]), DecodeError> {
        let mut r = Reader { input };
        let [kind] = r.array::<1>()?;
        let header = Self {
            kind: PacketKind::try_from(kind)?,
            sequence: u16::from_le_bytes(r.array()?),
            tick: u32::from_le_bytes(r.array()?),
        };
        Ok((header, r.input))
    }
}

/// The state of one entity in a snapshot: 4-byte id, position, velocity and rotation, 23 bytes in all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntityState {
    /// The server-assigned entity id.
    pub id: u32,
    /// Where the entity is.
    pub position: Position,
    /// How fast it moves.
    pub velocity: Velocity,
    /// Which way it faces.
    pub rotation: Rotation,
}

impl EntityState {
    /// Wire size in bytes.
    pub const SIZE: usize = 4 + Position::SIZE + Velocity::SIZE + Rotation::SIZE;

    /// Writes the state to the start of `out` and returns the number of bytes written.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let mut w = Writer::new(out, Self::SIZE)?;
        w.bytes(&self.id.to_le_bytes());
        self.position.write(&mut w);
        self.velocity.write(&mut w);
        self.rotation.write(&mut w);
        Ok(w.pos)
    }

    /// Reads a state from the start of `input` and returns it with the remaining bytes.
    pub fn decode(input: &[u8]) -> Result<(Self, &[u8]), DecodeError> {
        let mut r = Reader { input };
        let state = Self {
            id: u32::from_le_bytes(r.array()?),
            position: Position::read(&mut r)?,
            velocity: Velocity::read(&mut r)?,
            rotation: Rotation::read(&mut r)?,
        };
        Ok((state, r.input))
    }
}

/// Writes into a buffer already checked to hold `len` bytes.
#[derive(Debug)]
pub(crate) struct Writer<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    fn new(out: &'a mut [u8], len: usize) -> Result<Self, EncodeError> {
        if out.len() < len {
            return Err(EncodeError::BufferTooSmall);
        }
        Ok(Self { out, pos: 0 })
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.out[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
    }
}

/// Reads from the front of a byte slice.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    input: &'a [u8],
}

impl Reader<'_> {
    /// Fails if any input is left: a whole payload must end where its encoding does.
    pub(crate) fn finish(&self) -> Result<(), DecodeError> {
        if self.input.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let (head, rest) = self
            .input
            .split_first_chunk::<N>()
            .ok_or(DecodeError::Truncated)?;
        self.input = rest;
        Ok(*head)
    }
}

#[cfg(test)]
mod tests;
