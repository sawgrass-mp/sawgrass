//! Every format behind one interface. Each keeps its buffers between calls, as a gateway would.

use rkyv::rancor;
use rkyv::util::AlignedVec;

use crate::checksum;
use crate::handwritten;
use crate::model::*;

pub trait Format {
    fn name(&self) -> &'static str;
    /// Encodes into the format's own buffer and returns the bytes.
    fn encode(&mut self, p: &Packet) -> &[u8];
    /// Decodes untrusted bytes into an owned packet, with every check the format offers.
    fn decode(&mut self, bytes: &[u8]) -> Option<Packet>;
    /// Reads every field without building an owned packet, where the format can (zero-copy); otherwise decodes.
    fn read(&mut self, bytes: &[u8]) -> Option<u64> {
        self.decode(bytes).map(|p| checksum(&p))
    }
    fn zero_copy(&self) -> bool {
        false
    }
}

pub fn all() -> Vec<Box<dyn Format>> {
    vec![
        Box::new(Handwritten(Vec::new())),
        Box::new(Bitcode(bitcode::Buffer::new())),
        Box::new(Postcard(vec![0; 16384])),
        Box::new(Rkyv {
            out: AlignedVec::new(),
            input: AlignedVec::new(),
        }),
        Box::new(crate::flat::Flat::default()),
    ]
}

struct Handwritten(Vec<u8>);

impl Format for Handwritten {
    fn name(&self) -> &'static str {
        "current"
    }
    fn encode(&mut self, p: &Packet) -> &[u8] {
        handwritten::encode(p, &mut self.0);
        &self.0
    }
    fn decode(&mut self, bytes: &[u8]) -> Option<Packet> {
        handwritten::decode(bytes).ok()
    }
}

struct Bitcode(bitcode::Buffer);

impl Format for Bitcode {
    fn name(&self) -> &'static str {
        "bitcode"
    }
    fn encode(&mut self, p: &Packet) -> &[u8] {
        self.0.encode(p)
    }
    fn decode(&mut self, bytes: &[u8]) -> Option<Packet> {
        self.0.decode(bytes).ok()
    }
}

struct Postcard(Vec<u8>);

impl Format for Postcard {
    fn name(&self) -> &'static str {
        "postcard"
    }
    fn encode(&mut self, p: &Packet) -> &[u8] {
        postcard::to_slice(p, &mut self.0).expect("fits")
    }
    fn decode(&mut self, bytes: &[u8]) -> Option<Packet> {
        postcard::from_bytes(bytes).ok()
    }
}

struct Rkyv {
    out: AlignedVec,
    /// Bytes from the network are not aligned; rkyv needs them copied into an aligned buffer first.
    input: AlignedVec,
}

impl Rkyv {
    fn align(&mut self, bytes: &[u8]) {
        self.input.clear();
        self.input.extend_from_slice(bytes);
    }
}

fn archived_entity(e: &ArchivedEntity) -> u64 {
    u64::from(e.id.to_native())
        .wrapping_add(
            e.pos
                .iter()
                .map(|v| v.to_native() as u64)
                .fold(0, u64::wrapping_add),
        )
        .wrapping_add(
            e.vel
                .iter()
                .map(|v| v.to_native() as u64)
                .fold(0, u64::wrapping_add),
        )
        .wrapping_add(u64::from(e.rot.to_native()))
}

impl Format for Rkyv {
    fn name(&self) -> &'static str {
        "rkyv"
    }
    fn encode(&mut self, p: &Packet) -> &[u8] {
        let mut buf = std::mem::take(&mut self.out);
        buf.clear();
        self.out = rkyv::api::high::to_bytes_in::<_, rancor::Error>(p, buf).expect("encode");
        &self.out
    }
    fn decode(&mut self, bytes: &[u8]) -> Option<Packet> {
        self.align(bytes);
        rkyv::from_bytes::<Packet, rancor::Error>(&self.input).ok()
    }
    fn read(&mut self, bytes: &[u8]) -> Option<u64> {
        self.align(bytes);
        // Validated access: bytecheck checks every offset and length before anything is read.
        let p = rkyv::access::<ArchivedPacket, rancor::Error>(&self.input).ok()?;
        Some(match p {
            ArchivedPacket::Snapshot(s) => {
                u64::from(s.seq.to_native())
                    + u64::from(s.tick.to_native())
                    + s.entities.iter().map(archived_entity).fold(0, u64::wrapping_add)
            }
            ArchivedPacket::ClientState(c) => {
                u64::from(c.seq.to_native())
                    + u64::from(c.tick.to_native())
                    + archived_entity(&c.me)
                    + u64::from(c.buttons.to_native())
                    + c.aim[0].to_native() as u64
                    + c.aim[1].to_native() as u64
            }
            ArchivedPacket::Chat(c) => {
                u64::from(c.seq.to_native())
                    + u64::from(c.tick.to_native())
                    + u64::from(c.from.to_native())
                    + u64::from(c.channel)
                    + c.text.len() as u64
            }
            ArchivedPacket::ScriptEvent(s) => {
                u64::from(s.seq.to_native())
                    + u64::from(s.tick.to_native())
                    + s.name.len() as u64
                    + s.args
                        .iter()
                        .map(|a| match a {
                            ArchivedArg::Bool(b) => u64::from(*b),
                            ArchivedArg::Int(i) => i.to_native() as u64,
                            ArchivedArg::Float(f) => u64::from(f.to_native().to_bits()),
                            ArchivedArg::Str(s) => s.len() as u64,
                        })
                        .fold(0, u64::wrapping_add)
            }
        })
    }
    fn zero_copy(&self) -> bool {
        true
    }
}
