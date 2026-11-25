//! Delta-encoded snapshots (task NET-11, chosen in NET-5, `docs/research/packet-compression`).
//!
//! A snapshot is encoded against a baseline: an older snapshot the client said it decoded completely. Entities
//! in both with nothing changed are not sent at all, and a change of a few centimetres takes a byte or two. With
//! no baseline every entity is sent as new. The payload, every integer a LEB128 varint as postcard writes them,
//! signed ones zigzagged:
//!
//! ```text
//! baseline age in ticks (0: none, every entity is new)
//! removed: count, then id gaps                        entities in the baseline that left the view
//! changed: count, then per entity: id gap, mask, the changed fields as differences from the baseline
//! added:   count, then per entity: id gap, position, velocity, rotation in full
//! ```
//!
//! Ids ascend in each list. Each is sent as two varints: its high 16 bits as the gap from the previous id's high
//! bits (the first as is), then its low 16 bits. The server's ids are a slot index in the high bits and a reuse
//! generation in the low bits, so both usually take one byte. Mask bits 0 to 2 are
//! the position axes, 3 to 5 the velocity axes, bit 6 a rotation whose largest component is unchanged (three
//! 10-bit differences follow), bit 7 a rotation sent whole. A lost snapshot breaks nothing: the next one is
//! relative to whatever the client last acknowledged.
//!
//! A payload larger than one datagram is cut into at most [`MAX_FRAGMENTS`] fragments of [`FRAGMENT_PAYLOAD`]
//! bytes. Each datagram is a [`PacketHeader`](crate::PacketHeader) of kind
//! [`DeltaSnapshot`](crate::PacketKind::DeltaSnapshot), one fragment byte (index in the high nibble, count minus
//! one in the low) and its part of the payload. [`Reassembler`] gives the payload back once every part of a tick
//! has arrived; a tick with a part lost is never applied.

use crate::game::MAX_DATAGRAM;
use crate::{DecodeError, EncodeError, EntityState, PacketHeader, Position, Rotation, Velocity};

/// Most fragments of one snapshot.
pub const MAX_FRAGMENTS: usize = 16;

/// Payload bytes in one fragment.
pub const FRAGMENT_PAYLOAD: usize = MAX_DATAGRAM - PacketHeader::SIZE - 1;

/// Most bytes one entity can take in a payload: removed (two 3-byte id varints) and another added in its place
/// (ids, three 4-byte positions, three 3-byte velocities, a 5-byte rotation). One changed entity takes at most 34.
const WORST_ENTITY_BYTES: usize = 6 + 32;

/// Most entities in one snapshot, and in its baseline. Derived from [`MAX_FRAGMENTS`] so that any snapshot of this
/// many entities fits in its fragments, whatever their values; the decoder treats more as hostile. The header (age
/// and three counts) takes at most 20 bytes.
pub const MAX_ENTITIES: usize = (MAX_FRAGMENTS * FRAGMENT_PAYLOAD - 20) / WORST_ENTITY_BYTES;

const ROT_SHIFT: u32 = 10;
const ROT_MASK: u32 = (1 << ROT_SHIFT) - 1;

/// A snapshot the client has: its tick and its entities, sorted by id.
#[derive(Debug, Clone, Copy)]
pub struct Baseline<'a> {
    /// The tick it was sent for.
    pub tick: u32,
    /// Its entities, ascending by id.
    pub entities: &'a [EntityState],
}

fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_signed(out: &mut Vec<u8>, v: i64) {
    put(out, ((v << 1) ^ (v >> 63)) as u64);
}

fn rot_parts(r: u32) -> [i64; 4] {
    [
        i64::from(r >> (3 * ROT_SHIFT)),
        i64::from((r >> (2 * ROT_SHIFT)) & ROT_MASK),
        i64::from((r >> ROT_SHIFT) & ROT_MASK),
        i64::from(r & ROT_MASK),
    ]
}

fn ascending(entities: &[EntityState]) -> bool {
    entities.windows(2).all(|w| w[0].id < w[1].id)
}

