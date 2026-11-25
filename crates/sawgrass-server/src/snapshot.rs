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
//!
//! An entity's change between two ticks is the same bytes for every player who has it in a baseline of the older
//! tick. [`History::record`] therefore encodes, once per tick, every slot's state as an addition and its change
//! against each baseline age some player acknowledged; a view only merges its slot lists and copies those bodies,
//! so the per-player cost is about that of a whole snapshot. A view whose acknowledgement was not passed to
//! `record` encodes its changes itself.

use bytes::{Bytes, BytesMut};
use sawgrass_protocol::delta::{
    DeltaLists, FRAGMENT_PAYLOAD, MAX_ENTITIES, encode_added, encode_block, encode_changed, fragments,
};
use sawgrass_protocol::entity::{EntityKind, Info};
use sawgrass_protocol::{EntityState, PacketHeader, PacketKind};

use crate::interest::{DenseGrid, EntityId, Point, VisibilityChanges, VisibilityConfig, VisibleSet};

/// Most entities in one player's snapshot (`docs/architecture.md`: "about 100–150 nearby, by priority"). When
/// more are visible, players go first, then moving vehicles and NPCs, then everything else (IM-7); within each
/// class the nearest are sent, and one already sent counts as [`KEEP_FACTOR`] times nearer, so an entity at the
/// edge of the cut does not flicker in and out (IM-6).
pub const SNAPSHOT_LIMIT: usize = 128;

/// How much nearer an entity already in the last snapshot counts when the visible set is cut: 0.7 of its squared
/// distance, so it stays until a newcomer is about 16% nearer than it.
pub const KEEP_FACTOR: f32 = 0.7;

/// How often a player's cut is ranked again, in ticks, when more is visible than [`SNAPSHOT_LIMIT`]. In between,
/// the entities of its last snapshot that are still visible are sent again; players are spread over the ticks by
/// slot. Ranking is a third of a crowded player's snapshot cost, and an entity entering a full view waits at most
/// this many ticks.
pub const RANK_EVERY: usize = 4;

/// Slowest speed, in m/s, at which a vehicle or NPC counts as moving when the visible set is cut (IM-7): above
/// a walk's drift so parked cars and idle NPCs rank as still. A placeholder until the game's figures are known.
pub const MOVING_SPEED: f32 = 0.5;

/// Added to the squared distance in cm² of each class below players, so a class never mixes with the one above:
/// more than the square of any distance on the 16 km map.
const CLASS_GAP: f64 = 1e13;

/// Ticks of history kept for baselines: 0.8 s at 20 Hz, several round trips.
pub const HISTORY_TICKS: usize = 16;

/// No entity in the slot. Wire ids never take this value: the generation's low 16 bits would be 0xffff in slot
/// 65,535, beyond [`crate::slots::MAX_SLOTS`].
const NO_ID: u32 = u32::MAX;

/// Where a slot's encoded body is in a [`Bodies`] buffer.
#[derive(Debug, Clone, Copy)]
struct Body {
    /// The wire id of the entity in the slot now.
    id: u32,
    start: u32,
    len: u8,
}

impl Body {
    /// The slot holds no state now, or (for a change) none at the older tick or another entity then, the slot
    /// having been reused.
    const NONE: Body = Body {
        id: NO_ID,
        start: 0,
        len: u8::MAX,
    };
}

/// One body per slot, back to back.
#[derive(Debug, Default)]
struct Bodies {
    bytes: Vec<u8>,
    at: Vec<Body>,
}

impl Bodies {
    fn clear(&mut self) {
        self.bytes.clear();
        self.at.clear();
    }

    /// Appends the next slot's body, written by `write` for the entity `id`, or [`Body::NONE`] when it returns
    /// `None`.
    fn push(&mut self, id: u32, write: impl FnOnce(&mut Vec<u8>) -> Option<()>) {
        let start = self.bytes.len();
        let body = match write(&mut self.bytes) {
            // A body is at most 29 bytes of state and 87 of info (an added entity with every field), so it fits
            // in a u8 below NONE's length.
            Some(()) => Body {
                id,
                start: start as u32,
                len: (self.bytes.len() - start) as u8,
            },
            None => {
                self.bytes.truncate(start);
                Body::NONE
            }
        };
        self.at.push(body);
    }

    /// The wire id and body of `slot`; `None` for [`Body::NONE`].
    #[inline]
    fn get(&self, slot: usize) -> Option<(u32, &[u8])> {
        let body = self.at.get(slot).copied().unwrap_or(Body::NONE);
        (body.len != u8::MAX).then(|| {
            (
                body.id,
                self.bytes
                    .get(body.start as usize..body.start as usize + usize::from(body.len))
                    .unwrap_or(&[]),
            )
        })
    }
}

