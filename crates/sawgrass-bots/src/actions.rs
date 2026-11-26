//! What a driving or shooting bot does besides walking (BOT-5), as plain state machines the bot's send loop
//! steps, so they are tested without a server.
//!
//! A driver walks to a vehicle the server says it owns, takes the driver's seat, drives it along its walker's
//! legs, leaves it, sometimes sits a while in the passenger seat of the same parked vehicle, and walks on. The
//! server changes owners as it moves, so a run sees seats taken and left and ownership handed over.
//!
//! A shooter fires at intact vehicles in the latest snapshot it decoded, at the positions that snapshot gave, so
//! every hit of an honest bot passes the server's checks and changes something: a wrecked vehicle (body 0) takes
//! no more damage, so a hit on it would measure only the checks (BOT-9). It hits no player or NPC: the server has no respawn yet, so a
//! killed bot would stay dead and its later shots would be refused.

use sawgrass_protocol::EntityState;
use sawgrass_protocol::Position;
use sawgrass_protocol::combat::{Hit, Shot, Zone};
use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
use sawgrass_protocol::vehicle::VehicleBlock;

use crate::walker::{Rng, Walker};

/// How close a driver goes before it asks for a seat, in metres; the server's limit is 5 m.
pub const ENTER_AT: f32 = 3.0;
/// The driver's walking speed to a vehicle, in metres per second: a run.
pub const APPROACH_SPEED: f32 = 5.5;
/// How long a driver tries to reach a vehicle before it gives up, in seconds.
pub const APPROACH_LIMIT: f32 = 60.0;
/// The weapon every shooter holds.
pub const WEAPON: AssetId = AssetId(1);
/// How far a shooter aims, in metres: within the server's default weapon range of 150 m.
pub const SHOT_RANGE: f32 = 100.0;
/// The most vehicles one shot hits: the server's placeholder weapon fires one projectile.
pub const HITS_PER_SHOT: usize = 1;

/// The latest snapshot a bot decoded: its tick and its entities with their infos.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// The snapshot's tick.
    pub tick: u32,
    /// The entities, as decoded.
    pub entities: Vec<EntityState>,
    /// Their infos, in the same order.
    pub infos: Vec<Info>,
}

impl Seen {
    /// The position of entity `id`, in metres, if the snapshot holds it.
    #[must_use]
    pub fn position(&self, id: u32) -> Option<[f32; 3]> {
        self.entities
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.position.to_metres())
    }
}

/// What a driver asks the server for this step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Nothing.
    None,
    /// [`ClientEvent::EnterSeat`](sawgrass_protocol::game::ClientEvent::EnterSeat).
    Enter {
        /// The vehicle's entity id.
        vehicle: u32,
        /// The seat, 0 the driver's.
        seat: u8,
    },
    /// [`ClientEvent::LeaveSeat`](sawgrass_protocol::game::ClientEvent::LeaveSeat).
    Leave,
}

/// Where a driver is in its cycle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    /// On foot, for `left` more seconds before it looks for a vehicle.
    Walking { left: f32 },
    /// Going to `vehicle`, for at most `left` more seconds.
    Approaching { vehicle: u32, left: f32 },
    /// In the driver's seat of `vehicle`, for `left` more seconds; the bot sends the vehicle's state.
    Driving { vehicle: u32, left: f32 },
    /// Out of the driver's seat, about to take the passenger seat of the same vehicle.
    Parked { vehicle: u32 },
    /// In the passenger seat of the parked `vehicle`, for `left` more seconds; the walker stands still.
    Riding { vehicle: u32, left: f32 },
}

/// A driving bot's cycle.
#[derive(Debug, Clone)]
pub struct Driver {
    phase: Phase,
    rng: Rng,
}

impl Driver {
    /// A driver for bot number `index`, starting on foot.
    #[must_use]
    pub fn new(index: u64) -> Self {
        let mut rng = Rng::new(index ^ 0xB075);
        let left = rng.range(2.0, 10.0);
        Self {
            phase: Phase::Walking { left },
            rng,
        }
    }

    /// Where the driver is.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The vehicle whose state the bot sends this step, if it drives one.
    #[must_use]
    pub fn driving(&self) -> Option<u32> {
        match self.phase {
            Phase::Driving { vehicle, .. } => Some(vehicle),
            _ => None,
        }
    }

    /// Whether the walker stands still this step.
    #[must_use]
    pub fn holds(&self) -> bool {
        matches!(self.phase, Phase::Parked { .. } | Phase::Riding { .. })
    }

