//! Validation of the movement clients report (task SRV-9).
//!
//! Clients simulate their own movement (`docs/architecture.md`, "Authority"), so every state a client sends is a
//! claim. [`Movement::check`] refuses a state outside the map, one faster than the speed limit, and a move longer
//! than the player could have covered since its last accepted state. A refused state is not applied: the player
//! stays where it last was, for everyone, and the refusal is counted by kind for admins. The client is sent that
//! last accepted state (`ServerEvent::Corrected`, from `net`) so it moves its player back.
//!
//! A move may be as long as the speed limit times the wall-clock time since the last accepted state, plus a
//! slack for states that arrive bunched by network jitter. The slack is a purse of at most
//! [`MovementRules::slack_seconds`] of movement: an accepted move keeps what it left unused of its allowance, up to
//! that cap, so a bunched state may spend it but a client moving faster than the limit soon has none, and is held to
//! the limit. Measuring wall time, not ticks run, keeps an overloaded server that skips tick slots from refusing
//! honest players. And a refusal
//! is never permanent: the allowance keeps growing at the speed limit from the last accepted state, faster than a
//! player within it moves away, so a client cut off by a stall is accepted again once the allowance catches up.
//! Navmesh checks wait for the game's map data.
//!
//! A teleport the game itself makes (a respawn, an interior, a script) is refused like a cheat. Whoever adds
//! server-side teleports must reset the player's [`Movement`] with [`Movement::reset`].

use std::time::Instant;

use sawgrass_protocol::game::ClientState;

use crate::interest::{DENSE_CELL_SIZE, MapBounds};

/// Most cells the interest grid may have over [`MovementRules::bounds`] (the limit of `DenseGrid::new`).
const MAX_GRID_CELLS: f64 = (1u64 << 26) as f64;

/// The limits a client's movement is checked against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovementRules {
    /// Fastest a player may move, in m/s, on foot or in any vehicle. 150 m/s is above a fast plane's cruise in a
    /// city-sized map; the game's real figures replace it once known.
    pub max_speed: f32,
    /// Most seconds of movement at the speed limit a player may keep as slack and spend on top of the time since
    /// the last accepted state: how much jitter is forgiven.
    pub slack_seconds: f32,
    /// Lowest and highest height allowed, in metres.
    pub height: (f32, f32),
    /// The map's ground area. The interest grid covers the same square.
    pub bounds: MapBounds,
    /// Count a state that breaks a rule but apply it anyway, and send no correction: for a first session with the
    /// game, to learn which limits are wrong without fighting them. Not for public servers.
    pub report_only: bool,
}

impl MovementRules {
    /// Whether a state's position is inside the map's ground area and height range.
    #[must_use]
    pub fn contains(&self, state: &ClientState) -> bool {
        let [x, y, z] = state.position.to_metres();
        let (min, size) = (self.bounds.min, self.bounds.size);
        let inside = x >= min.x && x < min.x + size && y >= min.y && y < min.y + size;
        inside && z >= self.height.0 && z <= self.height.1
    }

    /// Checks that the rules can be used: finite numbers, a positive speed limit and map size, a height range that
    /// is not empty, and a map the interest grid can cover.
    ///
    /// # Errors
    ///
    /// What is wrong, in words for the person who set it.
    pub fn validate(&self) -> Result<(), String> {
        let MapBounds { min, size } = self.bounds;
        if !(min.x.is_finite() && min.y.is_finite()) {
            return Err("the map's corner must be finite".into());
        }
        if !(size.is_finite() && size > 0.0) {
            return Err(format!("the map size must be positive, not {size}"));
        }
        let cells = (f64::from(size) / f64::from(DENSE_CELL_SIZE)).ceil().powi(2);
        if cells > MAX_GRID_CELLS {
            return Err(format!(
                "a {size} m map needs {cells} grid cells of {DENSE_CELL_SIZE} m; the most is {MAX_GRID_CELLS}"
            ));
        }
        let (low, high) = self.height;
        if !(low.is_finite() && high.is_finite() && low < high) {
            return Err(format!(
                "the height range must be finite and low below high, not {low}..{high}"
            ));
        }
        if !(self.max_speed.is_finite() && self.max_speed > 0.0) {
            return Err(format!(
                "the speed limit must be positive, not {}",
                self.max_speed
            ));
        }
        if !(self.slack_seconds.is_finite() && self.slack_seconds >= 0.0) {
            return Err(format!(
                "the slack must be zero or more seconds, not {}",
                self.slack_seconds
            ));
        }
        Ok(())
    }
}

