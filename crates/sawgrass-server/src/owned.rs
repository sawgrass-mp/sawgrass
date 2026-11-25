//! What the server takes from an owner's state of a vehicle or NPC (ADR 0006, sections 4 and 5).
//!
//! The owner simulates the entity, so its kinematics, controls, flags, motion and actions are taken as sent once
//! the state passed the checks in `net.rs`. Damage is different, by the maintainer's choice of 2026-11-25:
//!
//! - an owner's damage values (a vehicle's engine, body and burst tyres; an NPC's health and armour) are taken only
//!   for NPCs and for vehicles that belong to nobody as property;
//! - only as decreases, and at most [`DAMAGE_PER_SECOND`] points a second per value ([`DamageBudget`]);
//! - for a vehicle that belongs to a player, the owner's damage values are ignored and the server's are kept.
//!
//! Vehicle damage beyond that comes only from events the server checked (an accepted hit report, a script).

use std::time::Instant;

use sawgrass_protocol::ped::{Motion, PedBlock};
use sawgrass_protocol::vehicle::VehicleBlock;

/// Most damage points an owner may take off one value in one second, and the most it may save up.
pub const DAMAGE_PER_SECOND: f32 = 100.0;

/// How much damage an owner may still report for one entity: refills at [`DAMAGE_PER_SECOND`].
#[derive(Debug, Clone, Copy)]
pub struct DamageBudget {
    points: f32,
    at: Option<Instant>,
}

impl Default for DamageBudget {
    fn default() -> Self {
        Self {
            points: DAMAGE_PER_SECOND,
            at: None,
        }
    }
}

impl DamageBudget {
    /// Refills the budget for the time since the last call.
    fn refill(&mut self, now: Instant) {
        if let Some(at) = self.at {
            let secs = now.saturating_duration_since(at).as_secs_f32();
            self.points = (self.points + secs * DAMAGE_PER_SECOND).min(DAMAGE_PER_SECOND);
        }
        self.at = Some(now);
    }

    /// The value after an owner reported `reported` for one at `server`: never higher, and lower by at most what
    /// is left of the budget, which the decrease spends.
    fn lower(&mut self, server: u8, reported: u8) -> u8 {
        let wanted = f32::from(server.saturating_sub(reported));
        let taken = wanted.min(self.points.max(0.0)).floor();
        self.points -= taken;
        server - taken as u8
    }
}

/// The vehicle block after its owner sent `owner`. `property` says whether the vehicle belongs to a player, whose
/// damage values the owner cannot change.
pub fn merge_vehicle(
    server: &VehicleBlock,
    owner: &VehicleBlock,
    property: bool,
    budget: &mut DamageBudget,
    now: Instant,
) -> VehicleBlock {
    let mut merged = VehicleBlock {
        engine: server.engine,
        body: server.body,
        burst_tyres: server.burst_tyres,
        ..*owner
    };
    if !property {
        budget.refill(now);
        merged.engine = budget.lower(server.engine, owner.engine);
        merged.body = budget.lower(server.body, owner.body);
        // A tyre can burst but not mend.
        merged.burst_tyres = server.burst_tyres | owner.burst_tyres;
    }
    merged
}

/// An NPC's ped block after its owner sent `owner`: presentation as sent, vehicle and seat the server's, health
/// and armour lowered within the budget, and "dead" and "in vehicle" the server's to set, as for a player.
pub fn merge_npc(server: &PedBlock, owner: &PedBlock, budget: &mut DamageBudget, now: Instant) -> PedBlock {
    let held = matches!(server.motion, Motion::InVehicle | Motion::Dead);
    let motion = match owner.motion {
        _ if held => server.motion,
        Motion::InVehicle | Motion::Dead => Motion::Idle,
        m => m,
    };
    budget.refill(now);
    PedBlock {
        motion,
        health: budget.lower(server.health, owner.health),
        armour: budget.lower(server.armour, owner.armour),
        vehicle: server.vehicle,
        seat: server.seat,
        ..*owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn wrecked() -> VehicleBlock {
        VehicleBlock {
            throttle: 90,
            flags: VehicleBlock::LIGHTS,
            engine: 0,
            body: 0,
            burst_tyres: 0b1111,
            ..VehicleBlock::INTACT
        }
    }

    #[test]
    fn a_vehicle_nobody_owns_takes_damage_within_the_budget_and_never_heals() {
        let now = Instant::now();
        let mut budget = DamageBudget::default();
        let m = merge_vehicle(&VehicleBlock::INTACT, &wrecked(), false, &mut budget, now);
        assert_eq!((m.throttle, m.flags), (90, VehicleBlock::LIGHTS));
        // 100 points this second, the engine first.
        assert_eq!((m.engine, m.body, m.burst_tyres), (155, 255, 0b1111));
        // Half a second later, 50 more.
        let m = merge_vehicle(
            &m,
            &wrecked(),
            false,
            &mut budget,
            now + Duration::from_millis(500),
        );
        assert_eq!((m.engine, m.body), (105, 255));
        // An owner cannot repair it, nor mend a tyre.
        let m = merge_vehicle(
            &m,
            &VehicleBlock::INTACT,
            false,
            &mut budget,
            now + Duration::from_secs(9),
        );
        assert_eq!((m.engine, m.body, m.burst_tyres), (105, 255, 0b1111));
    }

    #[test]
    fn a_players_vehicle_keeps_the_servers_damage() {
        let now = Instant::now();
        let mut budget = DamageBudget::default();
        let server = VehicleBlock {
            engine: 200,
            ..VehicleBlock::INTACT
        };
        let m = merge_vehicle(&server, &wrecked(), true, &mut budget, now);
        assert_eq!((m.engine, m.body, m.burst_tyres), (200, 255, 0));
        assert_eq!(m.throttle, 90, "the owner still drives it");
        let m = merge_vehicle(&server, &VehicleBlock::INTACT, true, &mut budget, now);
        assert_eq!(m.engine, 200, "nor repairs it");
    }

    #[test]
    fn an_npc_is_hurt_within_the_budget_but_cannot_heal_die_or_sit_by_its_owner() {
        let now = Instant::now();
        let mut budget = DamageBudget::default();
        let server = PedBlock {
            health: 100,
            armour: 50,
            ..PedBlock::default()
        };
        let owner = PedBlock {
            motion: Motion::Run,
            health: 0,
            armour: 0,
            seat: 2,
            ..PedBlock::default()
        };
        let m = merge_npc(&server, &owner, &mut budget, now);
        assert_eq!((m.motion, m.health, m.armour, m.seat), (Motion::Run, 0, 50, 0));
        let healed = PedBlock { health: 255, ..owner };
        assert_eq!(merge_npc(&m, &healed, &mut budget, now).health, 0);
        let dies = PedBlock {
            motion: Motion::Dead,
            ..owner
        };
        assert_eq!(merge_npc(&server, &dies, &mut budget, now).motion, Motion::Idle);
    }
}
