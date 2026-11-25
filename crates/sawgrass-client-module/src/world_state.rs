//! World state the server owns (ADR 0006, section 7), applied to the game: the clock, the weather and doors.
//!
//! The server sends the [`Environment`] on joining and on each change, and each [`Door`] the same way. The clock
//! runs between changes: the server sends the minute at a tick and the rate, and [`minute_of_day`] works out the
//! minute from the server tick the client estimates, so every client shows the same time without a stream of
//! updates. The weather and doors are applied once per change.

use sawgrass_protocol::world::{Door, Environment, MINUTES_PER_DAY};

use crate::module::World;

/// The game minute of the day at `server_tick`, from an [`Environment`] that held at an earlier tick. A tick before
/// the environment's (the estimate trailing it) counts as the environment's own.
#[must_use]
pub fn minute_of_day(environment: &Environment, server_tick: f64, tick_hz: f64) -> f32 {
    let elapsed_secs = if tick_hz > 0.0 {
        (server_tick - f64::from(environment.tick)).max(0.0) / tick_hz
    } else {
        0.0
    };
    let minutes = f64::from(environment.minute) + f64::from(environment.rate) / 100.0 * elapsed_secs;
    minutes.rem_euclid(f64::from(MINUTES_PER_DAY)) as f32
}

/// What the server last said about the world, and what is still to apply.
#[derive(Debug, Default)]
pub struct WorldState {
    environment: Option<Environment>,
    weather_due: bool,
    doors_due: Vec<Door>,
}

impl WorldState {
    /// Takes a [`ServerEvent::Environment`](sawgrass_protocol::game::ServerEvent::Environment). The weather is
    /// applied again only if it changed; the clock follows the new values from the next frame.
    pub fn set_environment(&mut self, environment: Environment) {
        let weather = |e: &Environment| (e.weather, e.next_weather, e.transition);
        self.weather_due |= self.environment.as_ref().map(weather) != Some(weather(&environment));
        self.environment = Some(environment);
    }

    /// Takes a [`ServerEvent::Door`](sawgrass_protocol::game::ServerEvent::Door), applied on the next frame.
    pub fn set_door(&mut self, door: Door) {
        self.doors_due.push(door);
    }

    /// The environment last received.
    #[must_use]
    pub fn environment(&self) -> Option<&Environment> {
        self.environment.as_ref()
    }

    /// Sets the clock for `server_tick` (the environment's own tick while the client has no estimate), and applies
    /// a changed weather and the doors that changed. Returns how many of them the world refused; a refused one is
    /// not tried again until the server sends it again, since an id missing from the asset table stays missing.
    pub fn apply(&mut self, server_tick: Option<f64>, tick_hz: f64, world: &mut World<'_>) -> u64 {
        let mut refused = 0;
        if let Some(environment) = &self.environment {
            let tick = server_tick.unwrap_or(f64::from(environment.tick));
            refused += u64::from(
                world
                    .set_clock(minute_of_day(environment, tick, tick_hz))
                    .is_err(),
            );
            if std::mem::take(&mut self.weather_due) {
                refused += u64::from(world.set_weather(environment).is_err());
            }
        }
        for door in self.doors_due.drain(..) {
            refused += u64::from(world.set_door(&door).is_err());
        }
        refused
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sawgrass_protocol::entity::AssetId;

    fn environment(tick: u32, minute: u16, rate: u16) -> Environment {
        Environment {
            tick,
            minute,
            rate,
            weather: AssetId(1),
            next_weather: AssetId(1),
            transition: 0,
        }
    }

    #[test]
    fn the_clock_runs_from_the_tick_it_held_at() {
        // Two game minutes a real second at 20 ticks a second: 600 ticks are 30 seconds, 60 game minutes.
        let e = environment(1_000, 12 * 60, 200);
        assert_eq!(minute_of_day(&e, 1_000.0, 20.0), 720.0);
        assert_eq!(minute_of_day(&e, 1_600.0, 20.0), 780.0);
        assert_eq!(minute_of_day(&e, 1_010.0, 20.0), 721.0);
    }

    #[test]
    fn the_clock_wraps_at_midnight_and_holds_when_stopped_or_early() {
        let e = environment(0, 23 * 60 + 30, 200);
        // Midnight after 15 real seconds, half past after 30.
        assert_eq!(minute_of_day(&e, 300.0, 20.0), 0.0);
        assert_eq!(minute_of_day(&e, 600.0, 20.0), 30.0);
        let stopped = environment(0, 300, 0);
        assert_eq!(minute_of_day(&stopped, 1e6, 20.0), 300.0);
        // An estimate behind the environment's tick does not run the clock backwards.
        let later = environment(5_000, 300, 200);
        assert_eq!(minute_of_day(&later, 4_000.0, 20.0), 300.0);
    }

    #[test]
    fn only_a_changed_weather_is_due_again() {
        let mut state = WorldState::default();
        state.set_environment(environment(0, 0, 100));
        assert!(state.weather_due);
        state.weather_due = false;
        // A new clock with the same weather.
        state.set_environment(environment(100, 60, 100));
        assert!(!state.weather_due);
        let mut turning = environment(200, 60, 100);
        turning.next_weather = AssetId(2);
        state.set_environment(turning);
        assert!(state.weather_due);
    }
}