    /// Advances by `dt` seconds. `owned` lists the vehicles the server says this bot owns, `seen` the latest
    /// snapshot. Steers `walker` and returns what to ask the server.
    pub fn step(&mut self, dt: f32, walker: &mut Walker, owned: &[u32], seen: &Seen) -> Request {
        let me = walker.position();
        match &mut self.phase {
            Phase::Walking { left } => {
                *left -= dt;
                if *left > 0.0 {
                    return Request::None;
                }
                // The nearest owned vehicle the snapshot shows; none yet, look again later.
                let nearest = owned
                    .iter()
                    .filter_map(|&id| seen.position(id).map(|p| (id, distance(me, p))))
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                self.phase = match nearest {
                    Some((vehicle, _)) => Phase::Approaching {
                        vehicle,
                        left: APPROACH_LIMIT,
                    },
                    None => Phase::Walking { left: 2.0 },
                };
                Request::None
            }
            Phase::Approaching { vehicle, left } => {
                let vehicle = *vehicle;
                *left -= dt;
                let at = seen.position(vehicle).filter(|_| owned.contains(&vehicle));
                match at {
                    Some(at) if distance(me, at) <= ENTER_AT => {
                        let left = self.rng.range(20.0, 60.0);
                        self.phase = Phase::Driving { vehicle, left };
                        walker.drive();
                        Request::Enter { vehicle, seat: 0 }
                    }
                    Some(at) if *left > 0.0 => {
                        walker.go_to([at[0], at[1]], APPROACH_SPEED);
                        Request::None
                    }
                    // Lost, or taken over: walk on.
                    _ => {
                        self.walk_on(walker);
                        Request::None
                    }
                }
            }
            Phase::Driving { vehicle, left } => {
                let vehicle = *vehicle;
                *left -= dt;
                if *left > 0.0 && owned.contains(&vehicle) {
                    return Request::None;
                }
                walker.stop_driving();
                self.phase = if self.rng.unit() < 0.5 {
                    Phase::Parked { vehicle }
                } else {
                    self.walk_on(walker);
                    self.phase
                };
                Request::Leave
            }
            Phase::Parked { vehicle } => {
                let vehicle = *vehicle;
                let left = self.rng.range(5.0, 15.0);
                self.phase = Phase::Riding { vehicle, left };
                Request::Enter { vehicle, seat: 1 }
            }
            Phase::Riding { left, .. } => {
                *left -= dt;
                if *left > 0.0 {
                    return Request::None;
                }
                self.walk_on(walker);
                Request::Leave
            }
        }
    }

    fn walk_on(&mut self, walker: &mut Walker) {
        walker.walk_on();
        self.phase = Phase::Walking {
            left: self.rng.range(20.0, 60.0),
        };
    }
}