/// The wire state of every slot in each of the last [`HISTORY_TICKS`] ticks, and the newest tick's bodies shared
/// by every player's snapshot.
#[derive(Debug, Default)]
pub struct History {
    ticks: [Option<u32>; HISTORY_TICKS],
    states: Vec<Vec<Option<EntityState>>>,
    /// The wire id in each slot, [`NO_ID`] for none: what a view's merge reads, a tenth of the states' bytes.
    ids: Vec<Vec<u32>>,
    /// The newest tick, whose bodies these are.
    newest: Option<u32>,
    /// Every slot's state of the newest tick, encoded as an addition.
    added: Bodies,
    /// Every slot's change from the tick `age` before the newest, at `age - 1`, for the ages in `shared`; empty if
    /// it did not change.
    changed: Vec<Bodies>,
    /// Bit `age` is set if `changed` holds that age for the newest tick.
    shared: u32,
    /// Every slot's kind, model and block fields at the newest tick.
    infos: Vec<Info>,
    /// The tick each slot's info last changed at: a view whose baseline is older sends that slot's block.
    info_ticks: Vec<u32>,
}

impl History {
    /// An empty history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the states of `tick`, by slot, replacing the tick [`HISTORY_TICKS`] before it, and encodes the
    /// bodies its snapshots share: every addition, and the changes against the tick each of `acks` names, the
    /// players' acknowledgements. Called on a rayon pool, it encodes them on that pool.
    ///
    /// `infos` holds each slot's info and `info_ticks` the tick it last changed at; a slot past their ends is a
    /// player with no block that never changed.
    pub fn record(
        &mut self,
        tick: u32,
        states: &[Option<EntityState>],
        infos: &[Info],
        info_ticks: &[u32],
        acks: impl IntoIterator<Item = u32>,
    ) {
        self.infos.clear();
        self.infos.extend_from_slice(infos);
        self.info_ticks.clear();
        self.info_ticks.extend_from_slice(info_ticks);
        let i = tick as usize % HISTORY_TICKS;
        self.states.resize_with(HISTORY_TICKS, Vec::new);
        self.states[i].clear();
        self.states[i].extend_from_slice(states);
        self.ids.resize_with(HISTORY_TICKS, Vec::new);
        self.ids[i].clear();
        self.ids[i].extend(states.iter().map(|s| s.map_or(NO_ID, |s| s.id)));
        self.ticks[i] = Some(tick);
        self.newest = Some(tick);
        let mut wanted = 0u32;
        for ack in acks {
            if ack != 0 && ack < tick && ((tick - ack) as usize) < HISTORY_TICKS {
                wanted |= 1 << (tick - ack);
            }
        }
        let olds: [Option<&[Option<EntityState>]>; HISTORY_TICKS - 1] = std::array::from_fn(|k| {
            let age = k as u32 + 1;
            if wanted & (1 << age) == 0 {
                return None;
            }
            let t = tick - age;
            let j = t as usize % HISTORY_TICKS;
            (self.ticks[j] == Some(t)).then(|| &self.states[j][..])
        });
        self.shared = olds
            .iter()
            .enumerate()
            .filter(|(_, old)| old.is_some())
            .fold(0, |bits, (k, _)| bits | 1 << (k + 1));
        self.changed.resize_with(HISTORY_TICKS - 1, Bodies::default);
        let (added, changed, infos) = (&mut self.added, &mut self.changed, &self.infos[..]);
        // Inside a pool (the tick's), the bodies are encoded in parallel, one task per age; elsewhere on this thread.
        if rayon::current_thread_index().is_some() {
            rayon::scope(|scope| {
                scope.spawn(|_| fill_added(added, states, infos));
                for (bodies, old) in changed.iter_mut().zip(olds) {
                    if let Some(old) = old {
                        scope.spawn(move |_| fill_changed(bodies, old, states));
                    }
                }
            });
        } else {
            fill_added(added, states, infos);
            for (bodies, old) in changed.iter_mut().zip(olds) {
                if let Some(old) = old {
                    fill_changed(bodies, old, states);
                }
            }
        }
    }

    /// The info of `slot` at the newest tick.
    fn info(&self, slot: usize) -> Info {
        self.infos.get(slot).copied().unwrap_or(Info::PLAYER)
    }

