//! Building one player's snapshot datagrams (task SRV-7).
//!
//! Everything a player's snapshot needs that changes, its visible set, sequence number and buffers, lives in its
//! own [`View`]. [`View::build`] reads the shared grid and wire states and writes only its own view, so the server
//! builds every player's view in parallel (SRV-4, "players in parallel on hecs",
//! `docs/research/tick-model`) and gets the same datagrams whatever the number of threads.

use bytes::{Bytes, BytesMut};
use sawgrass_protocol::game::{SNAPSHOT_MAX_ENTITIES, encode_snapshot};
use sawgrass_protocol::{EntityState, PacketHeader, PacketKind};

use crate::interest::{DenseGrid, EntityId, Point, VisibilityChanges, VisibilityConfig, VisibleSet};

/// Bytes a view reserves at a time for its datagrams. Once the gateway has sent and dropped them, the next
/// reservation takes the same memory back, so a player in a steady state allocates nothing per tick.
pub const VIEW_CHUNK: usize = 8 * 1024;

/// One player's visible set, snapshot sequence and output.
#[derive(Debug, Default)]
pub struct View {
    visible: VisibleSet,
    sequence: u16,
    changes: VisibilityChanges,
    states: Vec<EntityState>,
    packet: Vec<u8>,
    arena: BytesMut,
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

    /// Recomputes the visible set of the player in slot `me` and encodes its snapshot for `tick`: the wire state of
    /// every visible entity, at most [`SNAPSHOT_MAX_ENTITIES`] per datagram, and at least one datagram, since an
    /// empty snapshot still tells the client the tick advanced. Does nothing for a player not placed yet (no state
    /// in `states`).
    pub fn build(
        &mut self,
        me: usize,
        grid: &DenseGrid,
        states: &[Option<EntityState>],
        config: &VisibilityConfig,
        tick: u32,
    ) {
        self.out.clear();
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
        self.states.clear();
        self.states.extend(
            self.visible
                .members()
                .iter()
                .filter_map(|id| states.get(id.0 as usize).copied().flatten()),
        );
        let datagrams = self.states.len().div_ceil(SNAPSHOT_MAX_ENTITIES).max(1);
        for i in 0..datagrams {
            let start = i * SNAPSHOT_MAX_ENTITIES;
            let chunk = &self.states[start..(start + SNAPSHOT_MAX_ENTITIES).min(self.states.len())];
            self.packet.clear();
            let header = PacketHeader {
                kind: PacketKind::Snapshot,
                sequence: self.sequence,
                tick,
            };
            let mut buf = [0; PacketHeader::SIZE];
            // The buffer is exactly the header size, so encoding cannot fail.
            let _ = header.encode(&mut buf);
            self.packet.extend_from_slice(&buf);
            self.sequence = self.sequence.wrapping_add(1);
            if encode_snapshot(chunk, &mut self.packet).is_err() {
                continue;
            }
            if self.arena.capacity() < self.packet.len() {
                self.arena.reserve(VIEW_CHUNK.max(self.packet.len()));
            }
            self.arena.extend_from_slice(&self.packet);
            self.out.push(self.arena.split().freeze());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;
    use sawgrass_protocol::{Position, Rotation, Velocity};

    fn world(players: usize) -> (DenseGrid, Vec<Option<EntityState>>) {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 24) as f32
        };
        let mut grid = DenseGrid::default();
        let mut states = Vec::new();
        for id in 0..players {
            // A dense crowd: 600 m square, so most players see well over one datagram of others.
            let p = [random() * 600.0, random() * 600.0, 10.0];
            grid.upsert(EntityId(id as u32), Point::new(p[0], p[1]));
            states.push(Some(EntityState {
                id: id as u32,
                position: Position::from_metres(p).expect("position"),
                velocity: Velocity::from_metres_per_second([1.0, 0.0, 0.0]).expect("velocity"),
                rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
            }));
        }
        states.push(None); // a joined player not placed yet
        grid.rebuild();
        (grid, states)
    }

    fn build_all(threads: usize, grid: &DenseGrid, states: &[Option<EntityState>]) -> Vec<Vec<Bytes>> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("pool");
        let mut views: Vec<View> = (0..states.len()).map(|_| View::new()).collect();
        let config = VisibilityConfig::default();
        let mut sent = Vec::new();
        for tick in 0..3 {
            pool.install(|| {
                views
                    .par_iter_mut()
                    .enumerate()
                    .for_each(|(me, view)| view.build(me, grid, states, &config, tick));
            });
            sent.extend(views.iter_mut().map(|v| v.out().collect::<Vec<_>>()));
        }
        sent
    }

    #[test]
    fn the_same_datagrams_on_one_thread_and_on_four() {
        let (grid, states) = world(400);
        let one = build_all(1, &grid, &states);
        let four = build_all(4, &grid, &states);
        assert_eq!(one, four);
        // The crowd really needs several datagrams per player, and the unplaced player gets none.
        assert!(one.iter().any(|datagrams| datagrams.len() > 1));
        assert!(one[400].is_empty());
    }

    #[test]
    fn every_visible_entity_is_sent_once_and_never_the_player_itself() {
        let (grid, states) = world(400);
        let mut view = View::new();
        view.build(7, &grid, &states, &VisibilityConfig::default(), 1);
        let mut ids = Vec::new();
        for datagram in view.out() {
            sawgrass_protocol::game::decode_snapshot(&datagram[PacketHeader::SIZE..], |s| ids.push(s.id))
                .expect("decode");
        }
        let expected: Vec<u32> = view.visible().members().iter().map(|id| id.0).collect();
        assert_eq!(ids, expected);
        assert!(!ids.contains(&7));
        assert!(!ids.is_empty());
    }
}
