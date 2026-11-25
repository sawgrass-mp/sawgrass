//! Validation of the movement clients report (task SRV-9).
//!
//! Clients simulate their own movement (`docs/architecture.md`, "Authority"), so every state a client sends is a
//! claim. [`Movement::check`] refuses a state outside the map, one faster than the speed limit, and a move longer
//! than the player could have covered since its last accepted state. A refused state is not applied: the player
//! stays where it last was, for everyone, and the refusal is counted by kind for admins.
//!
//! A move may be as long as the speed limit times the wall-clock time since the last accepted state, plus
//! [`MovementRules::slack_seconds`] of movement for states that arrive bunched by network jitter. Measuring wall
//! time, not ticks run, keeps an overloaded server that skips tick slots from refusing honest players. And a refusal
//! is never permanent: the allowance keeps growing at the speed limit from the last accepted state, faster than a
//! player within it moves away, so a client cut off by a stall is accepted again once the allowance catches up.
//! Navmesh checks wait for the game's map data.
//!
//! A teleport the game itself makes (a respawn, an interior, a script) is refused like a cheat. Whoever adds
//! server-side teleports must reset the player's [`Movement`] with [`Movement::reset`].

use std::time::Instant;

use sawgrass_protocol::game::ClientState;

use crate::interest::MapBounds;

/// The limits a client's movement is checked against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovementRules {
    /// Fastest a player may move, in m/s, on foot or in any vehicle. 150 m/s is above a fast plane's cruise in a
    /// city-sized map; the game's real figures replace it once known.
    pub max_speed: f32,
    /// Seconds of movement at the speed limit allowed on top of the time since the last accepted state: how much
    /// jitter is forgiven.
    pub slack_seconds: f32,
    /// Lowest and highest height allowed, in metres.
    pub height: (f32, f32),
    /// The map's ground area.
    pub bounds: MapBounds,
}