/// Encodes snapshots; keeps its buffers between calls.
#[derive(Debug, Default)]
pub struct DeltaEncoder {
    removed: Vec<u8>,
    changed: Vec<u8>,
    added: Vec<u8>,
}

impl DeltaEncoder {
    /// Appends the payload of the snapshot for `tick` holding `current` to `out`, against `base` if given.
    ///
    /// Fails with [`EncodeError::OutOfRange`] if either list is not ascending by id, has more than
    /// [`MAX_ENTITIES`], or the baseline is not older than `tick`.
    pub fn encode(
        &mut self,
        tick: u32,
        current: &[EntityState],
        base: Option<Baseline<'_>>,
        out: &mut Vec<u8>,
    ) -> Result<(), EncodeError> {
        let (age, old) = match base {
            Some(b) if b.tick < tick => (tick - b.tick, b.entities),
            Some(_) => return Err(EncodeError::OutOfRange),
            None => (0, &[][..]),
        };
        if current.len() > MAX_ENTITIES || old.len() > MAX_ENTITIES || !ascending(current) || !ascending(old)
        {
            return Err(EncodeError::OutOfRange);
        }
        self.removed.clear();
        self.changed.clear();
        self.added.clear();
        let (mut n_removed, mut n_changed, mut n_added) = (0u64, 0u64, 0u64);
        let (mut last_removed, mut last_changed, mut last_added) = (0u32, 0u32, 0u32);
        let put_id = |out: &mut Vec<u8>, id: u32, last: &mut u32| {
            put(out, u64::from((id >> 16) - (*last >> 16)));
            put(out, u64::from(id & 0xffff));
            *last = id;
        };
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < current.len() {
            let ida = old.get(i).map_or(u64::MAX, |e| u64::from(e.id));
            let idb = current.get(j).map_or(u64::MAX, |e| u64::from(e.id));
            if ida < idb {
                let id = old[i].id;
                put_id(&mut self.removed, id, &mut last_removed);
                n_removed += 1;
                i += 1;
            } else if idb < ida {
                let e = &current[j];
                put_id(&mut self.added, e.id, &mut last_added);
                for v in e.position.cm() {
                    put_signed(&mut self.added, i64::from(v));
                }
                for v in e.velocity.cm_per_s {
                    put_signed(&mut self.added, i64::from(v));
                }
                put(&mut self.added, u64::from(e.rotation.bits));
                n_added += 1;
                j += 1;
            } else {
                let (a, b) = (&old[i], &current[j]);
                let (pa, pb) = (a.position.cm(), b.position.cm());
                let (va, vb) = (a.velocity.cm_per_s, b.velocity.cm_per_s);
                let mut mask = 0u8;
                for k in 0..3 {
                    if pa[k] != pb[k] {
                        mask |= 1 << k;
                    }
                    if va[k] != vb[k] {
                        mask |= 1 << (3 + k);
                    }
                }
                let (ra, rb) = (rot_parts(a.rotation.bits), rot_parts(b.rotation.bits));
                if ra != rb {
                    mask |= if ra[0] == rb[0] { 1 << 6 } else { 1 << 7 };
                }
                if mask != 0 {
                    let c = &mut self.changed;
                    put_id(c, b.id, &mut last_changed);
                    c.push(mask);
                    for k in 0..3 {
                        if mask & (1 << k) != 0 {
                            put_signed(c, i64::from(pb[k]) - i64::from(pa[k]));
                        }
                    }
                    for k in 0..3 {
                        if mask & (1 << (3 + k)) != 0 {
                            put_signed(c, i64::from(vb[k]) - i64::from(va[k]));
                        }
                    }
                    if mask & (1 << 6) != 0 {
                        for k in 1..4 {
                            put_signed(c, rb[k] - ra[k]);
                        }
                    }
                    if mask & (1 << 7) != 0 {
                        put(c, u64::from(b.rotation.bits));
                    }
                    n_changed += 1;
                }
                i += 1;
                j += 1;
            }
        }
        put(out, u64::from(age));
        put(out, n_removed);
        out.extend_from_slice(&self.removed);
        put(out, n_changed);
        out.extend_from_slice(&self.changed);
        put(out, n_added);
        out.extend_from_slice(&self.added);
        Ok(())
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let (&b, rest) = self.0.split_first().ok_or(DecodeError::Truncated)?;
        self.0 = rest;
        Ok(b)
    }

