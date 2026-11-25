//! The ped block of [ADR 0006](../../../docs/adr/0006-entity-sync-before-the-game.md), section 3: what a player or
//! NPC is doing, as an intent the receiving client's game replays, never bones or clip phases.
//!
//! A [`PedBlock`] travels as the block fields of an [`Info`](crate::entity::Info) in snapshots, field `i` being
//! entry `i` of the table below, and after a player's kinematics in its client state
//! ([`ClientState::encode_with_ped`](crate::game::ClientState::encode_with_ped)).
//!
//! | Field | Value |
//! |---|---|
//! | 0 | [`Motion`] |
//! | 1 | flags: [`PedBlock::CROUCHED`], [`PedBlock::AIMING`], [`PedBlock::FIRING`], [`PedBlock::RELOADING`] |
//! | 2 | weapon in hand, an [`AssetId`], `0` unarmed |
//! | 3 | aim: yaw relative to the body in the low byte (256 steps a turn), pitch in the next (`0` straight down, `128` level, `255` straight up) |
//! | 4 | the vehicle the ped sits in, by entity id; meaningful only with a seat |
//! | 5 | seat: `0` none, otherwise the seat index plus one |
//! | 6, 7, 8 | full-body action: [`AssetId`], start tick, flags |
//! | 9, 10, 11 | upper-body action: [`AssetId`], start tick, flags |
//! | 12, 13 | health and armour, set by the server only |
//!
//! Byte-sized fields that hold more than a byte are refused. A decoder reads the first [`PED_FIELDS`] fields and
//! ignores any after them, which a later version may add.

use crate::DecodeError;
use crate::entity::AssetId;

/// Fields in a ped block.
pub const PED_FIELDS: usize = 14;

/// How a ped moves. The values are part of the wire format; values not listed here are reserved for what the game
/// turns out to need and are shown as [`Motion::Idle`] by a client that does not know them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Motion {
    /// Standing.
    #[default]
    Idle = 0,
    /// Walking.
    Walk = 1,
    /// Running.
    Run = 2,
    /// Sprinting.
    Sprint = 3,
    /// In a jump.
    Jump = 4,
    /// Falling.
    Fall = 5,
    /// Swimming.
    Swim = 6,
    /// Climbing or vaulting.
    Climb = 7,
    /// Ragdoll, after a hit or a fall.
    Ragdoll = 8,
    /// In cover.
    Cover = 9,
    /// Sitting in a vehicle; the seat fields say where.
    InVehicle = 10,
    /// Dead, set by the server only.
    Dead = 11,
}

impl Motion {
    /// The motion for a wire value; unknown values are [`Motion::Idle`], as ADR 0006 asks.
    #[must_use]
    pub fn from_wire(value: u8) -> Self {
        match value {
            1 => Self::Walk,
            2 => Self::Run,
            3 => Self::Sprint,
            4 => Self::Jump,
            5 => Self::Fall,
            6 => Self::Swim,
            7 => Self::Climb,
            8 => Self::Ragdoll,
            9 => Self::Cover,
            10 => Self::InVehicle,
            11 => Self::Dead,
            _ => Self::Idle,
        }
    }
}

/// An action the ped performs on top of its motion: an emote, a scripted scenario, an interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Action {
    /// What it is, `0` for none.
    pub action: AssetId,
    /// The server tick it started at, so a client that sees it late starts it at the right phase.
    pub start_tick: u32,
    /// [`Action::LOOP`], [`Action::INTERRUPTIBLE`] and bits reserved for later.
    pub flags: u8,
}

impl Action {
    /// The action repeats until replaced.
    pub const LOOP: u8 = 1;
    /// Movement may interrupt it.
    pub const INTERRUPTIBLE: u8 = 2;
}

/// What a ped is doing. [`Default`] is a standing, unarmed ped with no health, as a field-less entity decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PedBlock {
    /// How it moves.
    pub motion: Motion,
    /// The flag bits below.
    pub flags: u8,
    /// The weapon in hand, `0` unarmed.
    pub weapon: AssetId,
    /// Aim yaw relative to the body, 256 steps a turn.
    pub aim_yaw: u8,
    /// Aim pitch: `0` straight down, `128` level, `255` straight up.
    pub aim_pitch: u8,
    /// The vehicle it sits in, by entity id, when `seat` is not 0.
    pub vehicle: u32,
    /// `0` for none, otherwise the seat index plus one.
    pub seat: u8,
    /// The full-body action.
    pub full_body: Action,
    /// The upper-body action.
    pub upper_body: Action,
    /// Health, set by the server.
    pub health: u8,
    /// Armour, set by the server.
    pub armour: u8,
}

