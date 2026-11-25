//! The world state the server owns (ADR 0006, section 7): the clock, the weather and the doors resources
//! registered. `net.rs` sends it to every joining player and each change to everyone.
//!
//! Routing buckets (interiors, instances) are part of the same section and come with their own task, as they
//! change interest management.

use std::collections::HashMap;
use std::time::Duration;

use sawgrass_protocol::entity::AssetId;
use sawgrass_protocol::world::{Door, Environment, MINUTES_PER_DAY};

/// The clock, the weather and the doors.
#[derive(Debug, Clone)]
pub struct WorldState {
    /// The game minute of the day at tick `since`.
    minute: f64,
    since: u32,
    /// Game minutes per real second.
    rate: f64,
    weather: AssetId,
    next_weather: AssetId,
    transition: u8,
    doors: Vec<Door>,
    by_id: HashMap<AssetId, usize>,
}

impl Default for WorldState {
    /// Noon, the clock at two game minutes a second (a game day in 12 real minutes, a placeholder until the
    /// roleplay base sets one), no weather.
    fn default() -> Self {
        Self {
            minute: 12.0 * 60.0,
            since: 0,
            rate: 2.0,
            weather: AssetId::NONE,
            next_weather: AssetId::NONE,
            transition: 0,
            doors: Vec::new(),
            by_id: HashMap::new(),
        }
    }
}

impl WorldState {
    /// The game minute of the day at `tick`, ticks being `dt` long.
    #[must_use]
    pub fn minute_at(&self, tick: u32, dt: Duration) -> f64 {
        let secs = f64::from(tick.saturating_sub(self.since)) * dt.as_secs_f64();
        (self.minute + secs * self.rate).rem_euclid(f64::from(MINUTES_PER_DAY))
    }

    /// Sets the clock to `minute` of the day at `tick`, running at `rate` game minutes per real second (0 stops
    /// it). Out-of-range values are wrapped or clamped to what the wire carries.
    pub fn set_clock(&mut self, tick: u32, minute: f64, rate: f64) {
        self.since = tick;
        self.minute = minute.rem_euclid(f64::from(MINUTES_PER_DAY));
        self.rate = rate.clamp(0.0, f64::from(u16::MAX) / 100.0);
    }

    /// Sets the weather, turning from `weather` into `next` with `transition` of the way done (0 to 255). The
    /// clock is first brought to `tick`, so the event the change sends carries the right minute.
    pub fn set_weather(&mut self, tick: u32, dt: Duration, weather: AssetId, next: AssetId, transition: u8) {
        self.set_clock(tick, self.minute_at(tick, dt), self.rate);
        self.weather = weather;
        self.next_weather = next;
        self.transition = transition;
    }

    /// The clock and the weather as of `tick`.
    #[must_use]
    pub fn environment(&self, tick: u32, dt: Duration) -> Environment {
        // Below MINUTES_PER_DAY and within u16 after the wrap and the clamp above.
        Environment {
            tick,
            minute: (self.minute_at(tick, dt) as u16).min(MINUTES_PER_DAY - 1),
            rate: (self.rate * 100.0).round() as u16,
            weather: self.weather,
            next_weather: self.next_weather,
            transition: self.transition,
        }
    }

    /// Registers the door or changes it; returns whether anything changed.
    pub fn set_door(&mut self, door: Door) -> bool {
        match self.by_id.get(&door.id) {
            Some(&i) if self.doors[i] == door => false,
            Some(&i) => {
                self.doors[i] = door;
                true
            }
            None => {
                self.by_id.insert(door.id, self.doors.len());
                self.doors.push(door);
                true
            }
        }
    }

    /// Every registered door, in the order they were registered.
    #[must_use]
    pub fn doors(&self) -> &[Door] {
        &self.doors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: Duration = Duration::from_millis(50);

    #[test]
    fn the_clock_runs_at_its_rate_and_wraps_at_midnight() {
        let mut world = WorldState::default();
        world.set_clock(100, 23.0 * 60.0 + 59.0, 2.0);
        // 20 ticks of 50 ms is a real second: two game minutes, past midnight.
        assert!((world.minute_at(120, DT) - 1.0).abs() < 1e-9);
        let e = world.environment(120, DT);
        assert_eq!((e.tick, e.minute, e.rate), (120, 1, 200));
        world.set_clock(120, 600.0, 0.0);
        assert_eq!(world.environment(10_000, DT).minute, 600);
    }

    #[test]
    fn a_weather_change_keeps_the_clock_where_it_was() {
        let mut world = WorldState::default();
        world.set_clock(0, 600.0, 2.0);
        world.set_weather(200, DT, AssetId(1), AssetId(2), 64);
        let e = world.environment(200, DT);
        assert_eq!(
            (e.minute, e.weather, e.next_weather, e.transition),
            (620, AssetId(1), AssetId(2), 64)
        );
    }

    #[test]
    fn a_door_is_registered_once_and_changed_in_place() {
        let mut world = WorldState::default();
        let door = |id, locked| Door {
            id: AssetId(id),
            locked,
            open: 0,
        };
        assert!(world.set_door(door(5, false)));
        assert!(world.set_door(door(6, true)));
        assert!(!world.set_door(door(5, false)));
        assert!(world.set_door(door(5, true)));
        assert_eq!(world.doors(), [door(5, true), door(6, true)]);
    }
}
