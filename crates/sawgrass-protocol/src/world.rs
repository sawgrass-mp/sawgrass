//! World state the server owns (ADR 0006, section 7): the clock, the weather and doors. A joining client gets
//! the [`Environment`] and every [`Door`], then each change as an event.
//!
//! Wire layouts, after the event code, little-endian:
//!
//! - [`Environment`]: the tick it holds at (`u32`), the game minute of the day then (`u16`, below 1440), the rate
//!   in hundredths of a game minute per real second (`u16`), the current and the next weather ([`AssetId`], `u32`
//!   each) and how far the transition between them is (`u8`, 0 none to 255 done). 17 bytes.
//! - [`Door`]: its [`AssetId`] (`u32`), locked (`u8`, 0 or 1) and how far it is open (`u8`, 0 shut to 255 wide).
//!   6 bytes.

use crate::entity::AssetId;
use crate::{DecodeError, Reader};

/// Game minutes in a day.
pub const MINUTES_PER_DAY: u16 = 24 * 60;

/// The clock and the weather, as of a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Environment {
    /// The tick the values hold at; a client advances the clock from there by `rate`.
    pub tick: u32,
    /// The game minute of the day at `tick`, below [`MINUTES_PER_DAY`].
    pub minute: u16,
    /// Game minutes per real second, in hundredths: 200 is two game minutes a second. 0 stops the clock.
    pub rate: u16,
    /// The weather now.
    pub weather: AssetId,
    /// The weather it is turning into; the same as `weather` when it is not changing.
    pub next_weather: AssetId,
    /// How far the change from `weather` to `next_weather` is, 0 none to 255 done.
    pub transition: u8,
}

impl Environment {
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.tick.to_le_bytes());
        out.extend_from_slice(&self.minute.to_le_bytes());
        out.extend_from_slice(&self.rate.to_le_bytes());
        out.extend_from_slice(&self.weather.0.to_le_bytes());
        out.extend_from_slice(&self.next_weather.0.to_le_bytes());
        out.push(self.transition);
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let tick = u32::from_le_bytes(r.array()?);
        let minute = u16::from_le_bytes(r.array()?);
        if minute >= MINUTES_PER_DAY {
            return Err(DecodeError::OutOfRange);
        }
        Ok(Self {
            tick,
            minute,
            rate: u16::from_le_bytes(r.array()?),
            weather: AssetId(u32::from_le_bytes(r.array()?)),
            next_weather: AssetId(u32::from_le_bytes(r.array()?)),
            transition: r.array::<1>()?[0],
        })
    }
}

/// A door a resource registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Door {
    /// The door's id; the client module's table maps it to the game's door (assumption A4).
    pub id: AssetId,
    /// Whether it is locked.
    pub locked: bool,
    /// How far it is open, 0 shut to 255 wide.
    pub open: u8,
}

impl Door {
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.0.to_le_bytes());
        out.push(u8::from(self.locked));
        out.push(self.open);
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let id = AssetId(u32::from_le_bytes(r.array()?));
        let [locked, open] = r.array::<2>()?;
        if locked > 1 {
            return Err(DecodeError::OutOfRange);
        }
        Ok(Self {
            id,
            locked: locked == 1,
            open,
        })
    }
}