    fn get(&mut self) -> Result<u64, DecodeError> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            // The tenth byte holds only the top bit; anything more would be lost.
            if shift == 63 && b > 1 {
                return Err(DecodeError::OutOfRange);
            }
            v |= u64::from(b & 0x7f)
                .checked_shl(shift)
                .ok_or(DecodeError::OutOfRange)?;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError::OutOfRange)
    }

    fn get_signed(&mut self) -> Result<i64, DecodeError> {
        let v = self.get()?;
        Ok((v >> 1) as i64 ^ -((v & 1) as i64))
    }

    fn count(&mut self) -> Result<usize, DecodeError> {
        let n = usize::try_from(self.get()?).map_err(|_| DecodeError::OutOfRange)?;
        // Each entry takes at least one byte, so a count larger than what is left is a lie.
        if n > MAX_ENTITIES || n > self.0.len() {
            return Err(DecodeError::OutOfRange);
        }
        Ok(n)
    }

    /// The next id in an ascending list: the gap in its high 16 bits, then its low 16 bits.
    fn id(&mut self, last: &mut Option<u32>) -> Result<u32, DecodeError> {
        let gap = self.get()?;
        let low = self.get()?;
        // The gap comes from the payload: it may be anything up to u64::MAX.
        let high = u64::from(last.map_or(0, |l| l >> 16))
            .checked_add(gap)
            .ok_or(DecodeError::OutOfRange)?;
        if high > 0xffff || low > 0xffff {
            return Err(DecodeError::OutOfRange);
        }
        let id = ((high as u32) << 16) | low as u32;
        if last.is_some_and(|l| id <= l) {
            return Err(DecodeError::OutOfRange);
        }
        *last = Some(id);
        Ok(id)
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        u32::try_from(self.get()?).map_err(|_| DecodeError::OutOfRange)
    }
}

fn narrow<T: TryFrom<i64>>(v: i64) -> Result<T, DecodeError> {
    T::try_from(v).map_err(|_| DecodeError::OutOfRange)
}

fn position(cm: [i64; 3]) -> Result<Position, DecodeError> {
    Position::from_cm([narrow(cm[0])?, narrow(cm[1])?, narrow(cm[2])?]).map_err(|_| DecodeError::OutOfRange)
}

/// The baseline age a payload names: 0 for none, otherwise how many ticks before its own tick the baseline is.
pub fn baseline_age(payload: &[u8]) -> Result<u32, DecodeError> {
    Reader(payload).u32()
}

