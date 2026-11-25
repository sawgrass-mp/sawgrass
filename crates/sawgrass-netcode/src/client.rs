//! The four parts together, behind the calls a game module (or a bot) makes.

use sawgrass_protocol::game::{ClientState, MAX_OWNED, OwnedState};
use sawgrass_protocol::ped::PedBlock;
use sawgrass_protocol::{EncodeError, EntityState, PacketHeader, PacketKind};

use crate::Pose;
use crate::clock::{ClockConfig, ServerClock};
use crate::interp::{InterpConfig, Interpolator, Sample};
use crate::local::{Correction, LocalConfig, LocalPlayer};
use crate::receive::{Skipped, SnapshotReceiver};

/// Settings of every part.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClientConfig {
    /// The server clock and render delay.
    pub clock: ClockConfig,
    /// Interpolation of remote entities.
    pub interp: InterpConfig,
    /// The local player.
    pub local: LocalConfig,
}

/// The network state of one connected client.
///
/// Each frame the game module passes the datagrams that arrived to [`Client::receive`], asks
/// [`Client::remote`] where to show remote entities (spawning those whose sample says
/// [`Event::Appeared`](crate::interp::Event::Appeared) and removing those in [`Client::removed`]), applies [`Client::frame`]'s correction offset to the local
/// player, and sends what [`Client::outgoing`] returns.
#[derive(Debug)]
pub struct Client {
    entity: u32,
    receiver: SnapshotReceiver,
    clock: ServerClock,
    interp: Interpolator,
    local: LocalPlayer,
    sequence: u16,
    render: Option<f64>,
    /// Where the next datagram's owned states start in the caller's list, when they do not all fit in one.
    owned_cursor: usize,
    /// The owned states chosen for the datagram being built.
    chosen: Vec<OwnedState>,
}

impl Client {
    /// A client that joined as `entity` on a server running at `tick_hz` (both from the join event).
    #[must_use]
    pub fn new(entity: u32, tick_hz: u16, config: ClientConfig) -> Self {
        Self {
            entity,
            receiver: SnapshotReceiver::new(),
            clock: ServerClock::new(tick_hz, config.clock),
            interp: Interpolator::new(tick_hz, config.interp),
            local: LocalPlayer::new(config.local),
            sequence: 0,
            render: None,
            owned_cursor: 0,
            chosen: Vec::new(),
        }
    }

    /// The local player's entity id.
    #[must_use]
    pub fn entity(&self) -> u32 {
        self.entity
    }

    /// The server clock, for its estimates.
    #[must_use]
    pub fn clock(&self) -> &ServerClock {
        &self.clock
    }

    /// The newest snapshot tick decoded, as acknowledged to the server.
    #[must_use]
    pub fn ack(&self) -> u32 {
        self.receiver.ack()
    }

    /// Takes one datagram from the server that arrived at local time `now`, in seconds. Returns the tick of the
    /// snapshot it completed.
    pub fn receive(&mut self, datagram: &[u8], now: f64) -> Result<u32, Skipped> {
        let snapshot = self.receiver.push(datagram)?;
        self.clock.observe(snapshot.tick, now);
        self.interp
            .insert(snapshot.tick, snapshot.states, snapshot.infos, Some(self.entity));
        Ok(snapshot.tick)
    }

    /// Writes the remote entities to show at local time `now` into `out`, cleared first, ascending by id. Empty
    /// until the first snapshot. Call once per frame.
    pub fn remote(&mut self, now: f64, out: &mut Vec<Sample>) {
        self.render = self.clock.render_tick(now);
        match self.render {
            Some(render) => self.interp.sample(render, out),
            None => out.clear(),
        }
    }

    /// The remote entities that stopped being shown in the last [`Client::remote`] call, ascending: remove them from
    /// the game.
    #[must_use]
    pub fn removed(&self) -> &[u32] {
        self.interp.removed()
    }

    /// The render tick of the last [`Client::remote`] call.
    #[must_use]
    pub fn render_tick(&self) -> Option<f64> {
        self.render
    }