    /// Whether the info of `slot` changed after `tick`.
    fn info_changed_after(&self, slot: usize, tick: u32) -> bool {
        self.info_ticks.get(slot).is_some_and(|&t| t > tick)
    }

    /// The shared changes against the baseline `age` ticks before the newest tick, if `record` encoded them.
    fn changes(&self, age: u32) -> Option<&Bodies> {
        (age < 32 && self.shared & (1 << age) != 0).then(|| &self.changed[age as usize - 1])
    }

    /// The wire ids of `tick`, by slot, [`NO_ID`] for none, if it is still recorded.
    fn ids_at(&self, tick: u32) -> Option<&[u32]> {
        let i = tick as usize % HISTORY_TICKS;
        (self.ticks[i] == Some(tick)).then(|| &self.ids[i][..])
    }

    /// The states of `tick`, by slot, if it is still recorded.
    #[must_use]
    pub fn at(&self, tick: u32) -> Option<&[Option<EntityState>]> {
        let i = tick as usize % HISTORY_TICKS;
        (self.ticks[i] == Some(tick)).then(|| &self.states[i][..])
    }
}

/// Encodes every slot's state of `states` and info of `infos` as an addition.
fn fill_added(bodies: &mut Bodies, states: &[Option<EntityState>], infos: &[Info]) {
    bodies.clear();
    for (slot, state) in states.iter().enumerate() {
        bodies.push(state.map_or(NO_ID, |s| s.id), |out| {
            encode_added(state.as_ref()?, infos.get(slot).unwrap_or(&Info::PLAYER), out);
            Some(())
        });
    }
}

/// Appends a block entry for `id` that sends every field of `info`: the view does not know which of them the
/// client's baseline lacks, and the client takes each sent field as is.
fn push_block(lists: &mut DeltaLists, scratch: &mut Vec<u8>, id: u32, info: &Info) -> Option<()> {
    let mut differs = *info;
    for (i, &v) in info.fields().iter().enumerate() {
        differs.set_field(i, v ^ 1).ok()?;
    }
    scratch.clear();
    if encode_block(id, &differs, info, scratch) {
        lists.block(id, scratch).ok()?;
    }
    Some(())
}

/// Encodes every slot's change from `old` to `states`.
fn fill_changed(bodies: &mut Bodies, old: &[Option<EntityState>], states: &[Option<EntityState>]) {
    bodies.clear();
    for (slot, state) in states.iter().enumerate() {
        bodies.push(state.map_or(NO_ID, |s| s.id), |out| {
            let (old, new) = (old.get(slot).copied().flatten()?, state.as_ref()?);
            if old.id != new.id {
                return None;
            }
            encode_changed(&old, new, out);
            Some(())
        });
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
    lists: DeltaLists,
    /// An addition encoded for this view alone, when the shared bodies are not of this tick.
    body: Vec<u8>,
    /// A change encoded for this view alone, for a baseline age without shared bodies.
    change: Vec<u8>,
    payload: Vec<u8>,
    /// This build's candidates for the cut, in slot order: ranking key and slot.
    ranked: Vec<(f64, u32)>,
    /// The slots that may go into this snapshot, ascending: the visible set, or its cut.
    chosen: Vec<u32>,
    /// Scratch for finding the cut's threshold.
    keys: Vec<f64>,
    /// Players visible but left out of a ranked cut since the last [`View::take_players_cut`].
    players_cut: u64,
    /// The tick of the last snapshot built.
    last: Option<u32>,
    /// The length of each of this tick's datagrams, to cut them from their buffer.
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

    /// The slots in the last snapshot built, ascending.
    #[must_use]
    pub fn sent(&self) -> &[u32] {
        self.last
            .and_then(|t| self.sent.get(t as usize % HISTORY_TICKS))
            .map_or(&[], |(_, slots)| slots)
    }

    /// How many times a visible player was left out of a ranked cut since the last call (IM-7: only when more
    /// than [`SNAPSHOT_LIMIT`] players are visible).
    pub fn take_players_cut(&mut self) -> u64 {
        std::mem::take(&mut self.players_cut)
    }

    /// The datagrams of the last build, to be sent in order. Drained by the caller.
    pub fn out(&mut self) -> std::vec::Drain<'_, Bytes> {
        self.out.drain(..)
    }

    /// Recomputes the visible set of the player in slot `me` and encodes its snapshot for `tick`, whose states
    /// `history` must hold, against `ack`, the newest tick the client decoded (0 for none). Writes at least one
    /// datagram, since an empty snapshot still tells the client the tick advanced; nothing for a player not placed
    /// yet. At most [`SNAPSHOT_LIMIT`] entities are sent, the nearest, with hysteresis.
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
        let limit = SNAPSHOT_LIMIT.min(MAX_ENTITIES);
        self.sent.resize_with(HISTORY_TICKS, Default::default);
        let cut = self.visible.len() > limit;
        if cut {
            self.chosen.clear();
            let previous = self.last.map(|t| &self.sent[t as usize % HISTORY_TICKS].1);
            match previous {
                Some(slots) if (tick as usize + me) % RANK_EVERY != 0 => {
                    // Between rankings the cut stays: the entities of the last snapshot that are still visible.
                    let visible = &self.visible;
                    self.chosen
                        .extend(slots.iter().copied().filter(|&s| visible.contains(EntityId(s))));
                }
                _ => self.rank(states, &history.infos, own, limit),
            }
        }
        let i = tick as usize % HISTORY_TICKS;
        let mut current = std::mem::take(&mut self.sent[i].1);
        self.sent[i].0 = None;
        let encoded = if cut {
            let chosen = std::mem::take(&mut self.chosen);
            let encoded = self.encode(history, |m| chosen.get(m).copied(), &mut current, tick, ack);
            self.chosen = chosen;
            encoded
        } else {
            // Everything visible fits: no cut, and no copy of the visible set.
            let visible = std::mem::take(&mut self.visible);
            let members = visible.members();
            let encoded = self.encode(
                history,
                |m| members.get(m).map(|id| id.0),
                &mut current,
                tick,
                ack,
            );
            self.visible = visible;
            encoded
        };
        self.sent[i] = (Some(tick), current);
        self.last = Some(tick);
        if !encoded {
            return;
        }
        let Ok(parts) = fragments(&self.payload) else {
            return;
        };
        let count = self.payload.len().div_ceil(FRAGMENT_PAYLOAD).max(1);
        // Written straight into one buffer of exactly this player's bytes for the tick.
        let mut packet = BytesMut::with_capacity(self.payload.len() + count * (PacketHeader::SIZE + 1));
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
            packet.extend_from_slice(&buf);
            packet.extend_from_slice(&[fragment]);
            packet.extend_from_slice(part);
            self.lens.push(PacketHeader::SIZE + 1 + part.len());
        }
        cut_datagrams(packet.freeze(), &self.lens, &mut self.out);
    }
}

