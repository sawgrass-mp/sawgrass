//! The current encoding: `sawgrass-protocol`'s 7-byte `PacketHeader` and 23-byte `EntityState`, used as they are.
//! The rest of each message does not exist in the protocol yet (NET-2 adds it), so it is written here in the same
//! style: fixed-width little-endian integers, `u16` counts, `u8` string lengths, every read checked.

use sawgrass_protocol::{EntityState, PacketHeader, PacketKind, Position, Rotation, Velocity};

use crate::model::*;

const EVENT_CHAT: u8 = 1;
const EVENT_SCRIPT: u8 = 2;

fn put_entity(out: &mut Vec<u8>, e: &Entity) {
    let state = EntityState {
        id: e.id,
        position: Position::from_cm(e.pos).expect("24-bit position"),
        velocity: Velocity { cm_per_s: e.vel },
        rotation: Rotation { bits: e.rot },
    };
    let start = out.len();
    out.resize(start + EntityState::SIZE, 0);
    state.encode(&mut out[start..]).expect("sized");
}

fn put_header(out: &mut Vec<u8>, kind: PacketKind, seq: u16, tick: u32) {
    let start = out.len();
    out.resize(start + PacketHeader::SIZE, 0);
    PacketHeader {
        kind,
        sequence: seq,
        tick,
    }
    .encode(&mut out[start..])
    .expect("sized");
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let len = u8::try_from(s.len()).expect("strings up to 255 bytes");
    out.push(len);
    out.extend_from_slice(s.as_bytes());
}

pub fn encode(p: &Packet, out: &mut Vec<u8>) {
    out.clear();
    match p {
        Packet::Snapshot(s) => {
            put_header(out, PacketKind::Snapshot, s.seq, s.tick);
            out.extend_from_slice(&(s.entities.len() as u16).to_le_bytes());
            for e in &s.entities {
                put_entity(out, e);
            }
        }
        Packet::ClientState(c) => {
            put_header(out, PacketKind::ClientState, c.seq, c.tick);
            put_entity(out, &c.me);
            out.extend_from_slice(&c.buttons.to_le_bytes());
            out.extend_from_slice(&c.aim[0].to_le_bytes());
            out.extend_from_slice(&c.aim[1].to_le_bytes());
        }
        Packet::Chat(c) => {
            put_header(out, PacketKind::Event, c.seq, c.tick);
            out.push(EVENT_CHAT);
            out.extend_from_slice(&c.from.to_le_bytes());
            out.push(c.channel);
            put_str(out, &c.text);
        }
        Packet::ScriptEvent(e) => {
            put_header(out, PacketKind::Event, e.seq, e.tick);
            out.push(EVENT_SCRIPT);
            put_str(out, &e.name);
            out.push(u8::try_from(e.args.len()).expect("up to 255 arguments"));
            for a in &e.args {
                match a {
                    Arg::Bool(b) => out.extend_from_slice(&[0, u8::from(*b)]),
                    Arg::Int(i) => {
                        out.push(1);
                        out.extend_from_slice(&i.to_le_bytes());
                    }
                    Arg::Float(f) => {
                        out.push(2);
                        out.extend_from_slice(&f.to_le_bytes());
                    }
                    Arg::Str(s) => {
                        out.push(3);
                        put_str(out, s);
                    }
                }
            }
        }
    }
}

/// Any decoding failure; the harness only needs to know that it failed.
#[derive(Debug)]
pub struct Bad;

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Bad> {
        if self.0.len() < n {
            return Err(Bad);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, Bad> {
        Ok(self.take(1)?[0])
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], Bad> {
        self.take(N)?.try_into().map_err(|_| Bad)
    }
    fn entity(&mut self) -> Result<Entity, Bad> {
        let (s, rest) = EntityState::decode(self.0).map_err(|_| Bad)?;
        self.0 = rest;
        Ok(Entity {
            id: s.id,
            pos: s.position.cm(),
            vel: s.velocity.cm_per_s,
            rot: s.rotation.bits,
        })
    }
    fn string(&mut self) -> Result<String, Bad> {
        let n = self.u8()? as usize;
        Ok(std::str::from_utf8(self.take(n)?).map_err(|_| Bad)?.to_owned())
    }
}

pub fn decode(bytes: &[u8]) -> Result<Packet, Bad> {
    let (h, rest) = PacketHeader::decode(bytes).map_err(|_| Bad)?;
    let mut r = Reader(rest);
    let (seq, tick) = (h.sequence, h.tick);
    let p = match h.kind {
        PacketKind::Snapshot => {
            let n = u16::from_le_bytes(r.arr()?) as usize;
            // Never trust a count further than the bytes that are there.
            if n * EntityState::SIZE > r.0.len() {
                return Err(Bad);
            }
            let mut entities = Vec::with_capacity(n);
            for _ in 0..n {
                entities.push(r.entity()?);
            }
            Packet::Snapshot(Snapshot { seq, tick, entities })
        }
        PacketKind::ClientState => Packet::ClientState(ClientState {
            seq,
            tick,
            me: r.entity()?,
            buttons: u32::from_le_bytes(r.arr()?),
            aim: [i16::from_le_bytes(r.arr()?), i16::from_le_bytes(r.arr()?)],
        }),
        PacketKind::Event => match r.u8()? {
            EVENT_CHAT => Packet::Chat(Chat {
                seq,
                tick,
                from: u32::from_le_bytes(r.arr()?),
                channel: r.u8()?,
                text: r.string()?,
            }),
            EVENT_SCRIPT => {
                let name = r.string()?;
                let n = r.u8()? as usize;
                let mut args = Vec::with_capacity(n.min(r.0.len()));
                for _ in 0..n {
                    args.push(match r.u8()? {
                        0 => Arg::Bool(match r.u8()? {
                            0 => false,
                            1 => true,
                            _ => return Err(Bad),
                        }),
                        1 => Arg::Int(i64::from_le_bytes(r.arr()?)),
                        2 => Arg::Float(f32::from_le_bytes(r.arr()?)),
                        3 => Arg::Str(r.string()?),
                        _ => return Err(Bad),
                    });
                }
                Packet::ScriptEvent(ScriptEvent {
                    seq,
                    tick,
                    name,
                    args,
                })
            }
            _ => return Err(Bad),
        },
    };
    if !r.0.is_empty() {
        return Err(Bad);
    }
    Ok(p)
}