    /// Takes where the server holds the local player while the game has it at `current`.
    pub fn correct(&mut self, current: &Pose, server: &Pose) -> Correction {
        self.local.correct(current, server)
    }

    /// Takes the state of a [`ServerEvent::Corrected`](sawgrass_protocol::game::ServerEvent::Corrected), the
    /// server's answer to a refused state, while the game has the local player at `current`. `None` if the event is
    /// for another entity: one this client simulates (a vehicle, an NPC), which the game module moves back itself.
    pub fn corrected(&mut self, state: &EntityState, current: &Pose) -> Option<Correction> {
        (state.id == self.entity).then(|| self.local.correct(current, &Pose::from_state(state)))
    }

    /// The offset to move the local player by this frame, `dt` seconds after the last, while a correction is being
    /// smoothed in.
    pub fn frame(&mut self, dt: f32) -> Option<[f32; 3]> {
        self.local.frame(dt)
    }

    /// The client state datagram to send at local time `now` for the local player at `pose`, if one is due and the
    /// pose fits the wire ranges. Its header carries the acknowledgement.
    pub fn outgoing(&mut self, now: f64, pose: &Pose) -> Option<Vec<u8>> {
        if !self.local.due(now) {
            return None;
        }
        let state = pose.to_client_state().ok()?;
        let mut out = self.header();
        out.resize(PacketHeader::SIZE + ClientState::SIZE, 0);
        // The buffer holds exactly a header and a state, so encoding cannot fail.
        let _ = state.encode(&mut out[PacketHeader::SIZE..]);
        Some(out)
    }

    /// As [`Client::outgoing`], with the local player's ped block and the states of the entities this client
    /// simulates (vehicles, NPCs; ADR 0006), in one datagram. `Ok(None)` when no state is due; an error only for a
    /// pose outside the wire ranges, which spends the send slot.
    ///
    /// The player's own state always goes out. The server may give a client more entities than one datagram
    /// carries ([`MAX_OWNED`], or fewer when their blocks are large), so each datagram takes as many as fit,
    /// starting where the last one stopped and wrapping around: every owned entity is sent at least once every
    /// `ceil(owned.len() / per_datagram)` datagrams. An entity whose own state does not encode is skipped.
    pub fn outgoing_with(
        &mut self,
        now: f64,
        pose: &Pose,
        ped: &PedBlock,
        owned: &[OwnedState],
    ) -> Result<Option<Vec<u8>>, EncodeError> {
        if !self.local.due(now) {
            return Ok(None);
        }
        let state = pose.to_client_state()?;
        let mut out = self.header();
        let start = if owned.is_empty() {
            0
        } else {
            self.owned_cursor % owned.len()
        };
        self.chosen.clear();
        self.chosen.extend(
            owned
                .iter()
                .cycle()
                .skip(start)
                .take(owned.len().min(MAX_OWNED))
                .copied(),
        );
        // Fewer if the blocks make the payload too large; with none, the player's state and ped block always fit.
        loop {
            match state.encode_with_owned(ped, &self.chosen, &mut out) {
                Ok(()) => break,
                Err(_) if !self.chosen.is_empty() => {
                    self.chosen.pop();
                }
                Err(e) => return Err(e),
            }
        }
        self.owned_cursor = start + self.chosen.len();
        Ok(Some(out))
    }

