//! The vehicle block of [ADR 0006](../../../docs/adr/0006-entity-sync-before-the-game.md), section 4: what the
//! owner's driver does and the coarse state other clients need to show and extrapolate a vehicle.
//!
//! A [`VehicleBlock`] travels as the block fields of a vehicle's [`Info`](crate::entity::Info), field `i` being
//! entry `i` of the table, and in an [`OwnedState`](crate::game::OwnedState) from the client that simulates it.
//!
//! | Field | Value |
//! |---|---|
//! | 0 | controls: steer (`i8`, full left `-127`) in the low byte, throttle (`i8`, reverse below 0) in the next, brake (`u8`) in the third |
//! | 1, 2, 3 | angular velocity about x, y and z, `i16` in 0.01 rad/s, as its 16 bits |
//! | 4 | flags, 16 bits: [`VehicleBlock::ENGINE`] and the others below, then one bit per door from [`VehicleBlock::DOOR_0`] |
//! | 5 | damage: engine health (`u8`, 255 intact) in the low byte, body health in the next, burst tyres as bits in the third |
//!
//! Deformation is not sent (ADR 0006, assumption A3).
//!
//! **Who may lower the damage fields** (ADR 0006, section 5, the maintainer's choice of 2026-11-25): the server
//! takes field 5 from the simulating owner's [`OwnedState`](crate::game::OwnedState) only for a vehicle that belongs
//! to nobody as property, and then only as a decrease under a cap. For a vehicle that belongs to a player, the owner's
//! value is ignored and the server sends its own, changed only by events it checked (an accepted hit report, a
//! script). A decoder reads the first [`VEHICLE_FIELDS`] fields and ignores
//! any after them; fields wider than their table entry are refused.

use crate::DecodeError;

/// Fields in a vehicle block.
pub const VEHICLE_FIELDS: usize = 6;

/// What a vehicle is doing. [`Default`] is a parked, wrecked vehicle, as a field-less entity decodes; a spawned one
/// starts from [`VehicleBlock::INTACT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct VehicleBlock {
    /// Steering, full left `-127`, full right `127`.
    pub steer: i8,
    /// Throttle, reverse below 0.
    pub throttle: i8,
    /// Brake, `0` to `255`.
    pub brake: u8,
    /// Angular velocity about x, y and z, in 0.01 rad/s.
    pub angular_velocity: [i16; 3],
    /// The flag bits below.
    pub flags: u16,
    /// Engine health, `255` intact. From a simulating owner, taken only for a vehicle nobody owns as property and
    /// only as a decrease (see the module's notes).
    pub engine: u8,
    /// Body health, `255` intact. As `engine`.
    pub body: u8,
    /// Burst tyres, one bit each. As `engine`: an owner may burst a tyre only of a vehicle nobody owns.
    pub burst_tyres: u8,
}

impl VehicleBlock {
    /// The engine runs.
    pub const ENGINE: u16 = 1;
    /// Lights on.
    pub const LIGHTS: u16 = 1 << 1;
    /// High beam on.
    pub const HIGH_BEAM: u16 = 1 << 2;
    /// Siren on.
    pub const SIREN: u16 = 1 << 3;
    /// Horn sounding.
    pub const HORN: u16 = 1 << 4;
    /// Left indicator.
    pub const INDICATOR_LEFT: u16 = 1 << 5;
    /// Right indicator.
    pub const INDICATOR_RIGHT: u16 = 1 << 6;
    /// The first door is open; door `n` is `DOOR_0 << n`, up to 8 doors.
    pub const DOOR_0: u16 = 1 << 8;

    /// A stopped vehicle with its engine off and no damage.
    pub const INTACT: Self = Self {
        steer: 0,
        throttle: 0,
        brake: 0,
        angular_velocity: [0; 3],
        flags: 0,
        engine: 255,
        body: 255,
        burst_tyres: 0,
    };

    /// The block's fields, in the order of the module's table.
    #[must_use]
    pub fn to_fields(&self) -> [u32; VEHICLE_FIELDS] {
        let [x, y, z] = self.angular_velocity.map(|v| u32::from(v as u16));
        [
            u32::from(self.steer as u8) | u32::from(self.throttle as u8) << 8 | u32::from(self.brake) << 16,
            x,
            y,
            z,
            u32::from(self.flags),
            u32::from(self.engine) | u32::from(self.body) << 8 | u32::from(self.burst_tyres) << 16,
        ]
    }

    /// Reads a block from its fields. Fewer than [`VEHICLE_FIELDS`] fields, or a field wider than its entry, is
    /// [`DecodeError::OutOfRange`]; fields after the known ones are ignored.
    pub fn from_fields(fields: &[u32]) -> Result<Self, DecodeError> {
        let f: &[u32; VEHICLE_FIELDS] = fields.first_chunk().ok_or(DecodeError::OutOfRange)?;
        let fits = |v: u32, bits: u32| {
            if v >> bits == 0 {
                Ok(v)
            } else {
                Err(DecodeError::OutOfRange)
            }
        };
        let controls = fits(f[0], 24)?;
        let damage = fits(f[5], 24)?;
        let angular = |v: u32| fits(v, 16).map(|v| v as u16 as i16);
        Ok(Self {
            steer: controls as u8 as i8,
            throttle: (controls >> 8) as u8 as i8,
            brake: (controls >> 16) as u8,
            angular_velocity: [angular(f[1])?, angular(f[2])?, angular(f[3])?],
            flags: fits(f[4], 16)? as u16,
            engine: damage as u8,
            body: (damage >> 8) as u8,
            burst_tyres: (damage >> 16) as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vehicle_block_round_trips_through_its_fields() {
        let block = VehicleBlock {
            steer: -127,
            throttle: -40,
            brake: 255,
            angular_velocity: [i16::MIN, -1, i16::MAX],
            flags: VehicleBlock::ENGINE | VehicleBlock::SIREN | VehicleBlock::DOOR_0 << 7,
            engine: 10,
            body: 0,
            burst_tyres: 0b1010,
        };
        assert_eq!(VehicleBlock::from_fields(&block.to_fields()), Ok(block));
        assert_eq!(
            VehicleBlock::from_fields(&VehicleBlock::INTACT.to_fields()),
            Ok(VehicleBlock::INTACT)
        );
        let mut longer = block.to_fields().to_vec();
        longer.push(7);
        assert_eq!(VehicleBlock::from_fields(&longer), Ok(block));
    }

    #[test]
    fn wide_fields_are_refused() {
        let fields = VehicleBlock::INTACT.to_fields();
        assert_eq!(
            VehicleBlock::from_fields(&fields[..5]),
            Err(DecodeError::OutOfRange)
        );
        for (i, bad) in [
            (0, 1 << 24),
            (1, 1 << 16),
            (3, u32::MAX),
            (4, 1 << 16),
            (5, 1 << 24),
        ] {
            let mut f = fields;
            f[i] = bad;
            assert_eq!(
                VehicleBlock::from_fields(&f),
                Err(DecodeError::OutOfRange),
                "field {i}"
            );
        }
    }
}
