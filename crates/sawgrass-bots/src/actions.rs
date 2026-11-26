//! What a driving or shooting bot does besides walking (BOT-5), as plain state machines the bot's send loop
//! steps, so they are tested without a server.
//!
//! A driver walks to a vehicle the server says it owns, takes the driver's seat, drives it along its walker's
//! legs, leaves it, sometimes sits a while in the passenger seat of the same parked vehicle, and walks on. The
//! server changes owners as it moves, so a run sees seats taken and left and ownership handed over.
//!
//! A driver trusts no seat the server has not granted: it drives, and sends its vehicle's state, only once the
//! server's [`ServerEvent::Seat`](sawgrass_protocol::game::ServerEvent::Seat) answer grants the seat (NET-21), and a
//! refused request or the server taking it out of its seat sends it on foot again (BOT-11).
//!
//! A shooter fires at intact vehicles in the latest snapshot it decoded, at the positions that snapshot gave, so
//! every hit of an honest bot passes the server's checks and changes something: a wrecked vehicle (body 0) takes
//! no more damage, so a hit on it would measure only the checks (BOT-9). It hits no player or NPC: the server has no respawn yet, so a
//! killed bot would stay dead and its later shots would be refused.
//!
//! With an aim delay (BOT-10), a shooter aims as a game client does: where it draws its targets, interpolated
//! between two snapshot ticks some ticks behind the newest, and names that render time in the shot
//! ([`Shot::frac`], NET-22). A driven vehicle moves between ticks, so this exercises the server's rewind.