impl View {
    /// Writes into `payload` the snapshot for `tick` of the candidates that have a state, at most
    /// [`MAX_ENTITIES`], and puts their slots into `current`. `candidate(m)` is the `m`-th candidate slot,
    /// ascending. Encodes against the snapshot sent
    /// for `ack` if this view and `history` still hold it, else whole. Slots ascend, and so do wire ids (slot index
    /// in the high bits), the order the lists need. Returns `false` if it could not be encoded, which the lists'
    /// checks make a server bug, not a client's doing.
    fn encode(
        &mut self,
        history: &History,
        candidate: impl Fn(usize) -> Option<u32>,
        current: &mut Vec<u32>,
        tick: u32,
        ack: u32,
    ) -> bool {
        let (Some(states), Some(ids)) = (history.at(tick), history.ids_at(tick)) else {
            return false;
        };
        self.lists.clear();
        self.payload.clear();
        current.clear();
        let shared = history.newest == Some(tick);
        let base = (ack != 0 && ack < tick && ((tick - ack) as usize) < HISTORY_TICKS)
            .then(|| {
                let (sent_tick, slots) = self.sent.get(ack as usize % HISTORY_TICKS)?;
                (*sent_tick == Some(ack)).then_some(())?;
                Some((&slots[..], history.ids_at(ack)?, history.at(ack)?))
            })
            .flatten();
        let age = base.map_or(0, |_| tick - ack);
        let changes = if shared && age > 0 {
            history.changes(age)
        } else {
            None
        };
        let (old_slots, old_ids, old_states) = base.unwrap_or((&[], &[], &[]));
        let Self {
            lists, body, change, ..
        } = self;
        // Adds the entity in `slot` now, with the shared body or one encoded here; `Some(false)` if it has no state.
        let mut add = |lists: &mut DeltaLists, slot: usize| -> Option<bool> {
            if shared {
                let Some((id, tail)) = history.added.get(slot) else {
                    return Some(false);
                };
                lists.add(id, tail).ok()?;
            } else {
                let Some(state) = states.get(slot).copied().flatten() else {
                    return Some(false);
                };
                body.clear();
                encode_added(&state, &history.info(slot), body);
                lists.add(state.id, body).ok()?;
            }
            Some(true)
        };
        let merged = (|| -> Option<()> {
            let (mut a, mut m) = (0, 0);
            loop {
                let sa = old_slots.get(a).copied().unwrap_or(u32::MAX);
                let sb = match candidate(m) {
                    Some(slot) if current.len() < MAX_ENTITIES => slot,
                    _ => u32::MAX,
                };
                if sa < sb {
                    lists.remove(*old_ids.get(sa as usize)?).ok()?;
                    a += 1;
                    continue;
                }
                if sb == u32::MAX {
                    break;
                }
                m += 1;
                let slot = sb as usize;
                if sb < sa {
                    if add(lists, slot)? {
                        current.push(sb);
                    }
                    continue;
                }
                a += 1;
                if let Some(bodies) = changes {
                    // The common case: the change against this baseline age is already encoded.
                    if let Some((id, tail)) = bodies.get(slot) {
                        current.push(sb);
                        if !tail.is_empty() {
                            lists.change(id, tail).ok()?;
                        }
                        if history.info_changed_after(slot, ack) {
                            push_block(lists, change, id, &history.info(slot))?;
                        }
                        continue;
                    }
                }
                let (old_id, id) = (*old_ids.get(slot)?, *ids.get(slot)?);
                if old_id != id {
                    // Reused, or gone: the client drops the old entity and takes the new one, if any.
                    lists.remove(old_id).ok()?;
                    if add(lists, slot)? {
                        current.push(sb);
                    }
                    continue;
                }
                change.clear();
                let (old, new) = (
                    old_states.get(slot).copied().flatten()?,
                    states.get(slot).copied().flatten()?,
                );
                current.push(sb);
                if encode_changed(&old, &new, change) {
                    lists.change(id, change).ok()?;
                }
                if history.info_changed_after(slot, ack) {
                    push_block(lists, change, id, &history.info(slot))?;
                }
            }
            Some(())
        })();
        if merged.is_none() {
            return false;
        }
        self.lists.finish(age, &mut self.payload);
        true
    }
}

