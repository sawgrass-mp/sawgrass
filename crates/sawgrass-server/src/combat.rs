//! What the server accepts from a shot, and the damage it does (ADR 0006, section 5). The shooter's client reports
//! its hits; the server checks each against what it knows and computes the damage itself.
//!
//! A hit is accepted only if the shooter is alive and holds the weapon, the weapon's fire rate allows the shot, the
//! target is in the shooter's visible set and within the weapon's range, the shot's tick is at most
//! [`REWIND`] old, and the target was within [`HIT_TOLERANCE`] of the hit point at that tick. `net.rs` runs these
//! checks; this module holds the figures and the arithmetic.
//!
//! - *Assumption:* every weapon has the figures of [`WeaponRules::default`] until the game's are known.
//! - *Assumption:* "holds the weapon" means the weapon in the shooter's ped block, which its client sets; the
//!   server checks it against an inventory once the roleplay base has one.
//! - *Accepted temporary limitation* (the maintainer's choice of 2026-11-25): no line-of-sight check, as the server
//!   has no map geometry yet. A hit through a wall is possible until collision data arrives.

use std::time::Duration;

use sawgrass_protocol::combat::Zone;
use sawgrass_protocol::ped::{Motion, PedBlock};
use sawgrass_protocol::vehicle::VehicleBlock;

/// Oldest snapshot a shot may be checked against: 250 ms, a placeholder for the round trip plus the client's
/// interpolation delay of a player far from the server.
pub const REWIND: Duration = Duration::from_millis(250);

/// Farthest the hit point may be from where the server had the target at the shot's tick, in metres: a ped's
/// size plus the 1 cm quantization, and the movement within one tick at a run.
pub const HIT_TOLERANCE: f32 = 2.0;

/// A weapon's figures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeaponRules {
    /// Damage of one hit on the torso, in health points.
    pub damage: u8,
    /// Farthest a hit may be from the shooter, in metres.
    pub range: f32,
    /// Shortest time between two shots.
    pub interval: Duration,
}

impl Default for WeaponRules {
    /// A placeholder for every weapon: 25 points, 150 m, 900 rounds a minute.
    fn default() -> Self {
        Self {
            damage: 25,
            range: 150.0,
            interval: Duration::from_micros(66_667),
        }
    }
}

/// How much a hit on `zone` does, as a multiple of the torso's: placeholders until the game's are known.
#[must_use]
pub fn zone_factor(zone: Zone) -> f32 {
    match zone {
        Zone::Head => 4.0,
        Zone::Torso => 1.0,
        Zone::Arm | Zone::Leg => 0.6,
        Zone::Other => 0.5,
    }
}

/// Share of a hit's damage that armour takes while it lasts; the rest reaches health.
pub const ARMOUR_SHARE: f32 = 0.6;

/// The ped after a hit of `damage` points on `zone`: armour takes [`ARMOUR_SHARE`] of it while it lasts, health
/// the rest. At zero health the ped is dead; a dead ped takes no more damage. Returns whether this hit killed it.
pub fn hit_ped(ped: &mut PedBlock, damage: u8, zone: Zone) -> bool {
    if ped.motion == Motion::Dead {
        return false;
    }
    let total = (f32::from(damage) * zone_factor(zone)).round();
    let to_armour = (total * ARMOUR_SHARE).min(f32::from(ped.armour));
    let to_health = total - to_armour;
    ped.armour -= to_armour as u8;
    ped.health = (f32::from(ped.health) - to_health).max(0.0) as u8;
    if ped.health == 0 {
        ped.motion = Motion::Dead;
        ped.vehicle = 0;
        ped.seat = 0;
        return true;
    }
    false
}

/// The vehicle after a hit of `damage` points: its body takes it, as a server-checked event that also counts for
/// a vehicle that belongs to a player (ADR 0006, section 5).
pub fn hit_vehicle(vehicle: &mut VehicleBlock, damage: u8) {
    vehicle.body = vehicle.body.saturating_sub(damage);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ped(health: u8, armour: u8) -> PedBlock {
        PedBlock {
            health,
            armour,
            ..PedBlock::default()
        }
    }

    #[test]
    fn armour_takes_its_share_then_health_goes_to_zero_and_the_ped_dies() {
        let mut p = ped(100, 10);
        // 25 on the torso: armour takes 10 of its 15, health the other 15.
        assert!(!hit_ped(&mut p, 25, Zone::Torso));
        assert_eq!((p.health, p.armour), (85, 0));
        // 25 on a leg is 15.
        assert!(!hit_ped(&mut p, 25, Zone::Leg));
        assert_eq!(p.health, 70);
        // A head hit is 100: dead.
        assert!(hit_ped(&mut p, 25, Zone::Head));
        assert_eq!((p.health, p.motion), (0, Motion::Dead));
        // Nothing more happens to the dead.
        assert!(!hit_ped(&mut p, 25, Zone::Head));
    }

    #[test]
    fn a_hit_on_a_vehicle_lowers_its_body() {
        let mut v = VehicleBlock::INTACT;
        hit_vehicle(&mut v, 25);
        assert_eq!((v.body, v.engine), (230, 255));
        v.body = 10;
        hit_vehicle(&mut v, 25);
        assert_eq!(v.body, 0);
    }
}