impl Default for MovementRules {
    fn default() -> Self {
        Self {
            max_speed: 150.0,
            slack_seconds: 0.5,
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
    /// The last accepted position, in metres, and when it was accepted.
    last: Option<([f32; 3], Instant)>,
    /// What was refused.
    violations: Violations,
}

impl Movement {
    /// A player that has not moved yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the last accepted state, so the next one only has to be inside the map: for a teleport the server
    /// makes itself.
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Checks `state`, received at `now`; on success it becomes the last accepted one. A refusal is counted and
    /// changes nothing else.
    ///
    /// # Errors
    ///
    /// The first rule `state` breaks, checked in the order bounds, speed, teleport.
    pub fn check(
        &mut self,
        rules: &MovementRules,
        state: &ClientState,
        now: Instant,
    ) -> Result<(), Violation> {
        match self.judge(rules, state, now) {
            Ok(()) => {
                self.last = Some((state.position.to_metres(), now));
                Ok(())
            }
            Err(violation) => {
                self.violations.count(violation);
                Err(violation)
            }
        }
    }

    fn judge(&self, rules: &MovementRules, state: &ClientState, now: Instant) -> Result<(), Violation> {
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
        let Some(([lx, ly, lz], then)) = self.last else {
            return Ok(());
        };
        let seconds = now.saturating_duration_since(then).as_secs_f32() + rules.slack_seconds;
        let distance = ((x - lx).powi(2) + (y - ly).powi(2) + (z - lz).powi(2)).sqrt();
        if distance > rules.max_speed * seconds {
            return Err(Violation::Teleport);
        }
        Ok(())
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
    use std::time::Duration;

    use sawgrass_protocol::{Position, Rotation, Velocity};

    const DT: Duration = Duration::from_millis(50);

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
        let now = Instant::now();
        for p in [[9000.0, 0.0, 10.0], [0.0, 0.0, 5000.0], [0.0, -8192.5, 10.0]] {
            assert_eq!(m.check(&rules, &at(p, [0.0; 3]), now), Err(Violation::Bounds));
        }
        assert_eq!(m.check(&rules, &at([100.0, 100.0, 10.0], [0.0; 3]), now), Ok(()));
        assert_eq!(m.violations().bounds, 3);
    }

    #[test]
    fn a_velocity_above_the_limit_is_refused() {
        let rules = MovementRules::default();
        let mut m = Movement::new();
        let now = Instant::now();
        assert_eq!(
            m.check(&rules, &at([0.0; 3], [120.0, 100.0, 0.0]), now),
            Err(Violation::Speed)
        );
        assert_eq!(m.check(&rules, &at([0.0; 3], [100.0, 100.0, 0.0]), now), Ok(()));
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
        let mut now = Instant::now();
        let mut x = 0.0;
        m.check(&rules, &at([x, 0.0, 10.0], [0.0; 3]), now)
            .expect("spawn");
        // 70 m/s for ten seconds, one state a tick.
        for _ in 0..200 {
            now += DT;
            x += 70.0 * DT.as_secs_f32();
            m.check(&rules, &at([x, 0.0, 10.0], [70.0, 0.0, 0.0]), now)
                .expect("drive");
        }
        now += DT;
        assert_eq!(
            m.check(&rules, &at([x + 500.0, 0.0, 10.0], [0.0; 3]), now),
            Err(Violation::Teleport)
        );
        // Refused: the player is still where it was, and may go on from there.
        m.check(&rules, &at([x + 3.0, 0.0, 10.0], [0.0; 3]), now)
            .expect("next move");
        assert_eq!(m.violations().teleport, 1);
    }

    #[test]
    fn states_bunched_by_jitter_pass_and_a_jump_from_standing_does_not() {
        let rules = MovementRules::default();
        let mut m = Movement::new();
        let start = Instant::now();
        m.check(&rules, &at([0.0, 0.0, 10.0], [0.0; 3]), start)
            .expect("spawn");
        // At the speed limit, a state sent one tick later arrives with the next one's jitter: 0.3 s of movement
        // accepted only 50 ms after the last.
        let now = start + DT;
        m.check(&rules, &at([0.3 * 150.0, 0.0, 10.0], [150.0, 0.0, 0.0]), now)
            .expect("bunched state");
        // A player that keeps sending while standing still saves nothing up: 200 m at once is a jump.
        let mut now = now;
        for _ in 0..1200 {
            now += DT;
            m.check(&rules, &at([45.0, 0.0, 10.0], [0.0; 3]), now)
                .expect("standing");
        }
        assert_eq!(
            m.check(&rules, &at([245.0, 0.0, 10.0], [0.0; 3]), now + DT),
            Err(Violation::Teleport)
        );
    }

    #[test]
    fn a_car_refused_after_a_stall_is_accepted_again() {
        let rules = MovementRules::default();
        let mut m = Movement::new();
        let mut now = Instant::now();
        let mut x = 0.0f32;
        let speed = [80.0, 0.0, 0.0];
        m.check(&rules, &at([x, 0.0, 10.0], speed), now).expect("spawn");
        // At 80 m/s, two seconds of states go nowhere and the next one is accepted only a tick after the last
        // (a stall the server's clock did not see): 160 m in 50 ms, refused.
        now += DT;
        x += 160.0;
        assert_eq!(
            m.check(&rules, &at([x, 0.0, 10.0], speed), now),
            Err(Violation::Teleport)
        );
        // One state a tick at 80 m/s from there: the allowance grows at 150 m/s and catches up within about a
        // second, and every state after that passes.
        let mut back = None;
        for i in 1..=200 {
            now += DT;
            x += 80.0 * DT.as_secs_f32();
            let ok = m.check(&rules, &at([x, 0.0, 10.0], speed), now).is_ok();
            match back {
                None if ok => back = Some(i),
                Some(_) => assert!(ok, "refused again at tick {i}"),
                None => {}
            }
        }
        let back = back.expect("never accepted again");
        assert!(back <= 30, "accepted again only after {back} ticks");
    }

    #[test]
    fn an_overloaded_server_does_not_refuse_a_player_at_the_limit() {
        // Ticks run every 200 ms instead of 50: the allowance follows the clock, not the ticks run.
        let rules = MovementRules::default();
        let mut m = Movement::new();
        let mut now = Instant::now();
        let mut x = 0.0;
        m.check(&rules, &at([x, 0.0, 10.0], [0.0; 3]), now)
            .expect("spawn");
        for _ in 0..100 {
            now += Duration::from_millis(200);
            x += 150.0 * 0.2;
            m.check(&rules, &at([x, 0.0, 10.0], [150.0, 0.0, 0.0]), now)
                .expect("at the limit");
        }
    }

    #[test]
    fn a_reset_lets_the_server_move_a_player() {
        let rules = MovementRules::default();
        let mut m = Movement::new();
        let now = Instant::now();
        m.check(&rules, &at([0.0, 0.0, 10.0], [0.0; 3]), now)
            .expect("spawn");
        m.reset();
        m.check(&rules, &at([3000.0, 0.0, 10.0], [0.0; 3]), now)
            .expect("respawn");
    }
}