impl View {
    /// Chooses `limit` visible entities into `chosen`, in slot order: players first, then vehicles and NPCs moving
    /// at [`MOVING_SPEED`] or faster, then the rest, the nearest of each class (`infos` gives a slot's kind). One in
    /// the last snapshot counts as [`KEEP_FACTOR`] times nearer. Ties at the cut go to the lowest slots, so the result does not depend on how
    /// the candidates are ordered.
    fn rank(&mut self, states: &[Option<EntityState>], infos: &[Info], own: EntityState, limit: usize) {
        let own_cm = own.position.cm().map(|c| c as f32);
        self.ranked.clear();
        // The last snapshot's slots, ascending like the visible set: walked alongside it.
        let previous = self
            .last
            .map_or(&[][..], |t| &self.sent[t as usize % HISTORY_TICKS].1[..]);
        let mut p = 0;
        for id in self.visible.members() {
            let Some(state) = states.get(id.0 as usize).copied().flatten() else {
                continue;
            };
            let cm = state.position.cm();
            let (dx, dy) = (cm[0] as f32 - own_cm[0], cm[1] as f32 - own_cm[1]);
            while previous.get(p).is_some_and(|&s| s < id.0) {
                p += 1;
            }
            let kept = previous.get(p) == Some(&id.0);
            let near = f64::from((dx * dx + dy * dy) * if kept { KEEP_FACTOR } else { 1.0 });
            self.ranked
                .push((near + class(infos.get(id.0 as usize), &state) * CLASS_GAP, id.0));
        }
        if self.ranked.len() <= limit {
            self.chosen.extend(self.ranked.iter().map(|r| r.1));
            return;
        }
        // Players are the first class, so all of them make the cut unless they alone are over the limit.
        let players = self.ranked.iter().filter(|r| r.0 < CLASS_GAP).count();
        self.players_cut += players.saturating_sub(limit) as u64;
        // Find the key of the last one that makes the cut; everything below it goes, and of those equal to it the
        // lowest slots. The candidates stay in slot order, so nothing is sorted.
        self.keys.clear();
        self.keys.extend(self.ranked.iter().map(|r| r.0));
        let (_, &mut threshold, _) = self.keys.select_nth_unstable_by(limit - 1, f64::total_cmp);
        let mut ties = limit - self.ranked.iter().filter(|r| r.0 < threshold).count();
        for &(key, id) in &self.ranked {
            if key < threshold || (key == threshold && ties > 0) {
                if key == threshold {
                    ties -= 1;
                }
                self.chosen.push(id);
            }
        }
    }
}

