//! Snapshots recorded from the real server under the load-test bots (`sawgrass-bots run --record FILE`), so the
//! suites can run on that stream instead of the simulated world.
//!
//! A recording holds, for each of the first few bots, every snapshot datagram it received, header included (see
//! `crates/sawgrass-bots/src/record.rs`). Datagrams of one bot with the same tick are one snapshot that the server
//! split; they are joined here and sorted by id, as the model's snapshots are.

use std::collections::BTreeMap;

use sawgrass_protocol::game::decode_snapshot;
use sawgrass_protocol::{PacketHeader, PacketKind};

use crate::model::{Entity, Snapshot};

/// Every recorded bot's snapshots, in the order they arrived.
pub type Streams = Vec<Vec<Snapshot>>;

/// Snapshots skipped at the start of each bot's stream (10 s at 20 Hz), while the other bots still connect.
pub const SKIP: usize = 200;

/// Reads a recording. Datagrams that do not decode as snapshots are skipped and counted in the second value.
pub fn load(path: &str) -> std::io::Result<(Streams, usize)> {
    let bytes = std::fs::read(path)?;
    let mut bots: BTreeMap<u16, Vec<Snapshot>> = BTreeMap::new();
    let (mut rest, mut bad) = (&bytes[..], 0);
    while rest.len() >= 4 {
        let bot = u16::from_le_bytes([rest[0], rest[1]]);
        let len = usize::from(u16::from_le_bytes([rest[2], rest[3]]));
        if rest.len() < 4 + len {
            bad += 1;
            break;
        }
        let datagram = &rest[4..4 + len];
        rest = &rest[4 + len..];
        let Ok((header, payload)) = PacketHeader::decode(datagram) else {
            bad += 1;
            continue;
        };
        if header.kind != PacketKind::Snapshot {
            continue;
        }
        let mut entities = Vec::new();
        let decoded = decode_snapshot(payload, |s| {
            entities.push(Entity {
                id: s.id,
                pos: s.position.cm(),
                vel: s.velocity.cm_per_s,
                rot: s.rotation.bits,
            });
        });
        if decoded.is_err() {
            bad += 1;
            continue;
        }
        let stream = bots.entry(bot).or_default();
        match stream.last_mut() {
            Some(last) if last.tick == header.tick => last.entities.extend(entities),
            _ => stream.push(Snapshot {
                seq: header.sequence,
                tick: header.tick,
                entities,
            }),
        }
    }
    let streams = bots
        .into_values()
        .map(|mut stream| {
            for s in &mut stream {
                s.entities.sort_unstable_by_key(|e| e.id);
            }
            stream.into_iter().skip(SKIP).collect()
        })
        .collect();
    Ok((streams, bad))
}
