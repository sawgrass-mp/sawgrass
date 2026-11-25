//! Delta snapshot datagrams in, whole snapshots out.
//!
//! The server sends each tick's snapshot as one or more [`PacketKind::DeltaSnapshot`] fragments, encoded against
//! a tick the client acknowledged (`sawgrass_protocol::delta`). The receiver reassembles the newest tick, decodes
//! it against the baseline it names from the snapshots kept here, and remembers the newest tick decoded: the
//! value the client puts in the tick field of its client states' headers. Each entity's [`Info`] (kind, model and
//! block fields, ADR 0006) is decoded and kept with the baselines too, since a delta carries only what changed.

use sawgrass_protocol::delta::{Baseline, Reassembler, baseline_age, decode_delta_with_info};
use sawgrass_protocol::entity::Info;
use sawgrass_protocol::{DecodeError, EntityState, PacketHeader, PacketKind};

/// Decoded snapshots kept as baselines: as many ticks as the server keeps history for.
pub const KEPT_TICKS: usize = 16;

/// Why a datagram gave no snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// Not a delta snapshot datagram.
    NotSnapshot,
    /// More fragments of its tick are missing, or it belongs to an older tick than one already started.
    Incomplete,
    /// Encoded against a baseline this client no longer has; a later snapshot against a newer
    /// acknowledgement replaces it.
    MissingBaseline,
    /// Bytes that do not decode.
    Invalid(DecodeError),
}

/// One decoded snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot<'a> {
    /// The server tick it describes.
    pub tick: u32,
    /// Every visible entity's state, ascending by id.
    pub states: &'a [EntityState],
    /// `infos[i]` is the info of `states[i]`.
    pub infos: &'a [Info],
}

/// A decoded snapshot kept as a baseline.
#[derive(Debug, Clone, Default)]
struct Kept {
    tick: Option<u32>,
    states: Vec<EntityState>,
    infos: Vec<Info>,
}

/// Reassembles and decodes snapshots, and keeps the baselines and the acknowledgement.
#[derive(Debug)]
pub struct SnapshotReceiver {
    reassembler: Reassembler,
    kept: Vec<Kept>,
    decoded: Kept,
    ack: u32,
}

impl Default for SnapshotReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotReceiver {
    /// A receiver that has decoded nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            reassembler: Reassembler::new(),
            kept: vec![Kept::default(); KEPT_TICKS],
            decoded: Kept::default(),
            ack: 0,
        }
    }

    /// The newest tick decoded, 0 for none: what the client acknowledges in its states' headers.
    #[must_use]
    pub fn ack(&self) -> u32 {
        self.ack
    }

    /// Takes one whole datagram, header included. Returns the snapshot once the datagram completes one that
    /// decodes.
    pub fn push(&mut self, datagram: &[u8]) -> Result<Snapshot<'_>, Skipped> {
        let (header, body) = PacketHeader::decode(datagram).map_err(Skipped::Invalid)?;
        if header.kind != PacketKind::DeltaSnapshot {
            return Err(Skipped::NotSnapshot);
        }
        let tick = header.tick;
        let payload = self
            .reassembler
            .push(tick, body)
            .map_err(Skipped::Invalid)?
            .ok_or(Skipped::Incomplete)?;
        let (base, base_infos) = match baseline_age(payload).map_err(Skipped::Invalid)? {
            0 => (None, &[][..]),
            age => {
                let t = tick.checked_sub(age).ok_or(Skipped::MissingBaseline)?;
                let kept = &self.kept[t as usize % KEPT_TICKS];
                if kept.tick != Some(t) {
                    return Err(Skipped::MissingBaseline);
                }
                let base = Baseline {
                    tick: t,
                    entities: &kept.states,
                };
                (Some(base), &kept.infos[..])
            }
        };
        let decoded = &mut self.decoded;
        decode_delta_with_info(
            payload,
            tick,
            base,
            base_infos,
            &mut decoded.states,
            &mut decoded.infos,
        )
        .map_err(Skipped::Invalid)?;
        decoded.tick = Some(tick);
        let slot = &mut self.kept[tick as usize % KEPT_TICKS];
        std::mem::swap(slot, &mut self.decoded);
        self.ack = self.ack.max(tick);
        Ok(Snapshot {
            tick,
            states: &slot.states,
            infos: &slot.infos,
        })
    }
}

#[cfg(test)]
mod tests {
    use sawgrass_protocol::delta::{DeltaEncoder, fragments};
    use sawgrass_protocol::entity::{AssetId, EntityKind};
    use sawgrass_protocol::{Position, Rotation, Velocity};

