//! The messages, exactly as NET-4 chose to send them: these types encoded with postcard, values quantized as
//! `sawgrass-protocol` quantizes them (positions in centimetres, velocities in cm/s, rotations as 32 smallest-three
//! bits).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub id: u32,
    pub pos: [i32; 3],
    pub vel: [i16; 3],
    pub rot: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u16,
    pub tick: u32,
    /// Sorted by id.
    pub entities: Vec<Entity>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientState {
    pub seq: u16,
    pub tick: u32,
    pub me: Entity,
    pub buttons: u32,
    pub aim: [i16; 2],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chat {
    pub seq: u16,
    pub tick: u32,
    pub from: u32,
    pub channel: u8,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptEvent {
    pub seq: u16,
    pub tick: u32,
    pub name: String,
    pub args: Vec<Arg>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Arg {
    Bool(bool),
    Int(i64),
    Float(f32),
    Str(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Packet {
    Snapshot(Snapshot),
    ClientState(ClientState),
    Chat(Chat),
    ScriptEvent(ScriptEvent),
}

/// A snapshot packet that borrows its snapshot, so encoding needs no copy. It encodes exactly as
/// `Packet::Snapshot` (the same variant index), so `decode` reads it back.
#[derive(Serialize)]
pub enum SnapshotRef<'a> {
    Snapshot(&'a Snapshot),
}

/// Encodes a snapshot with postcard.
pub fn encode_snapshot(s: &Snapshot, out: &mut Vec<u8>) {
    out.clear();
    *out = postcard::to_extend(&SnapshotRef::Snapshot(s), std::mem::take(out)).expect("encode");
}

/// Encodes with postcard.
pub fn encode(p: &Packet, out: &mut Vec<u8>) {
    out.clear();
    *out = postcard::to_extend(p, std::mem::take(out)).expect("encode");
}

/// Decodes with postcard and, as the NET-4 review asked, rejects bytes left over after the packet, so every
/// packet has one encoding.
pub fn decode(bytes: &[u8]) -> Option<Packet> {
    match postcard::take_from_bytes::<Packet>(bytes) {
        Ok((p, [])) => Some(p),
        _ => None,
    }
}
