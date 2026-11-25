//! Validation of the movement clients report (task SRV-9).
//!
//! Clients simulate their own movement (`docs/architecture.md`, "Authority"), so every state a client sends is a
//! claim. [`Movement::check`] refuses a state outside the map, one faster than the speed limit, and a move longer
//! than the player could have covered since its last accepted state. A refused state is not applied: the player
//! stays where it last was, for everyone, and the refusal is counted by kind for admins.
//!
//! The distance a player may cover is a budget. It grows at the speed limit each tick and is spent by each accepted
//! move. It is capped at [`MovementRules::burst_seconds`] of movement, so states that arrive bunched by network
//! jitter still pass, but a jump across the map does not. Navmesh checks wait for the game's map data.

use sawgrass_protocol::game::ClientState;

use crate::interest::MapBounds;

/// The limits a client's movement is checked against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovementRules {
    /// Fastest a player may move, in m/s, on foot or in any vehicle. 150 m/s is above a fast plane's cruise in a
    /// city-sized map; the game's real figures replace it once known.
    pub max_speed: f32,
    /// Most seconds of movement a player may save up and spend in one move: how much jitter is forgiven.
    pub burst_seconds: f32,
    /// Lowest and highest height allowed, in metres.
    pub height: (f32, f32),
    /// The map's ground area.
    pub bounds: MapBounds,
}

impl Default for MovementRules {
    fn default() -> Self {
        Self {
            max_speed: 150.0,
            burst_seconds: 1.0,
            height: (-500.0, 3000.0),
            bounds: MapBounds::default(),
        }
    }
}

/// Why a state was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// The position is outside the map.
    Bounds,
    /// The velocity is above the speed limit.
    Speed,
    /// The move is longer than the player could have covered.
    Teleport,
}

/// Refused states, by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Violations {
    /// Positions outside the map.
    pub bounds: u64,
    /// Velocities above the limit.
    pub speed: u64,
    /// Moves too long for the time.
    pub teleport: u64,
}

impl Violations {
    /// Counts one refusal.
    pub fn count(&mut self, violation: Violation) {
        match violation {
            Violation::Bounds => self.bounds += 1,
            Violation::Speed => self.speed += 1,
            Violation::Teleport => self.teleport += 1,
        }
    }

    /// All refusals.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.bounds + self.speed + self.teleport
    }
}

/// One player's movement so far.
#[derive(Debug, Clone, Default)]
pub struct Movement {
    /// The last accepted position, in metres.
    last: Option<[f32; 3]>,
    /// Metres the player may still move before the next tick adds more.
    budget: f32,
    /// What was refused.
    violations: Violations,
}

impl Movement {
    /// A player that has not moved yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one tick of `dt` seconds to the distance the player may move.
    pub fn tick(&mut self, rules: &MovementRules, dt: f32) {
        self.budget = (self.budget + rules.max_speed * dt).min(rules.max_speed * rules.burst_seconds);
    }

    /// Checks `state`; on success it becomes the last accepted one. A refusal is counted and changes nothing else.
    ///
    /// # Errors
    ///
    /// The first rule `state` breaks, checked in the order bounds, speed, teleport.
    pub fn check(&mut self, rules: &MovementRules, state: &ClientState) -> Result<(), Violation> {
        let result = self.judge(rules, state);
        match result {
            Ok(distance) => {
                self.budget -= distance;
                self.last = Some(state.position.to_metres());
                Ok(())
            }
            Err(violation) => {
                self.violations.count(violation);
                Err(violation)
            }
        }
    }

    fn judge(&self, rules: &MovementRules, state: &ClientState) -> Result<f32, Violation> {
        let [x, y, z] = state.position.to_metres();
        let (min, size) = (rules.bounds.min, rules.bounds.size);
        let inside = x >= min.x && x < min.x + size && y >= min.y && y < min.y + size;
        if !inside || z < rules.height.0 || z > rules.height.1 {
            return Err(Violation::Bounds);
        }
        let [vx, vy, vz] = state.velocity.to_metres_per_second();
        if vx * vx + vy * vy + vz * vz > rules.max_speed * rules.max_speed {
            return Err(Violation::Speed);
        }
        // The first state places the player: only the bounds apply.
        let Some([lx, ly, lz]) = self.last else {
            return Ok(0.0);
        };
        let distance = ((x - lx).powi(2) + (y - ly).powi(2) + (z - lz).powi(2)).sqrt();
        if distance > self.budget {
            return Err(Violation::Teleport);
        }
        Ok(distance)
    }

