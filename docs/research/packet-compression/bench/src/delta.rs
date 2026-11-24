//! Delta encoding of a snapshot against an older one the client has acknowledged (its baseline), the technique
//! of Quake 3 and most action-game servers since.
//!
//! Wire layout, every integer a LEB128 varint as postcard writes them, signed ones zigzagged:
//!
//! ```text
//! kind (0x10) | seq | tick | baseline age in ticks (0: none, every entity is new)
//! removed: count, then id gaps                        entities in the baseline that left the view
//! changed: count, then per entity: id gap, mask, the changed fields as differences from the baseline
//! added:   count, then per entity: id gap, position, velocity, rotation in full
//! ```
//!
//! Entities in both with nothing changed are not sent at all. Ids are ascending in each list, so each is sent as
//! the gap from the one before. Mask bits 0 to 2 are the position axes, 3 to 5 the velocity axes, bit 6 a
//! rotation whose largest component is unchanged (three 10-bit differences follow), bit 7 a rotation sent whole.
//! A lost snapshot breaks nothing: the next one is relative to whatever the client last acknowledged.

use crate::model::{Entity, Snapshot};

pub const KIND: u8 = 0x10;
/// More entities than this in one snapshot is treated as hostile.
const MAX_ENTITIES: usize = 1024;
const ROT_SHIFT: u32 = 10;
const ROT_MASK: u32 = (1 << ROT_SHIFT) - 1;

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

/// Reusable buffers, as a gateway would keep per thread.
#[derive(Default)]
pub struct Encoder {
    removed: Vec<u8>,
    changed: Vec<u8>,
    added: Vec<u8>,
}

impl Encoder {
    /// Encodes `cur` against `base` (`None`: no acknowledged baseline, everything is sent as new).
    pub fn encode(&mut self, cur: &Snapshot, base: Option<&Snapshot>, out: &mut Vec<u8>) {
        out.clear();
        self.removed.clear();
        self.changed.clear();
        self.added.clear();
        let empty = Vec::new();
        let base_entities = base.map_or(&empty, |b| &b.entities);
        let (mut n_removed, mut n_changed, mut n_added) = (0u64, 0u64, 0u64);
        let (mut last_removed, mut last_changed, mut last_added) = (0u32, 0u32, 0u32);
        let (mut i, mut j) = (0, 0);
        let (a, b) = (base_entities.as_slice(), cur.entities.as_slice());
        while i < a.len() || j < b.len() {
            let ida = a.get(i).map_or(u32::MAX, |e| e.id);
            let idb = b.get(j).map_or(u32::MAX, |e| e.id);
            if ida < idb {
                put(&mut self.removed, u64::from(ida - last_removed));
                last_removed = ida;
                n_removed += 1;
                i += 1;
            } else if idb < ida {
                let e = &b[j];
                put(&mut self.added, u64::from(idb - last_added));
                last_added = idb;
                for v in e.pos {
                    put_signed(&mut self.added, i64::from(v));
                }
                for v in e.vel {
                    put_signed(&mut self.added, i64::from(v));
                }
                put(&mut self.added, u64::from(e.rot));
                n_added += 1;
                j += 1;
            } else {
                let (old, new) = (&a[i], &b[j]);
                let mut mask = 0u8;
                for k in 0..3 {
                    if old.pos[k] != new.pos[k] {
                        mask |= 1 << k;
                    }
                    if old.vel[k] != new.vel[k] {
                        mask |= 1 << (3 + k);
                    }
                }
                let (ro, rn) = (rot_parts(old.rot), rot_parts(new.rot));
                if ro != rn {
                    mask |= if ro[0] == rn[0] { 1 << 6 } else { 1 << 7 };
                }
                if mask != 0 {
                    let c = &mut self.changed;
                    put(c, u64::from(idb - last_changed));
                    last_changed = idb;
                    c.push(mask);
                    for k in 0..3 {
                        if mask & (1 << k) != 0 {
                            put_signed(c, i64::from(new.pos[k]) - i64::from(old.pos[k]));
                        }
                    }
                    for k in 0..3 {
                        if mask & (1 << (3 + k)) != 0 {
                            put_signed(c, i64::from(new.vel[k]) - i64::from(old.vel[k]));
                        }
                    }
                    if mask & (1 << 6) != 0 {
                        for k in 1..4 {
                            put_signed(c, rn[k] - ro[k]);
                        }
                    }
                    if mask & (1 << 7) != 0 {
                        put(c, u64::from(new.rot));
                    }
                    n_changed += 1;
                }
                i += 1;
                j += 1;
            }
        }
        out.push(KIND);
        put(out, u64::from(cur.seq));
        put(out, u64::from(cur.tick));
        put(out, base.map_or(0, |b| u64::from(cur.tick - b.tick)));
        put(out, n_removed);
        out.extend_from_slice(&self.removed);
        put(out, n_changed);
        out.extend_from_slice(&self.changed);
        put(out, n_added);
        out.extend_from_slice(&self.added);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }

    fn get(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f).checked_shl(shift)?;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn get_signed(&mut self) -> Option<i64> {
        let v = self.get()?;
        Some((v >> 1) as i64 ^ -((v & 1) as i64))
    }