    use super::*;

    fn state(id: u32, x: f32) -> EntityState {
        EntityState {
            id,
            position: Position::from_metres([x, 0.0, 0.0]).expect("position"),
            velocity: Velocity::from_metres_per_second([1.0, 0.0, 0.0]).expect("velocity"),
            rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
        }
    }

    /// The datagrams the server would send for `tick` against `base`.
    fn datagrams(tick: u32, states: &[EntityState], base: Option<Baseline<'_>>) -> Vec<Vec<u8>> {
        datagrams_with_info(tick, states, &[], base, &[])
    }

    fn datagrams_with_info(
        tick: u32,
        states: &[EntityState],
        infos: &[Info],
        base: Option<Baseline<'_>>,
        base_infos: &[Info],
    ) -> Vec<Vec<u8>> {
        let mut payload = Vec::new();
        DeltaEncoder::default()
            .encode_with_info(tick, states, infos, base, base_infos, &mut payload)
            .expect("encode");
        fragments(&payload)
            .expect("fragments")
            .map(|(fragment, part)| {
                let mut out = vec![0; PacketHeader::SIZE];
                PacketHeader {
                    kind: PacketKind::DeltaSnapshot,
                    sequence: 0,
                    tick,
                }
                .encode(&mut out)
                .expect("header");
                out.push(fragment);
                out.extend_from_slice(part);
                out
            })
            .collect()
    }

    #[test]
    fn decodes_against_kept_baselines_and_acknowledges_the_newest() {
        let mut rx = SnapshotReceiver::new();
        let first = [state(1, 0.0), state(2, 5.0)];
        for d in datagrams(10, &first, None) {
            let snapshot = rx.push(&d).expect("snapshot");
            assert_eq!(snapshot.tick, 10);
            assert_eq!(snapshot.states, &first);
            assert_eq!(snapshot.infos, &[Info::PLAYER; 2]);
        }
        assert_eq!(rx.ack(), 10);

        let second = [state(1, 0.5), state(3, 9.0)];
        let base = Baseline {
            tick: 10,
            entities: &first,
        };
        for d in datagrams(12, &second, Some(base)) {
            let snapshot = rx.push(&d).expect("snapshot");
            assert_eq!(snapshot.tick, 12);
            assert_eq!(snapshot.states, &second);
        }
        assert_eq!(rx.ack(), 12);
    }

    #[test]
    fn keeps_infos_across_deltas() {
        let mut rx = SnapshotReceiver::new();
        let car = Info::with_fields(EntityKind::Vehicle, AssetId(77), &[3, 0, 9]).expect("info");
        let ped = Info::new(EntityKind::Npc, AssetId(5));
        let states = [state(1, 0.0), state(2, 5.0)];
        let infos = [car, ped];
        for d in datagrams_with_info(10, &states, &infos, None, &[]) {
            assert_eq!(rx.push(&d).expect("snapshot").infos, &infos);
        }
        // Only one field of the car changes; the delta carries just that, and the receiver rebuilds the rest.
        let car2 = Info::with_fields(EntityKind::Vehicle, AssetId(77), &[3, 1, 9]).expect("info");
        let moved = [state(1, 0.5), state(2, 5.0)];
        let base = Baseline {
            tick: 10,
            entities: &states,
        };
        for d in datagrams_with_info(11, &moved, &[car2, ped], Some(base), &infos) {
            let snapshot = rx.push(&d).expect("snapshot");
            assert_eq!(snapshot.states, &moved);
            assert_eq!(snapshot.infos, &[car2, ped]);
        }
    }

    #[test]
    fn waits_for_a_baseline_it_has() {
        let mut rx = SnapshotReceiver::new();
        let old = [state(1, 0.0)];
        let base = Baseline {
            tick: 3,
            entities: &old,
        };
        let d = datagrams(5, &[state(1, 1.0)], Some(base));
        assert_eq!(rx.push(&d[0]), Err(Skipped::MissingBaseline));
        assert_eq!(rx.ack(), 0);
    }

    #[test]
    fn ignores_other_packets_and_rejects_garbage() {
        let mut rx = SnapshotReceiver::new();
        let mut event = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::Event,
            sequence: 0,
            tick: 1,
        }
        .encode(&mut event)
        .expect("header");
        assert_eq!(rx.push(&event), Err(Skipped::NotSnapshot));
        assert!(matches!(rx.push(&[4, 0]), Err(Skipped::Invalid(_))));
    }
}