    /// A client state header with the next sequence number and the acknowledgement.
    fn header(&mut self) -> Vec<u8> {
        let mut out = vec![0; PacketHeader::SIZE];
        let header = PacketHeader {
            kind: PacketKind::ClientState,
            sequence: self.sequence,
            tick: self.receiver.ack(),
        };
        self.sequence = self.sequence.wrapping_add(1);
        // The buffer is exactly one header long, so encoding cannot fail.
        let _ = header.encode(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::new(3, 20, ClientConfig::default())
    }

    #[test]
    fn owned_states_go_out_in_the_players_datagram() {
        let mut client = client();
        let me = Pose::at([10.0, 20.0, 30.0]);
        let car = Pose {
            velocity: [12.0, 0.0, 0.0],
            ..Pose::at([14.0, 20.0, 30.0])
        };
        let owned = [car.to_owned_state(0x0002_0009, 4, &[7, 0, 1]).expect("owned")];
        let ped = PedBlock::default();
        let packet = client
            .outgoing_with(0.0, &me, &ped, &owned)
            .expect("encodes")
            .expect("due");
        let (header, body) = PacketHeader::decode(&packet).expect("header");
        assert_eq!(header.kind, PacketKind::ClientState);
        assert_eq!(header.tick, 0);
        let mut decoded = Vec::new();
        let (state, decoded_ped) = ClientState::decode_with_owned(body, &mut decoded).expect("decodes");
        assert_eq!(state, me.to_client_state().expect("state"));
        assert_eq!(decoded_ped, Some(ped));
        assert_eq!(decoded, owned);
        assert_eq!(decoded[0].epoch, 4);
        assert_eq!(decoded[0].fields(), &[7, 0, 1]);

        // Paced like `outgoing`, with the sequence going on from the last datagram.
        assert_eq!(client.outgoing_with(0.01, &me, &ped, &owned), Ok(None));
        let next = client.outgoing(0.06, &me).expect("due");
        assert_eq!(PacketHeader::decode(&next).expect("header").0.sequence, 1);
    }

    /// The owned ids in a datagram.
    fn owned_ids(packet: &[u8]) -> Vec<u32> {
        let (_, body) = PacketHeader::decode(packet).expect("header");
        let mut owned = Vec::new();
        ClientState::decode_with_owned(body, &mut owned).expect("decodes");
        owned.iter().map(|o| o.id).collect()
    }

    #[test]
    fn more_owned_entities_than_fit_are_rotated_and_the_player_always_goes_out() {
        let mut client = client();
        let me = Pose::at([0.0, 0.0, 30.0]);
        // The server's ownership cap (SRV-14) is above what one datagram carries.
        let owned: Vec<OwnedState> = (0..24)
            .map(|id| me.to_owned_state(id, 1, &[1, 2, 3]).expect("owned"))
            .collect();
        let mut seen = vec![0u32; 24];
        for frame in 0..6u32 {
            let packet = client
                .outgoing_with(f64::from(frame) * 0.05, &me, &PedBlock::default(), &owned)
                .expect("the player's state always encodes")
                .expect("due");
            let (_, body) = PacketHeader::decode(&packet).expect("header");
            assert_eq!(
                ClientState::decode(body).expect("decodes"),
                me.to_client_state().expect("state")
            );
            let ids = owned_ids(&packet);
            assert_eq!(ids.len(), MAX_OWNED);
            for id in ids {
                seen[id as usize] += 1;
            }
        }
        // 6 datagrams of 16 over 24 entities: each sent 4 times, none starved.
        assert!(seen.iter().all(|&n| n == 4), "{seen:?}");
    }

    #[test]
    fn owned_states_with_large_blocks_are_cut_to_fit_one_datagram() {
        let mut client = client();
        let me = Pose::at([0.0, 0.0, 30.0]);
        let big = [u32::MAX; sawgrass_protocol::entity::MAX_FIELDS];
        let owned: Vec<OwnedState> = (0..16)
            .map(|id| me.to_owned_state(id, 1, &big).expect("owned"))
            .collect();
        let first = client
            .outgoing_with(0.0, &me, &PedBlock::default(), &owned)
            .expect("encodes")
            .expect("due");
        assert!(first.len() <= sawgrass_protocol::game::MAX_DATAGRAM);
        let n = owned_ids(&first).len();
        assert!(n > 0 && n < 16, "{n} fit");
        // The next datagram goes on from the first entity left out.
        let second = client
            .outgoing_with(0.05, &me, &PedBlock::default(), &owned)
            .expect("encodes")
            .expect("due");
        assert_eq!(owned_ids(&second)[0], n as u32);
    }

    #[test]
    fn a_pose_off_the_wire_ranges_is_an_error() {
        let mut client = client();
        let far = Pose::at([1.0e9, 0.0, 0.0]);
        assert!(
            client
                .outgoing_with(0.0, &far, &PedBlock::default(), &[])
                .is_err()
        );
    }
}