/// A shot from `from` at up to [`HITS_PER_SHOT`] of the nearest intact vehicles in `seen` within [`SHOT_RANGE`], each hit
/// at the position the snapshot gave; `None` when there is nothing to hit.
#[must_use]
pub fn shot_at_vehicles(seen: &Seen, from: [f32; 3]) -> Option<Shot> {
    let mut targets: Vec<(f32, &EntityState)> = seen
        .entities
        .iter()
        .zip(&seen.infos)
        .filter(|(_, info)| {
            info.kind == EntityKind::Vehicle
                && VehicleBlock::from_fields(info.fields()).is_ok_and(|v| v.body > 0)
        })
        .map(|(e, _)| (distance(from, e.position.to_metres()), e))
        .filter(|(d, _)| *d <= SHOT_RANGE)
        .collect();
    if targets.is_empty() {
        return None;
    }
    targets.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut shot = Shot::new(seen.tick, WEAPON, Position::from_metres(from).ok()?);
    for (_, target) in targets.into_iter().take(HITS_PER_SHOT) {
        shot.push(Hit {
            target: target.id,
            zone: Zone::Other,
            point: target.position,
        });
    }
    Some(shot)
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::Area;
    use sawgrass_protocol::{Rotation, Velocity};

    fn entity(id: u32, at: [f32; 3]) -> EntityState {
        EntityState {
            id,
            position: Position::from_metres(at).expect("position"),
            velocity: Velocity::from_metres_per_second([0.0; 3]).expect("velocity"),
            rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
        }
    }

    fn car() -> Info {
        Info::with_fields(EntityKind::Vehicle, AssetId(3), &VehicleBlock::INTACT.to_fields()).expect("info")
    }

    #[test]
    fn a_driver_walks_to_its_vehicle_drives_rides_and_walks_on() {
        let mut walker = Walker::new(Area::default(), 3);
        let start = walker.position();
        let car_at = [start[0] + 40.0, start[1], start[2]];
        let seen = Seen {
            tick: 1,
            entities: vec![entity(7, car_at)],
            infos: vec![car()],
        };
        let mut driver = Driver::new(3);
        let mut requests = Vec::new();
        let mut parked_or_walked = false;
        for _ in 0..20 * 400 {
            let request = driver.step(0.05, &mut walker, &[7], &seen);
            if !driver.holds() {
                walker.step(0.05);
            }
            if request != Request::None {
                requests.push(request);
            }
            if let Phase::Driving { .. } = driver.phase() {
                assert_eq!(driver.driving(), Some(7));
            }
            if requests.len() >= 2 && matches!(driver.phase(), Phase::Walking { .. } | Phase::Riding { .. }) {
                parked_or_walked = true;
            }
        }
        assert_eq!(requests[0], Request::Enter { vehicle: 7, seat: 0 });
        assert_eq!(requests[1], Request::Leave);
        assert!(parked_or_walked);
        // Every enter is followed by a leave before the next enter.
        let mut seated = false;
        for request in &requests {
            match request {
                Request::Enter { .. } => {
                    assert!(!seated, "{requests:?}");
                    seated = true;
                }
                Request::Leave => {
                    assert!(seated, "{requests:?}");
                    seated = false;
                }
                Request::None => {}
            }
        }
        assert!(
            requests.contains(&Request::Enter { vehicle: 7, seat: 1 }),
            "{requests:?}"
        );
    }

    #[test]
    fn a_driver_gives_up_a_vehicle_it_no_longer_owns() {
        let mut walker = Walker::new(Area::default(), 4);
        let start = walker.position();
        let seen = Seen {
            tick: 1,
            entities: vec![entity(7, [start[0] + 500.0, start[1], start[2]])],
            infos: vec![car()],
        };
        let mut driver = Driver::new(4);
        while !matches!(driver.phase(), Phase::Approaching { .. }) {
            assert_eq!(driver.step(0.05, &mut walker, &[7], &seen), Request::None);
        }
        assert_eq!(driver.step(0.05, &mut walker, &[], &seen), Request::None);
        assert!(matches!(driver.phase(), Phase::Walking { .. }));
    }

    #[test]
    fn a_shot_hits_the_nearest_vehicles_where_the_snapshot_saw_them() {
        let ped = Info::with_fields(EntityKind::Player, AssetId(0), &[]).expect("info");
        let seen = Seen {
            tick: 42,
            entities: vec![
                entity(1, [10.0, 0.0, 30.0]),
                entity(2, [50.0, 0.0, 30.0]),
                entity(3, [5.0, 0.0, 30.0]),
                entity(4, [20.0, 0.0, 30.0]),
                entity(5, [500.0, 0.0, 30.0]),
            ],
            infos: vec![car(), car(), ped, car(), car()],
        };
        // A wrecked vehicle takes no more damage, so the nearest one is passed over.
        let wreck = Info::with_fields(
            EntityKind::Vehicle,
            AssetId(3),
            &VehicleBlock {
                body: 0,
                ..VehicleBlock::INTACT
            }
            .to_fields(),
        )
        .expect("info");
        let mut wrecked = seen.clone();
        wrecked.infos[0] = wreck;
        let shot = shot_at_vehicles(&wrecked, [0.0, 0.0, 30.0]).expect("a shot");
        assert_eq!(shot.hits()[0].target, 4);
        let shot = shot_at_vehicles(&seen, [0.0, 0.0, 30.0]).expect("a shot");
        assert_eq!(shot.tick, 42);
        assert_eq!(shot.weapon, WEAPON);
        let hits: Vec<u32> = shot.hits().iter().map(|h| h.target).collect();
        assert_eq!(hits, [1]);
        assert_eq!(shot.hits()[0].point, seen.entities[0].position);
        let far = Seen {
            entities: vec![entity(5, [500.0, 0.0, 30.0])],
            infos: vec![car()],
            ..seen
        };
        assert!(shot_at_vehicles(&far, [0.0, 0.0, 30.0]).is_none());
    }
}
