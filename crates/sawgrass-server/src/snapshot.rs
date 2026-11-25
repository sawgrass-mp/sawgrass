//! Building one player's snapshot datagrams (tasks SRV-7 and NET-14).
//!
//! Everything a player's snapshot needs that changes, its visible set, sequence number, what it was sent lately
//! and its buffers, lives in its own [`View`]. [`View::build`] reads the shared grid and [`History`] and writes
//! only its own view, so the server builds every player's view in parallel (SRV-4, "players in parallel on hecs",
//! `docs/research/tick-model`) and gets the same datagrams whatever the number of threads.
//!
//! Snapshots are delta-encoded ([`sawgrass_protocol::delta`], NET-5) against the newest snapshot the client says it
//! decoded completely, its acknowledgement. The view remembers which slots it sent in each of the last
//! [`HISTORY_TICKS`] ticks, and [`History`] holds every slot's wire state in those ticks, so the baseline is rebuilt
//! exactly as the client has it. An acknowledgement older than that, or one that names a tick this view did not
//! send, gets a whole snapshot (a delta against nothing).

use bytes::Bytes;
use sawgrass_protocol::delta::{Baseline, DeltaEncoder, MAX_ENTITIES, fragments};
use sawgrass_protocol::{EntityState, PacketHeader, PacketKind};

use crate::interest::{DenseGrid, EntityId, Point, VisibilityChanges, VisibilityConfig, VisibleSet};

/// Ticks of history kept for baselines: 0.8 s at 20 Hz, several round trips.
pub const HISTORY_TICKS: usize = 16;

/// The wire state of every slot in each of the last [`HISTORY_TICKS`] ticks.
#[derive(Debug, Default)]
pub struct History {
    ticks: [Option<u32>; HISTORY_TICKS],
    states: Vec<Vec<Option<EntityState>>>,
}

impl History {
    /// An empty history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the states of `tick`, by slot, replacing the tick [`HISTORY_TICKS`] before it.
    pub fn record(&mut self, tick: u32, states: &[Option<EntityState>]) {
        let i = tick as usize % HISTORY_TICKS;
        self.states.resize_with(HISTORY_TICKS, Vec::new);
        self.states[i].clear();
        self.states[i].extend_from_slice(states);
        self.ticks[i] = Some(tick);
    }

    /// The states of `tick`, by slot, if it is still recorded.
    #[must_use]
    pub fn at(&self, tick: u32) -> Option<&[Option<EntityState>]> {
        let i = tick as usize % HISTORY_TICKS;
        (self.ticks[i] == Some(tick)).then(|| &self.states[i][..])
    }
}

/// One player's visible set, snapshot sequence, recent snapshots and output.
#[derive(Debug, Default)]
pub struct View {
    visible: VisibleSet,
    sequence: u16,
    changes: VisibilityChanges,
    /// The slots sent in each of the last ticks, with the tick.
    sent: Vec<(Option<u32>, Vec<u32>)>,
    states: Vec<EntityState>,
    base: Vec<EntityState>,
    encoder: DeltaEncoder,
    payload: Vec<u8>,
    /// This tick's datagrams back to back, and the length of each, before they are cut into their own buffer.
    packet: Vec<u8>,
    lens: Vec<usize>,
    out: Vec<Bytes>,
}

impl View {
    /// An empty view.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The entities this player saw at the last build.
    #[must_use]
    pub fn visible(&self) -> &VisibleSet {
        &self.visible
    }

    /// The datagrams of the last build, to be sent in order. Drained by the caller.
    pub fn out(&mut self) -> std::vec::Drain<'_, Bytes> {
        self.out.drain(..)
    }

    /// Recomputes the visible set of the player in slot `me` and encodes its snapshot for `tick`, whose states
    /// `history` must hold, against `ack`, the newest tick the client decoded (0 for none). Writes at least one
    /// datagram, since an empty snapshot still tells the client the tick advanced; nothing for a player not placed
    /// yet. At most [`MAX_ENTITIES`] entities are sent, the lowest slots first.
    pub fn build(
        &mut self,
        me: usize,
        grid: &DenseGrid,
        history: &History,
        config: &VisibilityConfig,
        tick: u32,
        ack: u32,
    ) {
        self.out.clear();
        let Some(states) = history.at(tick) else {
            return;
        };
        let Some(own) = states.get(me).copied().flatten() else {
            return;
        };
        let [x, y, _] = own.position.to_metres();
        self.visible.update(
            grid,
            Some(EntityId(me as u32)),
            Point::new(x, y),
            config,
            &mut self.changes,
        );
        self.sent.resize_with(HISTORY_TICKS, Default::default);
        let slot = &mut self.sent[tick as usize % HISTORY_TICKS];
        slot.0 = Some(tick);
        slot.1.clear();
        self.states.clear();
        for id in self.visible.members().iter().take(MAX_ENTITIES) {
            if let Some(state) = states.get(id.0 as usize).copied().flatten() {
                slot.1.push(id.0);
                self.states.push(state);
            }
        }
        // Slots ascend, and so do wire ids (slot index in the high bits): the order the encoder needs.
        let base = baseline(&self.sent, history, tick, ack, &mut self.base);
        self.payload.clear();
        if self
            .encoder
            .encode(tick, &self.states, base, &mut self.payload)
            .is_err()
        {
            return;
        }
        let Ok(parts) = fragments(&self.payload) else {
            return;
        };
        self.packet.clear();
        self.lens.clear();
        for (fragment, part) in parts {
            let header = PacketHeader {
                kind: PacketKind::DeltaSnapshot,
                sequence: self.sequence,
                tick,
            };
            self.sequence = self.sequence.wrapping_add(1);
            let mut buf = [0; PacketHeader::SIZE];
            // The buffer is exactly the header size, so encoding cannot fail.
            let _ = header.encode(&mut buf);
            self.packet.extend_from_slice(&buf);
            self.packet.push(fragment);
            self.packet.extend_from_slice(part);
            self.lens.push(PacketHeader::SIZE + 1 + part.len());
        }
        cut_datagrams(&self.packet, &self.lens, &mut self.out);
    }
}