use sawgrass_protocol::EntityState;
use sawgrass_protocol::Position;
use sawgrass_protocol::combat::{Hit, Shot, Zone};
use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
use sawgrass_protocol::game::SeatAnswer;
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
    /// Asked for the driver's seat of `vehicle`, for `left` more seconds; once the server granted it (`seated`),
    /// the bot drives and sends the vehicle's state.
    Driving { vehicle: u32, left: f32, seated: bool },
    /// Out of the driver's seat, about to take the passenger seat of the same vehicle.
    Parked { vehicle: u32 },
    /// Asked for the passenger seat of the parked `vehicle`, for `left` more seconds, granted once `seated`; the
    /// walker stands still.
    Riding { vehicle: u32, left: f32, seated: bool },
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

    /// The vehicle whose state the bot sends this step, if it drives one: the server granted it the driver's seat.
    #[must_use]
    pub fn driving(&self) -> Option<u32> {
        match self.phase {
            Phase::Driving {
                vehicle,
                seated: true,
                ..
            } => Some(vehicle),
            _ => None,
        }
    }

    /// Takes the server's answer about seat `seat` of `vehicle`. A grant of the seat the driver asked for seats it,
    /// and a driver's walker starts driving; a refusal, or the news that the server took it out of that seat, sends
    /// it on foot without a leave request. Answers about any other seat, such as the driver's seat a riding bot left
    /// just before it asked for the passenger seat, change nothing.
    pub fn answer(&mut self, vehicle: u32, seat: u8, answer: SeatAnswer, walker: &mut Walker) {
        let (asked, at, seated) = match &mut self.phase {
            Phase::Driving { vehicle, seated, .. } => (*vehicle, 0, seated),
            Phase::Riding { vehicle, seated, .. } => (*vehicle, 1, seated),
            _ => return,
        };
        if (vehicle, seat) != (asked, at) {
            return;
        }
        match answer {
            SeatAnswer::Granted => {
                *seated = true;
                if at == 0 {
                    walker.drive();
                }
            }
            _ => self.walk_on(walker),
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
                        self.phase = Phase::Driving {
                            vehicle,
                            left,
                            seated: false,
                        };
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
            Phase::Driving { vehicle, left, .. } => {
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
                self.phase = Phase::Riding {
                    vehicle,
                    left,
                    seated: false,
                };
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

/// Most ticks a shooter's render time may be behind the newest snapshot: the server rewinds at most 10 ticks
/// (500 ms), and the shot takes a tick or two to arrive.
pub const MAX_AIM_DELAY: u32 = 8;

/// The render time of a client `delay` ticks behind the newest snapshot, `newest`, which arrived `elapsed` ticks
/// ago: a tick and the fraction toward the next, out of 256. The render time never passes `newest`.
#[must_use]
pub fn render_time(newest: u32, elapsed: f32, delay: u32) -> (u32, u8) {
    let delay = delay.min(newest);
    let ahead = elapsed.clamp(0.0, delay as f32);
    let whole = ahead.floor();
    let frac = ((ahead - whole) * 256.0) as u8;
    (newest - delay + whole as u32, frac)
}

/// A shot from `from` at up to [`HITS_PER_SHOT`] of the nearest intact vehicles in `then` within [`SHOT_RANGE`].
/// Each hit is where the shooter draws the target: `frac` of the way from its position in `then` to its position
/// in `next`, the snapshot of the tick after. A target missing from `next` is not aimed at, and with `frac` 0 `next`
/// is not needed. `None` when there is nothing to hit.
#[must_use]
pub fn shot_at_vehicles(then: &Seen, next: Option<&Seen>, frac: u8, from: [f32; 3]) -> Option<Shot> {
    let drawn = |e: &EntityState| -> Option<Position> {
        if frac == 0 {
            return Some(e.position);
        }
        let (a, b) = (e.position.to_metres(), next?.position(e.id)?);
        let t = f32::from(frac) / 256.0;
        Position::from_metres(std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)).ok()
    };
    let mut targets: Vec<(f32, u32, Position)> = then
        .entities
        .iter()
        .zip(&then.infos)
        .filter(|(_, info)| {
            info.kind == EntityKind::Vehicle
                && VehicleBlock::from_fields(info.fields()).is_ok_and(|v| v.body > 0)
        })
        .filter_map(|(e, _)| {
            let at = drawn(e)?;
            Some((distance(from, at.to_metres()), e.id, at))
        })
        .filter(|(d, _, _)| *d <= SHOT_RANGE)
        .collect();
    if targets.is_empty() {
        return None;
    }
    targets.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut shot = Shot::new(then.tick, WEAPON, Position::from_metres(from).ok()?);
    shot.frac = frac;
    for (_, target, point) in targets.into_iter().take(HITS_PER_SHOT) {
        shot.push(Hit {
            target,
            zone: Zone::Other,
            point,
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
            // The server grants every seat here.
            if let Request::Enter { vehicle, seat } = request {
                driver.answer(vehicle, seat, SeatAnswer::Granted, &mut walker);
            }
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

    /// A driver next to vehicle 7, stepped until it asks for the driver's seat.
    fn at_the_door(index: u64) -> (Driver, Walker, Seen) {
        let mut walker = Walker::new(Area::default(), index);
        let seen = Seen {
            tick: 1,
            entities: vec![entity(7, walker.position())],
            infos: vec![car()],
        };
        let mut driver = Driver::new(index);
        for _ in 0..20 * 20 {
            if driver.step(0.05, &mut walker, &[7], &seen) == (Request::Enter { vehicle: 7, seat: 0 }) {
                return (driver, walker, seen);
            }
        }
        panic!("no seat request: {:?}", driver.phase());
    }

    #[test]
    fn a_driver_drives_only_once_the_server_grants_its_seat() {
        let (mut driver, mut walker, seen) = at_the_door(5);
        assert!(matches!(driver.phase(), Phase::Driving { seated: false, .. }));
        assert_eq!(driver.driving(), None);
        // An answer about another seat or vehicle is not the one it waits for.
        driver.answer(7, 1, SeatAnswer::Granted, &mut walker);
        driver.answer(8, 0, SeatAnswer::Granted, &mut walker);
        assert_eq!(driver.driving(), None);
        driver.answer(7, 0, SeatAnswer::Granted, &mut walker);
        assert_eq!(driver.driving(), Some(7));
        assert_eq!(driver.step(0.05, &mut walker, &[7], &seen), Request::None);
        assert_eq!(driver.driving(), Some(7));
    }

    #[test]
    fn a_refused_seat_sends_the_driver_on_foot_without_a_leave_request() {
        for refusal in [
            SeatAnswer::NotAVehicle,
            SeatAnswer::NoSuchSeat,
            SeatAnswer::Taken,
            SeatAnswer::Locked,
            SeatAnswer::CannotReach,
            SeatAnswer::AlreadySeated,
        ] {
            let (mut driver, mut walker, seen) = at_the_door(6);
            driver.answer(7, 0, refusal, &mut walker);
            assert!(matches!(driver.phase(), Phase::Walking { .. }), "{refusal:?}");
            assert_eq!(driver.driving(), None);
            // Not seated, so nothing to leave.
            assert_eq!(driver.step(0.05, &mut walker, &[7], &seen), Request::None);
        }
    }

    #[test]
    fn a_driver_the_server_takes_out_of_its_seat_walks_on() {
        let (mut driver, mut walker, seen) = at_the_door(7);
        driver.answer(7, 0, SeatAnswer::Granted, &mut walker);
        assert_eq!(driver.driving(), Some(7));
        driver.answer(7, 0, SeatAnswer::Left, &mut walker);
        assert!(matches!(driver.phase(), Phase::Walking { .. }));
        assert_eq!(driver.step(0.05, &mut walker, &[7], &seen), Request::None);
    }

    #[test]
    fn a_rider_keeps_its_passenger_seat_when_the_driver_seat_it_left_is_answered() {
        // Driven until it leaves; half the time it parks and asks for the passenger seat of the same vehicle.
        let mut rider = None;
        for index in 8..64 {
            let (mut d, mut w, s) = at_the_door(index);
            d.answer(7, 0, SeatAnswer::Granted, &mut w);
            let mut requests = Vec::new();
            while requests.len() < 2 {
                let request = d.step(1.0, &mut w, &[7], &s);
                if request != Request::None {
                    requests.push(request);
                }
            }
            if requests == [Request::Leave, Request::Enter { vehicle: 7, seat: 1 }] {
                rider = Some((d, w));
                break;
            }
        }
        let (mut rider, mut walker) = rider.expect("a bot that parks and rides");
        // The answers come in order: the driver's seat it left, then the passenger seat.
        rider.answer(7, 0, SeatAnswer::Left, &mut walker);
        assert!(matches!(rider.phase(), Phase::Riding { seated: false, .. }));
        rider.answer(7, 1, SeatAnswer::Granted, &mut walker);
        assert!(matches!(rider.phase(), Phase::Riding { seated: true, .. }));
        assert!(rider.holds());
        assert_eq!(rider.driving(), None);
        // Refused or taken out, the rider walks on.
        rider.answer(7, 1, SeatAnswer::Left, &mut walker);
        assert!(matches!(rider.phase(), Phase::Walking { .. }));
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
        let shot = shot_at_vehicles(&wrecked, None, 0, [0.0, 0.0, 30.0]).expect("a shot");
        assert_eq!(shot.hits()[0].target, 4);
        let shot = shot_at_vehicles(&seen, None, 0, [0.0, 0.0, 30.0]).expect("a shot");
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
        assert!(shot_at_vehicles(&far, None, 0, [0.0, 0.0, 30.0]).is_none());
    }

    #[test]
    fn the_render_time_is_the_delay_behind_the_newest_tick_and_moves_on_between_snapshots() {
        assert_eq!(render_time(100, 0.0, 0), (100, 0));
        assert_eq!(render_time(100, 0.7, 0), (100, 0));
        assert_eq!(render_time(100, 0.0, 4), (96, 0));
        assert_eq!(render_time(100, 1.5, 4), (97, 128));
        // A late snapshot does not carry the render time past the newest tick.
        assert_eq!(render_time(100, 9.0, 4), (100, 0));
        assert_eq!(render_time(2, 0.0, 4), (0, 0));
    }

    #[test]
    fn a_delayed_shot_hits_a_moving_vehicle_where_it_is_drawn_between_two_ticks() {
        let then = Seen {
            tick: 96,
            entities: vec![entity(1, [10.0, 0.0, 30.0]), entity(2, [40.0, 0.0, 30.0])],
            infos: vec![car(), car()],
        };
        let next = Seen {
            tick: 97,
            entities: vec![entity(1, [16.0, 0.0, 30.0])],
            infos: vec![car()],
        };
        let shot = shot_at_vehicles(&then, Some(&next), 128, [0.0, 0.0, 30.0]).expect("a shot");
        assert_eq!((shot.tick, shot.frac), (96, 128));
        assert_eq!(shot.hits()[0].target, 1);
        let x = shot.hits()[0].point.to_metres()[0];
        assert!((x - 13.0).abs() < 0.02, "{x}");
        // Vehicle 2 left the next snapshot, so it cannot be drawn and is not aimed at.
        let gone = Seen {
            entities: vec![then.entities[1]],
            infos: vec![car()],
            ..then.clone()
        };
        assert!(shot_at_vehicles(&gone, Some(&next), 128, [0.0, 0.0, 30.0]).is_none());
        assert!(shot_at_vehicles(&then, None, 128, [0.0, 0.0, 30.0]).is_none());
        // At a whole tick the next snapshot is not needed.
        let shot = shot_at_vehicles(&gone, None, 0, [0.0, 0.0, 30.0]).expect("a shot");
        assert_eq!(shot.hits()[0].target, 2);
    }
}