    /// What this player had refused.
    #[must_use]
    pub fn violations(&self) -> Violations {
        self.violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sawgrass_protocol::{Position, Rotation, Velocity};

    const DT: f32 = 0.05;

    fn at(p: [f32; 3], v: [f32; 3]) -> ClientState {
        ClientState {
            position: Position::from_metres(p).expect("position"),
            velocity: Velocity::from_metres_per_second(v).expect("velocity"),
            rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
        }
    }

    #[test]
    fn a_state_outside_the_map_is_refused() {
        let rules = MovementRules::default();
        let mut m = Movement::new();
        assert_eq!(
            m.check(&rules, &at([9000.0, 0.0, 10.0], [0.0; 3])),
            Err(Violation::Bounds)
        );
        assert_eq!(
            m.check(&rules, &at([0.0, 0.0, 5000.0], [0.0; 3])),
            Err(Violation::Bounds)
        );
        assert_eq!(
            m.check(&rules, &at([0.0, -8192.5, 10.0], [0.0; 3])),
            Err(Violation::Bounds)
        );
        assert_eq!(m.check(&rules, &at([100.0, 100.0, 10.0], [0.0; 3])), Ok(()));
        assert_eq!(m.violations().bounds, 3);
    }

    #[test]
    fn a_velocity_above_the_limit_is_refused() {
        let rules = MovementRules::default();
        let mut m = Movement::new();
        assert_eq!(
            m.check(&rules, &at([0.0; 3], [120.0, 100.0, 0.0])),
            Err(Violation::Speed)
        );
        assert_eq!(m.check(&rules, &at([0.0; 3], [100.0, 100.0, 0.0])), Ok(()));
        assert_eq!(
            m.violations(),
            Violations {
                speed: 1,
                ..Violations::default()
            }
        );
    }

    #[test]
    fn a_fast_car_passes_and_a_teleport_does_not() {
        let rules = MovementRules::default();
        let mut m = Movement::new();
        let mut x = 0.0;
        m.check(&rules, &at([x, 0.0, 10.0], [0.0; 3])).expect("spawn");
        // 70 m/s for ten seconds, one state a tick.
        for _ in 0..200 {
            m.tick(&rules, DT);
            x += 70.0 * DT;
            m.check(&rules, &at([x, 0.0, 10.0], [70.0, 0.0, 0.0]))
                .expect("drive");
        }
        m.tick(&rules, DT);
        assert_eq!(
            m.check(&rules, &at([x + 500.0, 0.0, 10.0], [0.0; 3])),
            Err(Violation::Teleport)
        );
        // Refused: the player is still where it was, and may go on from there.
        m.check(&rules, &at([x + 3.0, 0.0, 10.0], [0.0; 3]))
            .expect("next move");
        assert_eq!(m.violations().teleport, 1);
    }

    #[test]
    fn states_bunched_by_jitter_pass() {
        let rules = MovementRules::default();
        let mut m = Movement::new();
        m.check(&rules, &at([0.0, 0.0, 10.0], [0.0; 3])).expect("spawn");
        // At the speed limit, nothing arrives for 15 ticks, then the newest state: 0.75 s of movement at once.
        for _ in 0..15 {
            m.tick(&rules, DT);
        }
        m.check(&rules, &at([0.75 * 150.0, 0.0, 10.0], [150.0, 0.0, 0.0]))
            .expect("late state");
        // But the allowance does not grow past one second: after a minute standing still, 200 m at once is a jump.
        for _ in 0..1200 {
            m.tick(&rules, DT);
        }
        assert_eq!(
            m.check(&rules, &at([0.75 * 150.0 + 200.0, 0.0, 10.0], [0.0; 3])),
            Err(Violation::Teleport)
        );
    }
}