impl PedBlock {
    /// Crouched.
    pub const CROUCHED: u8 = 1;
    /// Aiming; the aim fields are meaningful.
    pub const AIMING: u8 = 2;
    /// Holding the trigger.
    pub const FIRING: u8 = 4;
    /// Reloading.
    pub const RELOADING: u8 = 8;

    /// The block's fields, in the order of the module's table.
    #[must_use]
    pub fn to_fields(&self) -> [u32; PED_FIELDS] {
        let a = |a: &Action| [a.action.0, a.start_tick, u32::from(a.flags)];
        let [f6, f7, f8] = a(&self.full_body);
        let [f9, f10, f11] = a(&self.upper_body);
        [
            self.motion as u32,
            u32::from(self.flags),
            self.weapon.0,
            u32::from(self.aim_yaw) | u32::from(self.aim_pitch) << 8,
            self.vehicle,
            u32::from(self.seat),
            f6,
            f7,
            f8,
            f9,
            f10,
            f11,
            u32::from(self.health),
            u32::from(self.armour),
        ]
    }

    /// Reads a block from its fields. Fewer than [`PED_FIELDS`] fields, or a byte-sized field above 255 (above
    /// 65,535 for the aim), is [`DecodeError::OutOfRange`]; fields after the known ones are ignored.
    pub fn from_fields(fields: &[u32]) -> Result<Self, DecodeError> {
        let f: &[u32; PED_FIELDS] = fields.first_chunk().ok_or(DecodeError::OutOfRange)?;
        let byte = |v: u32| u8::try_from(v).map_err(|_| DecodeError::OutOfRange);
        if f[3] > 0xffff {
            return Err(DecodeError::OutOfRange);
        }
        let action = |i: usize| -> Result<Action, DecodeError> {
            Ok(Action {
                action: AssetId(f[i]),
                start_tick: f[i + 1],
                flags: byte(f[i + 2])?,
            })
        };
        Ok(Self {
            motion: Motion::from_wire(byte(f[0])?),
            flags: byte(f[1])?,
            weapon: AssetId(f[2]),
            aim_yaw: f[3] as u8,
            aim_pitch: (f[3] >> 8) as u8,
            vehicle: f[4],
            seat: byte(f[5])?,
            full_body: action(6)?,
            upper_body: action(9)?,
            health: byte(f[12])?,
            armour: byte(f[13])?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PedBlock {
        PedBlock {
            motion: Motion::InVehicle,
            flags: PedBlock::AIMING | PedBlock::FIRING,
            weapon: AssetId(0xABCD_1234),
            aim_yaw: 200,
            aim_pitch: 140,
            vehicle: 7 << 16 | 3,
            seat: 2,
            full_body: Action {
                action: AssetId(u32::MAX),
                start_tick: 12_345,
                flags: Action::LOOP,
            },
            upper_body: Action::default(),
            health: 255,
            armour: 50,
        }
    }

    #[test]
    fn a_ped_block_round_trips_through_its_fields() {
        let block = sample();
        assert_eq!(PedBlock::from_fields(&block.to_fields()), Ok(block));
        let mut longer = block.to_fields().to_vec();
        longer.push(99);
        assert_eq!(PedBlock::from_fields(&longer), Ok(block));
    }

    #[test]
    fn out_of_range_fields_are_refused_and_unknown_motions_are_idle() {
        let fields = sample().to_fields();
        assert_eq!(
            PedBlock::from_fields(&fields[..PED_FIELDS - 1]),
            Err(DecodeError::OutOfRange)
        );
        for (i, bad) in [
            (0, 256),
            (1, 256),
            (3, 0x1_0000),
            (5, 300),
            (8, 256),
            (11, 256),
            (12, 256),
            (13, 999),
        ] {
            let mut f = fields;
            f[i] = bad;
            assert_eq!(
                PedBlock::from_fields(&f),
                Err(DecodeError::OutOfRange),
                "field {i}"
            );
        }
        let mut f = fields;
        f[0] = 200;
        assert_eq!(PedBlock::from_fields(&f).map(|b| b.motion), Ok(Motion::Idle));
    }
}