    fn count(&mut self) -> Option<usize> {
        let n = usize::try_from(self.get()?).ok()?;
        // Each entry takes at least one byte, so a count larger than what is left is a lie.
        (n <= MAX_ENTITIES && n <= self.0.len()).then_some(n)
    }

    /// The next id in an ascending list.
    fn id(&mut self, last: &mut Option<u32>) -> Option<u32> {
        let gap = u32::try_from(self.get()?).ok()?;
        let id = match *last {
            None => gap,
            Some(l) if gap > 0 => l.checked_add(gap)?,
            Some(_) => return None,
        };
        *last = Some(id);
        Some(id)
    }
}

fn narrow<T: TryFrom<i64>>(v: i64) -> Option<T> {
    T::try_from(v).ok()
}

/// Decodes a delta packet against `base`, the snapshot it names. Every count, id and value is checked: an id that
/// is not in the baseline, a duplicate, a value out of range or a byte left over rejects the packet.
pub fn decode(bytes: &[u8], base: Option<&Snapshot>) -> Option<Snapshot> {
    let mut r = Reader(bytes);
    if r.byte()? != KIND {
        return None;
    }
    let seq = u16::try_from(r.get()?).ok()?;
    let tick = u32::try_from(r.get()?).ok()?;
    let age = r.get()?;
    let empty = Vec::new();
    let base_entities = match (age, base) {
        (0, _) => &empty,
        (age, Some(b)) if u64::from(tick.checked_sub(b.tick)?) == age => &b.entities,
        _ => return None,
    };

    let n = r.count()?;
    let mut removed = Vec::with_capacity(n);
    let mut last = None;
    for _ in 0..n {
        removed.push(r.id(&mut last)?);
    }

    let n = r.count()?;
    let mut changed: Vec<(u32, u8, [i64; 10])> = Vec::with_capacity(n);
    let mut last = None;
    for _ in 0..n {
        let id = r.id(&mut last)?;
        let mask = r.byte()?;
        if mask == 0 || mask & 0xC0 == 0xC0 {
            return None;
        }
        let mut d = [0i64; 10];
        for (k, slot) in d.iter_mut().take(6).enumerate() {
            if mask & (1 << k) != 0 {
                *slot = r.get_signed()?;
            }
        }
        if mask & (1 << 6) != 0 {
            for slot in &mut d[6..9] {
                *slot = r.get_signed()?;
            }
        }
        if mask & (1 << 7) != 0 {
            d[9] = i64::from(u32::try_from(r.get()?).ok()?);
        }
        changed.push((id, mask, d));
    }

    let n = r.count()?;
    let mut added = Vec::with_capacity(n);
    let mut last = None;
    for _ in 0..n {
        let id = r.id(&mut last)?;
        let mut pos = [0i32; 3];
        for v in &mut pos {
            *v = narrow(r.get_signed()?)?;
        }
        let mut vel = [0i16; 3];
        for v in &mut vel {
            *v = narrow(r.get_signed()?)?;
        }
        let rot = u32::try_from(r.get()?).ok()?;
        added.push(Entity { id, pos, vel, rot });
    }
    if !r.0.is_empty() {
        return None;
    }

    // Walk the baseline once, dropping the removed and applying the changed; both must name entities in it.
    let mut kept = Vec::with_capacity(base_entities.len() + added.len());
    let (mut ri, mut ci) = (0, 0);
    for old in base_entities {
        if removed.get(ri) == Some(&old.id) {
            ri += 1;
            continue;
        }
        let mut e = *old;
        if let Some(&(id, mask, d)) = changed.get(ci)
            && id == old.id
        {
            ci += 1;
            for k in 0..3 {
                if mask & (1 << k) != 0 {
                    e.pos[k] = narrow(i64::from(e.pos[k]).checked_add(d[k])?)?;
                }
                if mask & (1 << (3 + k)) != 0 {
                    e.vel[k] = narrow(i64::from(e.vel[k]).checked_add(d[3 + k])?)?;
                }
            }
            if mask & (1 << 6) != 0 {
                let mut p = rot_parts(e.rot);
                for k in 1..4 {
                    p[k] = p[k].checked_add(d[5 + k])?;
                    if !(0..=i64::from(ROT_MASK)).contains(&p[k]) {
                        return None;
                    }
                }
                e.rot = (p[0] << (3 * ROT_SHIFT) | p[1] << (2 * ROT_SHIFT) | p[2] << ROT_SHIFT | p[3]) as u32;
            }
            if mask & (1 << 7) != 0 {
                e.rot = d[9] as u32;
            }
        }
        kept.push(e);
    }
    if ri != removed.len() || ci != changed.len() {
        return None;
    }
    // Merge in the new entities; an id already present is an error.
    let mut entities = Vec::with_capacity(kept.len() + added.len());
    let (mut i, mut j) = (0, 0);
    while i < kept.len() || j < added.len() {
        match (kept.get(i), added.get(j)) {
            (Some(a), Some(b)) if a.id == b.id => return None,
            (Some(a), Some(b)) if a.id < b.id => {
                entities.push(*a);
                i += 1;
            }
            (Some(a), None) => {
                entities.push(*a);
                i += 1;
            }
            (_, Some(b)) => {
                entities.push(*b);
                j += 1;
            }
            (None, None) => unreachable!(),
        }
    }
    if entities.len() > MAX_ENTITIES {
        return None;
    }
    Some(Snapshot { seq, tick, entities })
}
