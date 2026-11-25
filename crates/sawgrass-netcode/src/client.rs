//! The four parts together, behind the calls a game module (or a bot) makes.

use sawgrass_protocol::game::ClientState;
use sawgrass_protocol::{EntityState, PacketHeader, PacketKind};

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
        Some(self.packet(&state))
    }

    fn packet(&mut self, state: &ClientState) -> Vec<u8> {
        let mut out = vec![0; PacketHeader::SIZE + ClientState::SIZE];
        let header = PacketHeader {
            kind: PacketKind::ClientState,
            sequence: self.sequence,
            tick: self.receiver.ack(),
        };
        self.sequence = self.sequence.wrapping_add(1);
        // The buffer holds exactly a header and a state, so neither encoding can fail.
        let _ = header.encode(&mut out);
        let _ = state.encode(&mut out[PacketHeader::SIZE..]);
        out
    }
}
