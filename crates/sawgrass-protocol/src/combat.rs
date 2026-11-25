//! Shots and the hits they report (ADR 0006, section 5): the shooter's client tells the server what it hit, and the
//! server decides. One [`Shot`] is one pull of the trigger, so a shotgun's pellets are one shot with several hits,
//! and a shot that hit nothing still counts against the weapon's fire rate.
//!
//! Wire layout, after the event code: the snapshot tick the shooter drew its targets from (`u32`), how far its
//! render time was past that tick toward the next (`u8`, out of 256; NET-22), the weapon ([`AssetId`], `u32`), the
//! shooter's position (9 bytes), the number of hits (`u8`, at most [`MAX_HITS`]), then each
//! hit: the target's entity id (`u32`), the body zone (`u8`, [`Zone`]) and the hit point (9 bytes). All integers
//! are little-endian.

use crate::entity::AssetId;
use crate::quant::Position;
use crate::{DecodeError, Reader, Writer};

/// Most hits one shot reports: enough for a shotgun's pellets.
pub const MAX_HITS: usize = 16;

/// Where a ped was hit. A hit on a vehicle or an object reports [`Zone::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Zone {
    /// The head.
    Head = 0,
    /// The torso.
    Torso = 1,
    /// An arm.
    Arm = 2,
    /// A leg.
    Leg = 3,
    /// Anything else, and every hit on something that is not a ped.
    #[default]
    Other = 4,
}

impl TryFrom<u8> for Zone {
    type Error = DecodeError;

    fn try_from(v: u8) -> Result<Self, DecodeError> {
        Ok(match v {
            0 => Self::Head,
            1 => Self::Torso,
            2 => Self::Arm,
            3 => Self::Leg,
            4 => Self::Other,
            _ => return Err(DecodeError::OutOfRange),
        })
    }
}

/// One hit of a shot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hit {
    /// The entity hit.
    pub target: u32,
    /// Where on it.
    pub zone: Zone,
    /// Where the shooter's client saw the hit.
    pub point: Position,
}

impl Default for Hit {
    fn default() -> Self {
        Self {
            target: 0,
            zone: Zone::Other,
            // 0 cm is within range, so this cannot fail.
            point: Position::from_cm([0; 3]).unwrap_or_else(|_| unreachable!()),
        }
    }
}

/// One shot and what it hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Shot {
    /// The snapshot tick the shooter's client drew the targets from when it fired: the older of the two it
    /// interpolated between, at its render time (NET-22). The server checks the hits against where the targets
    /// were then.
    pub tick: u32,
    /// How far the render time was from `tick` toward `tick + 1`, out of 256: 0 is `tick` itself.
    pub frac: u8,
    /// The weapon fired.
    pub weapon: AssetId,
    /// Where the shooter was when it fired, as its client saw it.
    pub from: Position,
    hits: [Hit; MAX_HITS],
    count: u8,
}

impl Shot {
    /// Size of one hit on the wire.
    const HIT_SIZE: usize = 4 + 1 + Position::SIZE;

    /// A shot with no hits.
    #[must_use]
    pub fn new(tick: u32, weapon: AssetId, from: Position) -> Self {
        Self {
            tick,
            frac: 0,
            weapon,
            from,
            hits: [Hit::default(); MAX_HITS],
            count: 0,
        }
    }

    /// Adds a hit; `false` if the shot already has [`MAX_HITS`].
    pub fn push(&mut self, hit: Hit) -> bool {
        let Some(slot) = self.hits.get_mut(usize::from(self.count)) else {
            return false;
        };
        *slot = hit;
        self.count += 1;
        true
    }

    /// The hits, in the order the client reported them.
    #[must_use]
    pub fn hits(&self) -> &[Hit] {
        &self.hits[..usize::from(self.count)]
    }

    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.tick.to_le_bytes());
        out.push(self.frac);
        out.extend_from_slice(&self.weapon.0.to_le_bytes());
        put_position(self.from, out);
        out.push(self.count);
        for hit in self.hits() {
            out.extend_from_slice(&hit.target.to_le_bytes());
            out.push(hit.zone as u8);
            put_position(hit.point, out);
        }
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let tick = u32::from_le_bytes(r.array()?);
        let [frac] = r.array::<1>()?;
        let weapon = AssetId(u32::from_le_bytes(r.array()?));
        let from = Position::read(r)?;
        let [count] = r.array::<1>()?;
        if usize::from(count) > MAX_HITS {
            return Err(DecodeError::OutOfRange);
        }
        if r.input.len() < usize::from(count) * Self::HIT_SIZE {
            return Err(DecodeError::Truncated);
        }
        let mut shot = Self::new(tick, weapon, from);
        shot.frac = frac;
        for _ in 0..count {
            let target = u32::from_le_bytes(r.array()?);
            let [zone] = r.array::<1>()?;
            let point = Position::read(r)?;
            shot.push(Hit {
                target,
                zone: Zone::try_from(zone)?,
                point,
            });
        }
        Ok(shot)
    }
}

fn put_position(p: Position, out: &mut Vec<u8>) {
    let mut buf = [0; Position::SIZE];
    // The buffer is exactly one position long, so the writer cannot fail.
    if let Ok(mut w) = Writer::new(&mut buf, Position::SIZE) {
        p.write(&mut w);
    }
    out.extend_from_slice(&buf);
}