impl Default for MovementRules {
    fn default() -> Self {
        Self {
            max_speed: 150.0,
            slack_seconds: 0.5,
            height: (-500.0, 3000.0),
            bounds: MapBounds::default(),
            report_only: false,
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
    /// Metres of slack left, from the allowance accepted moves did not use.
    slack: f32,
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

    fn full_slack(rules: &MovementRules) -> f32 {
        rules.max_speed * rules.slack_seconds
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
            Ok(left) => {
                self.slack = left.min(Self::full_slack(rules));
                self.last = Some((state.position.to_metres(), now));
                Ok(())
            }
            Err(violation) => {
                self.violations.count(violation);
                Err(violation)
            }
        }
    }

    /// Takes `state` as the last accepted one although it broke a rule ([`MovementRules::report_only`]), with the
    /// full slack, as if the player had been placed there.
    pub fn accept(&mut self, rules: &MovementRules, state: &ClientState, now: Instant) {
        self.slack = Self::full_slack(rules);
        self.last = Some((state.position.to_metres(), now));
    }

    /// The allowance left after the move, or why it is refused.
    fn judge(&self, rules: &MovementRules, state: &ClientState, now: Instant) -> Result<f32, Violation> {
        if !rules.contains(state) {
            return Err(Violation::Bounds);
        }
        let [x, y, z] = state.position.to_metres();
        let [vx, vy, vz] = state.velocity.to_metres_per_second();
        if vx * vx + vy * vy + vz * vz > rules.max_speed * rules.max_speed {
            return Err(Violation::Speed);
        }
        // The first state places the player: only the bounds apply.
        let Some(([lx, ly, lz], then)) = self.last else {
            return Ok(Self::full_slack(rules));
        };
        let allowed = rules.max_speed * now.saturating_duration_since(then).as_secs_f32() + self.slack;
        let distance = ((x - lx).powi(2) + (y - ly).powi(2) + (z - lz).powi(2)).sqrt();
        if distance > allowed {
            return Err(Violation::Teleport);
        }
        Ok(allowed - distance)
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

    use crate::interest::Point;

    const DT: Duration = Duration::from_millis(50);

    fn at(p: [f32; 3], v: [f32; 3]) -> ClientState {
        ClientState {
            position: Position::from_metres(p).expect("position"),
            velocity: Velocity::from_metres_per_second(v).expect("velocity"),
            rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
        }
    }

    #[test]
    fn the_default_rules_are_valid_and_bad_ones_are_named() {
        assert_eq!(MovementRules::default().validate(), Ok(()));
        let bad = [
            MovementRules {
                max_speed: 0.0,
                ..MovementRules::default()
            },
            MovementRules {
                max_speed: f32::NAN,
                ..MovementRules::default()
            },
            MovementRules {
                slack_seconds: -1.0,
                ..MovementRules::default()
            },
            MovementRules {
                height: (10.0, 10.0),
                ..MovementRules::default()
            },
            MovementRules {
                bounds: MapBounds {
                    size: 0.0,
                    ..MapBounds::default()
                },
                ..MovementRules::default()
            },
            MovementRules {
                bounds: MapBounds {
                    size: 500_000.0,
                    ..MapBounds::default()
                },
                ..MovementRules::default()
            },
            MovementRules {
                bounds: MapBounds {
                    min: Point::new(f32::INFINITY, 0.0),
                    size: 100.0,
                },
                ..MovementRules::default()
            },
        ];
        for rules in bad {
            assert!(rules.validate().is_err(), "{rules:?}");
        }
        // The largest grid the server allows: 409,600 m at 50 m cells.
        let largest = MovementRules {
            bounds: MapBounds {
                size: 409_600.0,
                ..MapBounds::default()
            },
            ..MovementRules::default()
        };
        assert_eq!(largest.validate(), Ok(()));
    }

    #[test]
    fn an_accepted_rule_breaker_is_the_new_starting_point() {
        let rules = MovementRules::default();
        let mut m = Movement::new();
        let now = Instant::now();
        m.check(&rules, &at([0.0, 0.0, 10.0], [0.0; 3]), now)
            .expect("spawn");
        let jump = at([5000.0, 0.0, 10.0], [0.0; 3]);
        assert_eq!(m.check(&rules, &jump, now + DT), Err(Violation::Teleport));
        m.accept(&rules, &jump, now + DT);
        assert_eq!(
            m.check(&rules, &at([5005.0, 0.0, 10.0], [0.0; 3]), now + 2 * DT),
            Ok(())
        );
        assert_eq!(m.violations().teleport, 1);
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
    fn a_client_faster_than_the_limit_is_held_to_it() {
        // 82 m a tick with velocity 0: 1,640 m/s. The slack covers the first tick at most; after that every state is
        // refused, because each is further from the last accepted one than the limit allows.
        let rules = MovementRules::default();
        let mut m = Movement::new();
        let mut now = Instant::now();
        let mut x = 0.0f32;
        m.check(&rules, &at([x, 0.0, 10.0], [0.0; 3]), now)
            .expect("spawn");
        let mut accepted = Vec::new();
        for i in 0..50 {
            now += DT;
            x += 82.0;
            if m.check(&rules, &at([x, 0.0, 10.0], [0.0; 3]), now).is_ok() {
                accepted.push(i);
            }
        }
        assert!(accepted.iter().all(|&i| i < 1), "accepted at ticks {accepted:?}");
        assert_eq!(m.violations().teleport, 49);
    }

    #[test]
    fn a_car_keeps_driving_after_a_two_second_stall() {
        // 80 m/s, one state a tick; the uplink drops for two seconds, then states resume where the car really is.
        let rules = MovementRules::default();
        let mut m = Movement::new();
        let mut now = Instant::now();
        let mut x = 0.0f32;
        let speed = [80.0, 0.0, 0.0];
        for tick in 0..300 {
            now += DT;
            x += 80.0 * DT.as_secs_f32();
            if (100..140).contains(&tick) {
                continue;
            }
            m.check(&rules, &at([x, 0.0, 10.0], speed), now)
                .unwrap_or_else(|v| panic!("tick {tick}: {v:?}"));
        }
        assert_eq!(m.violations().total(), 0);
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
