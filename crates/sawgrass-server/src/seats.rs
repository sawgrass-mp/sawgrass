//! Who sits where (ADR 0006, section 4): seats belong to the server. A client asks to enter a seat, and the
//! server grants it if the vehicle exists and is not locked, the seat is free, the ped is close enough and not
//! dead, and it sits nowhere yet. Leaving is always granted, and also covers a ped the owner's physics threw out.
//!
//! This module keeps the occupancy; `net.rs` checks the distance and the ped, sets the ped block and tells the
//! ownership rules who drives.
//!
//! - *Assumption:* every vehicle has [`VEHICLE_SEATS`] seats until the game's per-model counts are known.
//! - *Not yet:* the scripts' say on a request (ADR 0006: "the scripts allow it") arrives with a scripting task; until
//!   then only the rules here decide.

/// Seats in every vehicle, seat 0 the driver's. A placeholder until the game's per-model counts are known.
pub const VEHICLE_SEATS: usize = 4;

/// Farthest a ped may be from a vehicle, in metres, to enter it. A placeholder until the game's figures are known.
pub const ENTER_RADIUS: f32 = 5.0;

/// Why a request to enter a seat was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The id is not a spawned vehicle.
    NotAVehicle,
    /// The seat index is past the vehicle's seats.
    NoSuchSeat,
    /// Someone sits there.
    Taken,
    /// The vehicle is locked.
    Locked,
    /// The ped is farther than [`ENTER_RADIUS`], has no position yet, or is dead.
    CannotReach,
    /// The ped already sits in a vehicle; it leaves first.
    AlreadySeated,
}

/// Seat occupancy and locks, by vehicle slot and by player slot.
#[derive(Debug, Default)]
pub struct Seats {
    by_vehicle: Vec<[Option<u32>; VEHICLE_SEATS]>,
    by_player: Vec<Option<(u32, u8)>>,
    locked: Vec<bool>,
}

impl Seats {
    /// No one sits anywhere and nothing is locked.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seats `player` in `seat` of `vehicle` (slots), if the seat exists and is free, the vehicle is not locked and
    /// the player sits nowhere. The caller has checked that `vehicle` is a spawned vehicle within reach.
    ///
    /// # Errors
    ///
    /// The first rule the request breaks; nothing changes then.
    pub fn enter(&mut self, player: u32, vehicle: u32, seat: u8) -> Result<(), Refusal> {
        if usize::from(seat) >= VEHICLE_SEATS {
            return Err(Refusal::NoSuchSeat);
        }
        if self.seat_of(player).is_some() {
            return Err(Refusal::AlreadySeated);
        }
        if self.is_locked(vehicle) {
            return Err(Refusal::Locked);
        }
        let v = vehicle as usize;
        if self.by_vehicle.len() <= v {
            self.by_vehicle.resize(v + 1, [None; VEHICLE_SEATS]);
        }
        let place = &mut self.by_vehicle[v][usize::from(seat)];
        if place.is_some() {
            return Err(Refusal::Taken);
        }
        *place = Some(player);
        let p = player as usize;
        if self.by_player.len() <= p {
            self.by_player.resize(p + 1, None);
        }
        self.by_player[p] = Some((vehicle, seat));
        Ok(())
    }

    /// Takes `player` out of its seat, and returns the vehicle slot and seat it left; `None` if it sat nowhere.
    pub fn leave(&mut self, player: u32) -> Option<(u32, u8)> {
        let (vehicle, seat) = self.by_player.get_mut(player as usize)?.take()?;
        if let Some(seats) = self.by_vehicle.get_mut(vehicle as usize) {
            seats[usize::from(seat)] = None;
        }
        Some((vehicle, seat))
    }

    /// Takes everyone out of `vehicle` and forgets its lock, as when it is removed; returns who sat there, with
    /// their seats.
    pub fn vacate(&mut self, vehicle: u32) -> Vec<(u32, u8)> {
        if let Some(l) = self.locked.get_mut(vehicle as usize) {
            *l = false;
        }
        let Some(seats) = self.by_vehicle.get_mut(vehicle as usize) else {
            return Vec::new();
        };
        let players: Vec<(u32, u8)> = seats
            .iter_mut()
            .enumerate()
            .filter_map(|(seat, p)| Some((p.take()?, seat as u8)))
            .collect();
        for &(p, _) in &players {
            self.by_player[p as usize] = None;
        }
        players
    }

    /// The vehicle slot and seat `player` sits in.
    #[must_use]
    pub fn seat_of(&self, player: u32) -> Option<(u32, u8)> {
        self.by_player.get(player as usize).copied().flatten()
    }

    /// Who sits in `seat` of `vehicle`.
    #[must_use]
    pub fn occupant(&self, vehicle: u32, seat: u8) -> Option<u32> {
        self.by_vehicle
            .get(vehicle as usize)?
            .get(usize::from(seat))
            .copied()
            .flatten()
    }

    /// Locks or unlocks `vehicle`: a locked one refuses everyone who asks to enter; those inside stay.
    pub fn set_locked(&mut self, vehicle: u32, locked: bool) {
        let v = vehicle as usize;
        if self.locked.len() <= v {
            self.locked.resize(v + 1, false);
        }
        self.locked[v] = locked;
    }

    /// Whether `vehicle` is locked.
    #[must_use]
    pub fn is_locked(&self, vehicle: u32) -> bool {
        self.locked.get(vehicle as usize).copied().unwrap_or(false)
    }

    /// Every seated player with its vehicle slot, in player order.
    pub fn seated(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.by_player
            .iter()
            .enumerate()
            .filter_map(|(p, s)| s.map(|(v, _)| (p as u32, v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_holds_one_player_and_a_player_one_seat() {
        let mut seats = Seats::new();
        assert_eq!(seats.enter(1, 10, 0), Ok(()));
        assert_eq!(seats.enter(2, 10, 0), Err(Refusal::Taken));
        assert_eq!(seats.enter(1, 10, 1), Err(Refusal::AlreadySeated));
        assert_eq!(seats.enter(2, 10, VEHICLE_SEATS as u8), Err(Refusal::NoSuchSeat));
        assert_eq!(seats.enter(2, 10, 1), Ok(()));
        assert_eq!(seats.seat_of(2), Some((10, 1)));
        assert_eq!(seats.occupant(10, 0), Some(1));
        assert_eq!(seats.seated().collect::<Vec<_>>(), [(1, 10), (2, 10)]);
        assert_eq!(seats.leave(1), Some((10, 0)));
        assert_eq!(seats.leave(1), None);
        assert_eq!(seats.occupant(10, 0), None);
        assert_eq!(seats.enter(3, 10, 0), Ok(()));
    }

    #[test]
    fn a_locked_vehicle_refuses_newcomers_and_removal_empties_it() {
        let mut seats = Seats::new();
        assert_eq!(seats.enter(1, 4, 0), Ok(()));
        seats.set_locked(4, true);
        assert_eq!(seats.enter(2, 4, 1), Err(Refusal::Locked));
        assert_eq!(seats.seat_of(1), Some((4, 0)));
        assert_eq!(seats.vacate(4), [(1, 0)]);
        assert_eq!(seats.seat_of(1), None);
        assert!(!seats.is_locked(4));
        assert_eq!(seats.enter(2, 4, 1), Ok(()));
    }
}