/// Decodes the payload of the snapshot for `tick` against `base` into `out` (cleared first), ascending by id.
///
/// Every count, id and value is checked: a baseline other than the one the payload names, an id that is not in
/// the baseline, a duplicate, a value out of range or a byte left over is an error, and `out` is then unspecified.
pub fn decode_delta(
    payload: &[u8],
    tick: u32,
    base: Option<Baseline<'_>>,
    out: &mut Vec<EntityState>,
) -> Result<(), DecodeError> {
    out.clear();
    let mut r = Reader(payload);
    let age = r.u32()?;
    let old = match (age, base) {
        (0, _) => &[][..],
        (age, Some(b)) if tick.checked_sub(b.tick) == Some(age) => b.entities,
        _ => return Err(DecodeError::OutOfRange),
    };

    // Removed: ids that must be in the baseline; walked in step with it.
    let n = r.count()?;
    let mut removed = Vec::with_capacity(n);
    let mut last = None;
    for _ in 0..n {
        removed.push(r.id(&mut last)?);
    }
    let (mut ri, mut oi) = (0, 0);
    let mut kept: Vec<EntityState> = Vec::with_capacity(old.len());
    for e in old {
        if removed.get(ri) == Some(&e.id) {
            ri += 1;
        } else {
            kept.push(*e);
        }
    }
    if ri != removed.len() {
        return Err(DecodeError::OutOfRange);
    }

    // Changed: applied to the kept entities, which are in id order like the list.
    let n = r.count()?;
    let mut last = None;
    for _ in 0..n {
        let id = r.id(&mut last)?;
        while oi < kept.len() && kept[oi].id < id {
            oi += 1;
        }
        let e = kept
            .get_mut(oi)
            .filter(|e| e.id == id)
            .ok_or(DecodeError::OutOfRange)?;
        let mask = r.byte()?;
        if mask == 0 || mask & 0xC0 == 0xC0 {
            return Err(DecodeError::OutOfRange);
        }
        let mut cm = e.position.cm().map(i64::from);
        for (k, v) in cm.iter_mut().enumerate() {
            if mask & (1 << k) != 0 {
                *v = v.checked_add(r.get_signed()?).ok_or(DecodeError::OutOfRange)?;
            }
        }
        e.position = position(cm)?;
        for k in 0..3 {
            if mask & (1 << (3 + k)) != 0 {
                let v = i64::from(e.velocity.cm_per_s[k]).checked_add(r.get_signed()?);
                e.velocity.cm_per_s[k] = narrow(v.ok_or(DecodeError::OutOfRange)?)?;
            }
        }
        if mask & (1 << 6) != 0 {
            let mut p = rot_parts(e.rotation.bits);
            for part in &mut p[1..] {
                *part = part.checked_add(r.get_signed()?).ok_or(DecodeError::OutOfRange)?;
                if !(0..=i64::from(ROT_MASK)).contains(part) {
                    return Err(DecodeError::OutOfRange);
                }
            }
            e.rotation.bits =
                (p[0] << (3 * ROT_SHIFT) | p[1] << (2 * ROT_SHIFT) | p[2] << ROT_SHIFT | p[3]) as u32;
        }
        if mask & (1 << 7) != 0 {
            e.rotation = Rotation { bits: r.u32()? };
        }
    }

    // Added: merged with the kept entities; an id already present is an error.
    let n = r.count()?;
    let mut last = None;
    let mut ki = 0;
    for _ in 0..n {
        let id = r.id(&mut last)?;
        let cm = [r.get_signed()?, r.get_signed()?, r.get_signed()?];
        let mut cm_per_s = [0i16; 3];
        for v in &mut cm_per_s {
            *v = narrow(r.get_signed()?)?;
        }
        let added = EntityState {
            id,
            position: position(cm)?,
            velocity: Velocity { cm_per_s },
            rotation: Rotation { bits: r.u32()? },
        };
        while ki < kept.len() && kept[ki].id < id {
            out.push(kept[ki]);
            ki += 1;
        }
        if kept.get(ki).is_some_and(|e| e.id == id) {
            return Err(DecodeError::OutOfRange);
        }
        out.push(added);
    }
    out.extend_from_slice(&kept[ki..]);
    if !r.0.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    if out.len() > MAX_ENTITIES {
        return Err(DecodeError::OutOfRange);
    }
    Ok(())
}

/// Cuts `payload` into fragments: yields each fragment byte with its part. Fails with
/// [`EncodeError::OutOfRange`] if it needs more than [`MAX_FRAGMENTS`].
pub fn fragments(payload: &[u8]) -> Result<impl Iterator<Item = (u8, &[u8])>, EncodeError> {
    let count = payload.len().div_ceil(FRAGMENT_PAYLOAD).max(1);
    if count > MAX_FRAGMENTS {
        return Err(EncodeError::OutOfRange);
    }
    Ok((0..count).map(move |i| {
        let part = &payload
            [(i * FRAGMENT_PAYLOAD).min(payload.len())..((i + 1) * FRAGMENT_PAYLOAD).min(payload.len())];
        (((i as u8) << 4) | (count as u8 - 1), part)
    }))
}