/// The rank class of an entity whose info is `info`: 0 for a player (a slot past the infos is one), 1 for a
/// vehicle or NPC moving at [`MOVING_SPEED`] or faster, 2 for anything else.
fn class(info: Option<&Info>, state: &EntityState) -> f64 {
    match info.map(|i| i.kind) {
        None | Some(EntityKind::Player) => 0.0,
        Some(EntityKind::Vehicle | EntityKind::Npc) => {
            let [vx, vy, vz] = state.velocity.to_metres_per_second();
            if vx * vx + vy * vy + vz * vz >= MOVING_SPEED * MOVING_SPEED {
                1.0
            } else {
                2.0
            }
        }
        Some(EntityKind::Object) => 2.0,
    }
}

/// Appends each datagram of `buffer`, back to back with the lengths in `lens`, cut from it, to `out` in order.
/// `buffer` holds exactly those bytes, so the datagrams share it and nothing else: a datagram quinn still holds
/// for a slow client keeps only its own player's bytes of that tick alive (NET-9).
fn cut_datagrams(mut buffer: Bytes, lens: &[usize], out: &mut Vec<Bytes>) {
    for &len in lens {
        out.push(buffer.split_to(len));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;
    use sawgrass_protocol::delta::{
        Baseline, DeltaEncoder, Reassembler, baseline_age, decode_delta_with_info,
    };
    use sawgrass_protocol::entity::AssetId;
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

    fn ped(motion: u32) -> Info {
        let mut fields = [0; 14];
        fields[0] = motion;
        fields[12] = 100;
        Info::with_fields(EntityKind::Player, AssetId::NONE, &fields).expect("fields")
    }

    /// A dense crowd on a 600 m square, so most players see well over one datagram of others, and one joined
    /// player not placed yet (the last slot). Each tick everyone moves a little.
    struct Crowd {
        rng: Rng,
        points: Vec<[f32; 2]>,
        grid: DenseGrid,
        history: History,
        /// Each slot's generation, the low bits of its wire id; raised to reuse a slot.
        generations: Vec<u32>,
        /// Each slot's info and the tick it last changed; empty unless the crowd has ped blocks.
        infos: Vec<Info>,
        info_ticks: Vec<u32>,
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
                generations: vec![0; players],
                infos: Vec::new(),
                info_ticks: Vec::new(),
            }
        }

        /// The same crowd with ped blocks, one in fifty of which changes its motion each tick: about every 2.5 s at
        /// 20 Hz, as often as a walker stops or starts.
        fn with_peds(players: usize) -> Self {
            let mut crowd = Self::new(players);
            crowd.infos = vec![ped(0); players + 1];
            crowd.info_ticks = vec![0; players + 1];
            crowd
        }

        /// Moves everyone and records the tick, sharing bodies for the baselines `acks` names.
        fn step(&mut self, tick: u32, acks: &[u32]) {
            let mut states = Vec::new();
            for (id, p) in self.points.iter_mut().enumerate() {
                p[0] += self.rng.unit() - 0.5;
                p[1] += self.rng.unit() - 0.5;
                self.grid.upsert(EntityId(id as u32), Point::new(p[0], p[1]));
                states.push(Some(state((id as u32) << 16 | self.generations[id], *p)));
            }
            states.push(None);
            for slot in 0..self.infos.len() {
                if self.rng.unit() < 0.02 {
                    self.infos[slot] = ped((self.rng.unit() * 4.0) as u32);
                    self.info_ticks[slot] = tick;
                }
            }
            self.grid.rebuild();
            self.history
                .record(tick, &states, &self.infos, &self.info_ticks, acks.iter().copied());
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
            crowd.step(tick, &[tick - 1]);
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
        cut_datagrams(Bytes::copy_from_slice(&bytes), &lens, &mut cut);
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
        // Ped blocks change all the time, so the blocks list is rebuilt through the losses too.
        let mut crowd = Crowd::with_peds(400);
        let config = VisibilityConfig::default();
        let mut view = View::new();
        let mut reassembler = Reassembler::new();
        type Decoded = (u32, Vec<EntityState>, Vec<Info>);
        let mut decoded: Vec<Option<Decoded>> = vec![None; HISTORY_TICKS];
        let (mut ack, mut rng, mut checked) = (0u32, Rng(7), 0);
        let (mut whole_bytes, mut delta_bytes) = (0usize, 0usize);
        for tick in 1..60u32 {
            // Shared bodies on odd ticks, the view's own encoding on even ones: both must rebuild the same.
            let shared = [ack];
            crowd.step(tick, if tick % 2 == 1 { &shared[..] } else { &[] });
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
                let (t, e, i) = decoded[(tick - age) as usize % HISTORY_TICKS]
                    .as_ref()
                    .expect("baseline kept");
                assert_eq!(*t, tick - age);
                (
                    Baseline {
                        tick: *t,
                        entities: e,
                    },
                    &i[..],
                )
            });
            let (mut out, mut infos) = (Vec::new(), Vec::new());
            decode_delta_with_info(
                &payload,
                tick,
                base.map(|b| b.0),
                base.map_or(&[], |b| b.1),
                &mut out,
                &mut infos,
            )
            .expect("decode");
            let states = crowd.history.at(tick).expect("states");
            let expected: Vec<EntityState> = view.sent().iter().filter_map(|&s| states[s as usize]).collect();
            assert_eq!(out, expected, "tick {tick}");
            let expected: Vec<Info> = view.sent().iter().map(|&s| crowd.infos[s as usize]).collect();
            assert_eq!(infos, expected, "tick {tick}");
            assert!(!out.iter().any(|s| s.id >> 16 == 7));
            decoded[tick as usize % HISTORY_TICKS] = Some((tick, out, infos));
            ack = tick;
            checked += 1;
        }
        assert!(checked > 10, "only {checked} ticks decoded");
        assert!(
            delta_bytes * 3 < whole_bytes,
            "delta {delta_bytes} B, whole {whole_bytes} B"
        );
    }

    /// The shared bodies give the very bytes the protocol's own encoder writes, for every baseline age, with the
    /// changes shared or encoded by the view, and for a slot reused between the baseline and now.
    #[test]
    fn shared_bodies_encode_what_the_encoder_does() {
        let mut crowd = Crowd::new(400);
        let config = VisibilityConfig::default();
        let mut view = View::new();
        let mut encoder = DeltaEncoder::default();
        let ages = [0u32, 1, 2, 3, 4, 5, 7, 1, 2];
        let mut compared = 0;
        for tick in 1..40u32 {
            if tick == 20 {
                // Slots seen by player 7 change hands.
                for id in view.visible().members().iter().take(5) {
                    crowd.generations[id.0 as usize] += 1;
                }
            }
            let age = ages[tick as usize % ages.len()];
            let ack = tick.saturating_sub(age);
            // Shared bodies on all but every third tick, where the view encodes its changes itself.
            let shared = [ack];
            crowd.step(tick, if tick % 3 == 0 { &[][..] } else { &shared });
            view.build(7, &crowd.grid, &crowd.history, &config, tick, ack);
            let states = crowd.history.at(tick).expect("states");
            let of = |slots: &[u32], states: &[Option<EntityState>]| -> Vec<EntityState> {
                slots.iter().filter_map(|&s| states[s as usize]).collect()
            };
            let (_, slots) = &view.sent[tick as usize % HISTORY_TICKS];
            let current = of(slots, states);
            let base_states;
            let base = if age > 0 && tick > age {
                let (sent_tick, old) = &view.sent[ack as usize % HISTORY_TICKS];
                assert_eq!(*sent_tick, Some(ack));
                base_states = of(old, crowd.history.at(ack).expect("old states"));
                Some(Baseline {
                    tick: ack,
                    entities: &base_states,
                })
            } else {
                None
            };
            let mut expected = Vec::new();
            encoder
                .encode(tick, &current, base, &mut expected)
                .expect("encode");
            assert_eq!(view.payload, expected, "tick {tick}, age {age}");
            compared += 1;
        }
        assert_eq!(compared, 39);
    }

    /// More visible than the limit: the nearest are sent; one already sent stays until a newcomer is clearly nearer.
    #[test]
    fn the_cut_sends_the_nearest_and_does_not_flicker() {
        let mut grid = DenseGrid::default();
        let mut history = History::new();
        let config = VisibilityConfig::default();
        let mut view = View::new();
        // The viewer in slot 0 at the origin, others 1 m apart from 1 m out.
        let others = SNAPSHOT_LIMIT + 1;
        let place = |grid: &mut DenseGrid, history: &mut History, tick: u32, far: f32| {
            let mut states = vec![Some(state(0, [0.0, 0.0]))];
            grid.upsert(EntityId(0), Point::new(0.0, 0.0));
            for i in 1..=others {
                let x = if i == others { far } else { i as f32 };
                grid.upsert(EntityId(i as u32), Point::new(x, 0.0));
                states.push(Some(state(i as u32, [x, 0.0])));
            }
            grid.rebuild();
            history.record(tick, &states, &[], &[], []);
        };
        place(&mut grid, &mut history, 1, 150.0);
        view.build(0, &grid, &history, &config, 1, 0);
        let first: Vec<u32> = (1..=SNAPSHOT_LIMIT as u32).collect();
        assert_eq!(view.sent(), &first[..]);
        // The last one comes slightly nearer than the farthest sent: at the next ranking nothing changes.
        let rank = RANK_EVERY as u32;
        place(&mut grid, &mut history, rank, SNAPSHOT_LIMIT as f32 - 0.5);
        view.build(0, &grid, &history, &config, rank, 0);
        assert_eq!(view.sent(), &first[..]);
        // Clearly nearer: it waits for the next ranking, then replaces the farthest.
        place(&mut grid, &mut history, rank + 1, 100.5);
        view.build(0, &grid, &history, &config, rank + 1, 0);
        assert_eq!(view.sent(), &first[..]);
        place(&mut grid, &mut history, 2 * rank, 100.5);
        view.build(0, &grid, &history, &config, 2 * rank, 0);
        let mut third: Vec<u32> = (1..SNAPSHOT_LIMIT as u32).collect();
        third.push(others as u32);
        assert_eq!(view.sent(), &third[..]);
    }

    /// IM-7: a parking lot nearer than the players does not push them, or moving vehicles, out of the cut; within
    /// a class the nearest go, and of two at the same distance the lower slot.
    #[test]
    fn the_cut_sends_players_then_moving_entities_then_the_rest() {
        let mut grid = DenseGrid::default();
        let mut history = History::new();
        let mut view = View::new();
        let parked = SNAPSHOT_LIMIT as u32 + 1;
        let car = Info::with_fields(EntityKind::Vehicle, AssetId(3), &[255, 255, 0, 0, 0, 0]).expect("car");
        let still = |id: u32, x: f32| EntityState {
            velocity: Velocity::from_metres_per_second([0.1, 0.0, 0.0]).expect("velocity"),
            ..state(id, [x, 0.0])
        };
        // Slot 0 the viewer; 1..=parked still cars 1 m apart, the last as far as the farthest that makes the cut;
        // then two cars driving at 10 m/s and two players, all farther out.
        let mut states = vec![Some(state(0, [0.0, 0.0]))];
        let mut infos = vec![Info::PLAYER];
        for i in 1..=parked {
            let x = if i == parked {
                -(SNAPSHOT_LIMIT as f32 - 4.0)
            } else {
                i as f32
            };
            states.push(Some(still(i, x)));
            infos.push(car);
        }
        for (i, x) in [(parked + 1, 140.0), (parked + 2, 141.0)] {
            states.push(Some(EntityState {
                velocity: Velocity::from_metres_per_second([10.0, 0.0, 0.0]).expect("velocity"),
                ..state(i, [x, 0.0])
            }));
            infos.push(car);
        }
        for (i, x) in [(parked + 3, 150.0), (parked + 4, 151.0)] {
            states.push(Some(still(i, x)));
            infos.push(Info::PLAYER);
        }
        for (slot, s) in states.iter().enumerate() {
            let [x, y, _] = s.expect("state").position.to_metres();
            grid.upsert(EntityId(slot as u32), Point::new(x, y));
        }
        grid.rebuild();
        let ticks = vec![1; infos.len()];
        history.record(1, &states, &infos, &ticks, []);
        view.build(0, &grid, &history, &VisibilityConfig::default(), 1, 0);
        // Four places go to the players and moving cars; the nearest parked cars fill the rest, and of the two
        // parked at the edge of the cut the lower slot wins.
        let mut expected: Vec<u32> = (1..=SNAPSHOT_LIMIT as u32 - 4).collect();
        expected.extend(parked + 1..=parked + 4);
        assert_eq!(view.sent(), &expected[..]);
        assert_eq!(class(Some(&car), &still(1, 0.0)), 2.0);
        assert_eq!(class(Some(&car), &state(1, [0.0, 0.0])), 1.0);
        assert_eq!(class(None, &still(1, 0.0)), 0.0);
        assert_eq!(view.take_players_cut(), 0);
    }

    #[test]
    fn an_acknowledgement_outside_the_history_gets_a_whole_snapshot() {
        let mut crowd = Crowd::new(50);
        let mut view = View::new();
        let config = VisibilityConfig::default();
        for tick in 1..40u32 {
            // Acknowledges tick 1 forever, or a tick never sent.
            let ack = if tick < 30 { 1 } else { tick + 5 };
            crowd.step(tick, &[ack]);
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