/// Copies `bytes`, datagrams back to back with the lengths in `lens`, into one buffer of exactly that size and
/// appends each datagram, cut from it, to `out` in order. The datagrams share that buffer and nothing else, so a
/// datagram quinn still holds for a slow client keeps only its own player's bytes of that tick alive (NET-9).
fn cut_datagrams(bytes: &[u8], lens: &[usize], out: &mut Vec<Bytes>) {
    let mut buffer = Bytes::copy_from_slice(bytes);
    for &len in lens {
        out.push(buffer.split_to(len));
    }
}

/// The snapshot `sent` holds for `ack`, rebuilt into `base` from the slots sent then and their states in `history`,
/// if both are still kept.
fn baseline<'b>(
    sent: &[(Option<u32>, Vec<u32>)],
    history: &History,
    tick: u32,
    ack: u32,
    base: &'b mut Vec<EntityState>,
) -> Option<Baseline<'b>> {
    if ack == 0 || ack >= tick || (tick - ack) as usize >= HISTORY_TICKS {
        return None;
    }
    let (sent_tick, slots) = sent.get(ack as usize % HISTORY_TICKS)?;
    let states = history.at(ack)?;
    if *sent_tick != Some(ack) {
        return None;
    }
    base.clear();
    base.extend(
        slots
            .iter()
            .filter_map(|&s| states.get(s as usize).copied().flatten()),
    );
    Some(Baseline {
        tick: ack,
        entities: base,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;
    use sawgrass_protocol::delta::{Reassembler, baseline_age, decode_delta};
    use sawgrass_protocol::{Position, Rotation, Velocity};

    struct Rng(u64);

    impl Rng {
        fn unit(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 24) as f32
        }
    }

    fn state(id: u32, p: [f32; 2]) -> EntityState {
        EntityState {
            id,
            position: Position::from_metres([p[0], p[1], 10.0]).expect("position"),
            velocity: Velocity::from_metres_per_second([1.0, 0.0, 0.0]).expect("velocity"),
            rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
        }
    }

    /// A dense crowd on a 600 m square, so most players see well over one datagram of others, and one joined
    /// player not placed yet (the last slot). Each tick everyone moves a little.
    struct Crowd {
        rng: Rng,
        points: Vec<[f32; 2]>,
        grid: DenseGrid,
        history: History,
    }

    impl Crowd {
        fn new(players: usize) -> Self {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            let points = (0..players)
                .map(|_| [rng.unit() * 600.0, rng.unit() * 600.0])
                .collect();
            Self {
                rng,
                points,
                grid: DenseGrid::default(),
                history: History::new(),
            }
        }

        fn step(&mut self, tick: u32) {
            let mut states = Vec::new();
            for (id, p) in self.points.iter_mut().enumerate() {
                p[0] += self.rng.unit() - 0.5;
                p[1] += self.rng.unit() - 0.5;
                self.grid.upsert(EntityId(id as u32), Point::new(p[0], p[1]));
                states.push(Some(state(id as u32, *p)));
            }
            states.push(None);
            self.grid.rebuild();
            self.history.record(tick, &states);
        }
    }

    fn build_all(threads: usize) -> Vec<Vec<Bytes>> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("pool");
        let mut crowd = Crowd::new(400);
        let mut views: Vec<View> = (0..=400).map(|_| View::new()).collect();
        let config = VisibilityConfig::default();
        let mut sent = Vec::new();
        for tick in 1..4 {
            crowd.step(tick);
            let (grid, history) = (&crowd.grid, &crowd.history);
            pool.install(|| {
                views
                    .par_iter_mut()
                    .enumerate()
                    // Every client acknowledged the previous tick.
                    .for_each(|(me, view)| view.build(me, grid, history, &config, tick, tick - 1));
            });
            sent.extend(views.iter_mut().map(|v| v.out().collect::<Vec<_>>()));
        }
        sent
    }

    /// One player's datagrams of a tick come out in order, as one buffer of exactly their bytes: contiguous, with
    /// nothing before or after them that a queued datagram could keep alive.
    #[test]
    fn a_players_datagrams_share_one_buffer_of_exactly_their_size() {
        let bytes: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
        let lens = [1100, 1100, 300];
        let mut cut = Vec::new();
        cut_datagrams(&bytes, &lens, &mut cut);
        assert_eq!(cut.iter().map(Bytes::len).collect::<Vec<_>>(), lens);
        assert_eq!(cut.concat(), bytes);
        for pair in cut.windows(2) {
            assert_eq!(pair[0].as_ptr().wrapping_add(pair[0].len()), pair[1].as_ptr());
        }
        let last = cut.pop().expect("last");
        drop(cut);
        assert_eq!(&last[..], &bytes[2200..]);
    }

    #[test]
    fn the_same_datagrams_on_one_thread_and_on_four() {
        let one = build_all(1);
        assert_eq!(one, build_all(4));
        // The crowd needs several fragments for the first, whole snapshot; the unplaced player gets nothing.
        assert!(one.iter().any(|datagrams| datagrams.len() > 1));
        assert!(one[400].is_empty());
    }

    /// A client that loses a third of its datagrams still rebuilds exactly what the server sees for it, every tick
    /// it decodes, and the deltas against its acknowledgements are much smaller than whole snapshots.
    #[test]
    fn a_client_rebuilds_the_view_through_lost_datagrams() {
        let mut crowd = Crowd::new(400);
        let config = VisibilityConfig::default();
        let mut view = View::new();
        let mut reassembler = Reassembler::new();
        let mut decoded: Vec<Option<(u32, Vec<EntityState>)>> = vec![None; HISTORY_TICKS];
        let (mut ack, mut rng, mut checked) = (0u32, Rng(7), 0);
        let (mut whole_bytes, mut delta_bytes) = (0usize, 0usize);
        for tick in 1..60u32 {
            crowd.step(tick);
            view.build(7, &crowd.grid, &crowd.history, &config, tick, ack);
            let datagrams: Vec<Bytes> = view.out().collect();
            let bytes: usize = datagrams.iter().map(Bytes::len).sum();
            let mut complete = None;
            for datagram in datagrams {
                if rng.unit() < 0.33 {
                    continue;
                }
                let (header, body) = PacketHeader::decode(&datagram).expect("header");
                assert_eq!((header.kind, header.tick), (PacketKind::DeltaSnapshot, tick));
                if let Some(payload) = reassembler.push(tick, body).expect("fragment") {
                    complete = Some(payload.to_vec());
                }
            }
            let Some(payload) = complete else { continue };
            let age = baseline_age(&payload).expect("age");
            if age == 0 {
                whole_bytes = whole_bytes.max(bytes);
            } else {
                delta_bytes = delta_bytes.max(bytes);
            }
            let base = (age > 0).then(|| {
                let (t, e) = decoded[(tick - age) as usize % HISTORY_TICKS]
                    .as_ref()
                    .expect("baseline kept");
                assert_eq!(*t, tick - age);
                Baseline {
                    tick: *t,
                    entities: e,
                }
            });
            let mut out = Vec::new();
            decode_delta(&payload, tick, base, &mut out).expect("decode");
            let states = crowd.history.at(tick).expect("states");
            let expected: Vec<EntityState> = view
                .visible()
                .members()
                .iter()
                .filter_map(|id| states[id.0 as usize])
                .collect();
            assert_eq!(out, expected, "tick {tick}");
            assert!(!out.iter().any(|s| s.id == 7));
            decoded[tick as usize % HISTORY_TICKS] = Some((tick, out));
            ack = tick;
            checked += 1;
        }
        assert!(checked > 10, "only {checked} ticks decoded");
        assert!(
            delta_bytes * 3 < whole_bytes,
            "delta {delta_bytes} B, whole {whole_bytes} B"
        );
    }

    #[test]
    fn an_acknowledgement_outside_the_history_gets_a_whole_snapshot() {
        let mut crowd = Crowd::new(50);
        let mut view = View::new();
        let config = VisibilityConfig::default();
        for tick in 1..40u32 {
            crowd.step(tick);
            // Acknowledges tick 1 forever, or a tick never sent.
            let ack = if tick < 30 { 1 } else { tick + 5 };
            view.build(3, &crowd.grid, &crowd.history, &config, tick, ack);
            let mut r = Reassembler::new();
            let mut payload = None;
            for d in view.out() {
                let (_, body) = PacketHeader::decode(&d).expect("header");
                payload = r
                    .push(tick, body)
                    .expect("fragment")
                    .map(<[u8]>::to_vec)
                    .or(payload);
            }
            let age = baseline_age(&payload.expect("complete")).expect("age");
            let in_history = tick > 1 && tick < 30 && (tick - 1) < HISTORY_TICKS as u32;
            assert_eq!(age > 0, in_history, "tick {tick}");
        }
    }
}