/// Collects the fragments of the newest tick and gives its payload back once all have arrived.
#[derive(Debug, Default)]
pub struct Reassembler {
    tick: Option<u32>,
    count: usize,
    received: u16,
    parts: Vec<Vec<u8>>,
    payload: Vec<u8>,
}

impl Reassembler {
    /// An empty reassembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes one datagram's body (fragment byte and part) for `tick`. Returns the whole payload when this was the
    /// last missing part of the newest tick seen; `None` while parts are missing, or for a part of an older tick or
    /// one already received. A fragment byte that contradicts the tick's earlier parts is an error.
    pub fn push(&mut self, tick: u32, body: &[u8]) -> Result<Option<&[u8]>, DecodeError> {
        let (&fragment, part) = body.split_first().ok_or(DecodeError::Truncated)?;
        let (index, count) = (usize::from(fragment >> 4), usize::from(fragment & 0x0f) + 1);
        if index >= count || part.len() > FRAGMENT_PAYLOAD {
            return Err(DecodeError::OutOfRange);
        }
        match self.tick {
            Some(t) if tick < t => return Ok(None),
            Some(t) if tick == t => {
                if count != self.count {
                    return Err(DecodeError::OutOfRange);
                }
                if self.received & (1 << index) != 0 {
                    return Ok(None);
                }
            }
            _ => {
                self.tick = Some(tick);
                self.count = count;
                self.received = 0;
                self.parts.resize_with(MAX_FRAGMENTS, Vec::new);
            }
        }
        self.received |= 1 << index;
        self.parts[index].clear();
        self.parts[index].extend_from_slice(part);
        if self.received.count_ones() as usize != count {
            return Ok(None);
        }
        // A later duplicate of this tick must not deliver it twice.
        self.received = u16::MAX;
        self.payload.clear();
        for part in &self.parts[..count] {
            self.payload.extend_from_slice(part);
        }
        Ok(Some(&self.payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: u32, x: f32, vx: f32, yaw: f32) -> EntityState {
        let half = yaw / 2.0;
        EntityState {
            id,
            position: Position::from_metres([x, 2.0 * x, 30.0]).expect("position"),
            velocity: Velocity::from_metres_per_second([vx, 0.0, -1.0]).expect("velocity"),
            rotation: Rotation::from_quaternion([0.0, 0.0, half.sin(), half.cos()]).expect("rotation"),
        }
    }

    fn round_trip(
        tick: u32,
        current: &[EntityState],
        base: Option<Baseline<'_>>,
    ) -> (usize, Vec<EntityState>) {
        let mut payload = Vec::new();
        DeltaEncoder::default()
            .encode(tick, current, base, &mut payload)
            .expect("encode");
        let mut out = Vec::new();
        decode_delta(&payload, tick, base, &mut out).expect("decode");
        (payload.len(), out)
    }

    #[test]
    fn whole_and_delta_snapshots_round_trip() {
        let base: Vec<EntityState> = (0..100).map(|i| entity(i * 3, i as f32, 1.0, 0.1)).collect();
        let (whole, out) = round_trip(10, &base, None);
        assert_eq!(out, base);

        // Most move a little, one turns past the largest component, some leave, some arrive, some stand still.
        let mut current: Vec<EntityState> = base
            .iter()
            .filter(|e| e.id % 7 != 0)
            .map(|e| {
                let [x, _, _] = e.position.to_metres();
                if e.id % 5 == 0 {
                    *e
                } else {
                    entity(e.id, x + 0.03, 1.2, 0.12)
                }
            })
            .collect();
        current[3] = entity(current[3].id, 1.0, 1.0, 3.0);
        current.extend((0..5).map(|i| entity(1000 + i, 9.0, 0.0, 0.0)));
        let b = Baseline {
            tick: 8,
            entities: &base,
        };
        let (delta, out) = round_trip(10, &current, Some(b));
        assert_eq!(out, current);
        assert!(delta * 2 < whole, "delta {delta} B against whole {whole} B");
    }

    #[test]
    fn a_reused_slot_is_a_removal_and_an_addition() {
        // Same slot (low 16 bits), new generation: the client must drop the old entity and take the new one.
        let base = vec![entity(5, 1.0, 0.0, 0.0)];
        let current = vec![entity(5, 1.0, 0.0, 0.0), entity((1 << 16) | 5, 1.0, 0.0, 0.0)];
        let (_, out) = round_trip(
            3,
            &current,
            Some(Baseline {
                tick: 2,
                entities: &base,
            }),
        );
        assert_eq!(out, current);
        let current = vec![entity((1 << 16) | 5, 1.0, 0.0, 0.0)];
        let (_, out) = round_trip(
            3,
            &current,
            Some(Baseline {
                tick: 2,
                entities: &base,
            }),
        );
        assert_eq!(out, current);
    }

    #[test]
    fn the_wrong_baseline_is_refused() {
        let base = vec![entity(1, 1.0, 0.0, 0.0)];
        let mut payload = Vec::new();
        DeltaEncoder::default()
            .encode(
                10,
                &base,
                Some(Baseline {
                    tick: 8,
                    entities: &base,
                }),
                &mut payload,
            )
            .expect("encode");
        assert_eq!(baseline_age(&payload), Ok(2));
        let mut out = Vec::new();
        let other = Baseline {
            tick: 7,
            entities: &base,
        };
        assert!(decode_delta(&payload, 10, Some(other), &mut out).is_err());
        assert!(decode_delta(&payload, 10, None, &mut out).is_err());
    }

    #[test]
    fn unsorted_input_and_a_future_baseline_are_not_encoded() {
        let mut enc = DeltaEncoder::default();
        let mut out = Vec::new();
        let unsorted = [entity(2, 0.0, 0.0, 0.0), entity(1, 0.0, 0.0, 0.0)];
        assert!(enc.encode(1, &unsorted, None, &mut out).is_err());
        let base = Baseline {
            tick: 5,
            entities: &[],
        };
        assert!(enc.encode(5, &[], Some(base), &mut out).is_err());
    }

    /// Small deterministic generator so the test needs no dependency.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn hostile_payloads_do_not_panic() {
        let base: Vec<EntityState> = (0..60).map(|i| entity(i, i as f32, 1.0, 0.2)).collect();
        let current: Vec<EntityState> = (5..70).map(|i| entity(i, i as f32 + 0.5, 1.5, 0.3)).collect();
        let b = Baseline {
            tick: 1,
            entities: &base,
        };
        let mut valid = Vec::new();
        DeltaEncoder::default()
            .encode(2, &current, Some(b), &mut valid)
            .expect("encode");
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut out = Vec::new();
        for _ in 0..20_000 {
            let len = (next(&mut state) % 64) as usize;
            let random: Vec<u8> = (0..len).map(|_| next(&mut state) as u8).collect();
            let _ = decode_delta(&random, 2, Some(b), &mut out);
            let mut corrupted = valid.clone();
            let at = (next(&mut state) as usize) % corrupted.len();
            corrupted[at] ^= 1 << (next(&mut state) % 8);
            if decode_delta(&corrupted, 2, Some(b), &mut out).is_ok() {
                assert!(out.len() <= MAX_ENTITIES);
            }
        }
    }

    #[test]
    fn an_id_gap_that_overflows_is_refused() {
        // Baseline ids use the high bits, so the previous id's high part is non-zero when the huge gap is added.
        let base = vec![entity(1 << 16, 1.0, 0.0, 0.0), entity(2 << 16, 2.0, 0.0, 0.0)];
        let b = Baseline {
            tick: 1,
            entities: &base,
        };
        // Age 1, two removed: (gap 1, low 0), then (gap of about u64::MAX, low 0).
        let mut payload = vec![1, 2, 1, 0];
        payload.extend_from_slice(&[0xff; 9]);
        payload.extend_from_slice(&[0x01, 0, 0, 0]);
        let mut out = Vec::new();
        assert_eq!(decode_delta(&payload, 2, Some(b), &mut out), Err(DecodeError::OutOfRange));
        // A tenth varint byte above 1 is refused too, rather than losing its bits.
        let mut payload = vec![1, 2, 1, 0];
        payload.extend_from_slice(&[0xff; 9]);
        payload.extend_from_slice(&[0x02, 0, 0, 0]);
        assert_eq!(decode_delta(&payload, 2, Some(b), &mut out), Err(DecodeError::OutOfRange));
    }

    #[test]
    fn the_largest_snapshot_fits_in_its_fragments() {
        // Every entity of the baseline removed and as many others added, all at the ends of their ranges.
        let far = |id: u32, sign: f32| EntityState {
            id,
            position: Position::from_cm([8_388_607 * sign as i32; 3]).expect("position"),
            velocity: Velocity { cm_per_s: [i16::MAX * sign as i16; 3] },
            rotation: Rotation::from_quaternion([0.5, -0.5, 0.5, 0.5]).expect("rotation"),
        };
        let n = MAX_ENTITIES as u32;
        let base: Vec<EntityState> = (0..n).map(|i| far((i * 2) << 16 | 0xffff, 1.0)).collect();
        let current: Vec<EntityState> = (0..n).map(|i| far((i * 2 + 1) << 16 | 0xfffe, -1.0)).collect();
        let b = Baseline {
            tick: 1,
            entities: &base,
        };
        let mut payload = Vec::new();
        DeltaEncoder::default()
            .encode(2, &current, Some(b), &mut payload)
            .expect("encode");
        assert!(fragments(&payload).is_ok(), "{} bytes", payload.len());
        let mut out = Vec::new();
        decode_delta(&payload, 2, Some(b), &mut out).expect("decode");
        assert_eq!(out, current);
        // The same entities changed instead, against a baseline at the other end of every range.
        let moved: Vec<EntityState> = base.iter().map(|e| far(e.id, -1.0)).collect();
        payload.clear();
        DeltaEncoder::default()
            .encode(2, &moved, Some(b), &mut payload)
            .expect("encode");
        assert!(fragments(&payload).is_ok(), "{} bytes", payload.len());
    }

    #[test]
    fn fragments_reassemble_only_when_complete() {
        let payload: Vec<u8> = (0..(FRAGMENT_PAYLOAD * 2 + 10)).map(|i| i as u8).collect();
        let parts: Vec<(u8, Vec<u8>)> = fragments(&payload)
            .expect("fragments")
            .map(|(f, p)| (f, p.to_vec()))
            .collect();
        assert_eq!(parts.len(), 3);
        let body = |i: usize| [&[parts[i].0][..], &parts[i].1].concat();

        let mut r = Reassembler::new();
        // Out of order, with a duplicate, completes once.
        assert_eq!(r.push(7, &body(2)), Ok(None));
        assert_eq!(r.push(7, &body(0)), Ok(None));
        assert_eq!(r.push(7, &body(0)), Ok(None));
        assert_eq!(r.push(7, &body(1)).expect("push"), Some(&payload[..]));
        assert_eq!(r.push(7, &body(1)), Ok(None));
        // A tick with a part lost is never delivered; a newer one replaces it, and older parts are ignored.
        assert_eq!(r.push(8, &body(0)), Ok(None));
        assert_eq!(r.push(8, &body(1)), Ok(None));
        assert_eq!(r.push(9, &body(0)), Ok(None));
        assert_eq!(r.push(8, &body(2)), Ok(None));
        assert_eq!(r.push(9, &body(1)), Ok(None));
        assert_eq!(r.push(9, &body(2)).expect("push"), Some(&payload[..]));
        // A fragment byte that disagrees with the tick's count is an error.
        assert_eq!(r.push(10, &body(0)), Ok(None));
        assert!(r.push(10, &[0x10, 1, 2]).is_err());
        assert!(r.push(11, &[0x21]).is_err(), "index beyond count");
        assert!(fragments(&vec![0; FRAGMENT_PAYLOAD * MAX_FRAGMENTS + 1]).is_err());
        let one: Vec<_> = fragments(&[]).expect("empty").collect();
        assert_eq!(one, vec![(0, &[][..])]);
    }
}
