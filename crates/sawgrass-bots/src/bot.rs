//! One bot: connects through the gateway like a player, sends its state every client tick and reads snapshots.
//!
//! Snapshots are delta-encoded (NET-14): the bot reassembles each tick's fragments, decodes it against the
//! baseline it names from the ticks it decoded lately, and acknowledges the newest one in the tick field of its
//! client states' headers, as a game client does.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use sawgrass_gateway::{Connector, Datagrams, EventSender, RootCertStore, connect};
use sawgrass_protocol::EntityState;
use sawgrass_protocol::combat::Shot;
use sawgrass_protocol::delta::{Baseline, Reassembler, baseline_age, decode_delta, decode_delta_with_info};
use sawgrass_protocol::entity::Info;
use sawgrass_protocol::game::{ClientEvent, ClientState, OwnedState, PayResult, SeatAnswer, ServerEvent};
use sawgrass_protocol::ped::{Motion, PedBlock};
use sawgrass_protocol::vehicle::VehicleBlock;
use sawgrass_protocol::{PROTOCOL_VERSION, PacketHeader, PacketKind, Position, Rotation, Velocity};
use tokio::time::{Instant, MissedTickBehavior};

use crate::actions::{Driver, Request, Seen, WEAPON, render_time, shot_at_vehicles};
use crate::record::Recorder;
use crate::stats::Counters;
use crate::walker::{Area, Rng, Walker};

/// The longest pause before a bot's first new connection attempt, doubled after each failure up to [`LAST_RETRY`].
pub const FIRST_RETRY: Duration = Duration::from_millis(250);
/// The longest pause between a bot's connection attempts.
pub const LAST_RETRY: Duration = Duration::from_secs(4);

/// What every bot of a run shares.
#[derive(Debug, Clone)]
pub struct BotConfig {
    /// The gateway's address.
    pub server: SocketAddr,
    /// The name in the gateway's certificate.
    pub server_name: String,
    /// The certificates the bot trusts.
    pub roots: RootCertStore,
    /// State packets per second, like a game client's send rate.
    pub send_hz: u32,
    /// Where the bots move.
    pub area: Area,
    /// Records the snapshots of the first few bots, if set.
    pub record: Option<Arc<Recorder>>,
    /// Bots numbered below this decode every snapshot; the others check that a snapshot is whole and its
    /// baseline is one they acknowledged, then acknowledge it without decoding the entities. The server sees the
    /// same acknowledgements either way, so its work is the same, and the generator saves most of its CPU.
    pub decoders: u64,
    /// Sockets the bots share, bot `i` using socket `i % len`; empty gives every bot its own, as players have.
    /// Sharing saves the generator a receive loop per bot and leaves the server one connection per bot.
    pub sockets: Vec<Connector>,
    /// How often each bot pays another, on average, if set (BOT-7). The server must run with a ledger and
    /// `--money-events`.
    pub pay_every: Option<Duration>,
    /// The entity ids of the bots that joined, so a bot pays a real player.
    pub payees: Arc<Mutex<Vec<u32>>>,
    /// Bots numbered below this drive the vehicles the server hands them and change seats (BOT-5).
    pub drivers: u64,
    /// Bots numbered below this fire at vehicles in their snapshots, `shot_hz` times a second (BOT-5).
    pub shooters: u64,
    /// Shots per second of a shooting bot.
    pub shot_hz: u32,
    /// How many ticks behind the newest snapshot a shooter draws its targets, interpolating between two
    /// snapshots, as a game client does (BOT-10). 0 aims at the newest snapshot itself.
    pub aim_delay: u32,
}

/// The newest snapshot a shooter may fire against: with the aim delay on top, older ones would come near the
/// server's rewind of 500 ms.
const FRESH: Duration = Duration::from_millis(150);

/// What a driving or shooting bot's tasks share: the vehicles the server says it owns, with their epochs; the
/// server's seat answers the driver has not read yet, in order; the latest snapshot it decoded, with when it
/// arrived; for a shooter with an aim delay, the snapshots before it back to the render time, `keep` of them; and
/// the server's tick rate.
#[derive(Debug, Default)]
struct Shared {
    owned: Mutex<Vec<(u32, u8)>>,
    seats: Mutex<Vec<(u32, u8, SeatAnswer)>>,
    seen: Mutex<(Option<Instant>, Seen)>,
    past: Mutex<VecDeque<Seen>>,
    keep: usize,
    tick_hz: AtomicU16,
}

/// What one bot learned from the server, for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Joined {
    /// The bot's entity id.
    pub entity: u32,
    /// Server ticks per second.
    pub tick_hz: u16,
}

/// Connects bot number `index`, then plays until `stop` is reached or the connection ends. Bot 0 asks the server
/// for its stats when `want_stats` is set; one asking is enough. Returns what the server sent on joining, or
/// `None` if the connection failed.
pub async fn run(
    config: Arc<BotConfig>,
    index: u64,
    counters: Arc<Counters>,
    want_stats: bool,
    stop: Instant,
    joined: Option<tokio::sync::oneshot::Sender<Joined>>,
) {
    // A refused or timed-out attempt is tried again after a growing pause, as a game client does when a server
    // is busy, until the run's stop. Each pause is drawn between half and all of the step, so bots refused together
    // do not all come back in the same instant.
    let mut jitter = Rng::new(!index);
    let mut step = FIRST_RETRY;
    let session = loop {
        let attempt = if config.sockets.is_empty() {
            connect(
                config.server,
                &config.server_name,
                config.roots.clone(),
                PROTOCOL_VERSION,
            )
            .await
        } else {
            let socket = &config.sockets[(index % config.sockets.len() as u64) as usize];
            socket
                .connect(config.server, &config.server_name, PROTOCOL_VERSION)
                .await
        };
        match attempt {
            Ok(session) => break session,
            Err(e) => {
                counters.error(e.to_string());
                let pause = step.mul_f32(jitter.range(0.5, 1.0));
                if Instant::now() + pause >= stop {
                    Counters::add(&counters.failed, 1);
                    return;
                }
                Counters::add(&counters.connect_retries, 1);
                tokio::time::sleep(pause).await;
                step = (step * 2).min(LAST_RETRY);
            }
        }
    };
    Counters::add(&counters.connected, 1);
    let (datagrams, mut event_sender, mut event_receiver) = session.split();

    if want_stats {
        let mut request = header(PacketKind::Event, 0, 0);
        ClientEvent::RequestStats.encode(&mut request);
        let _ = event_sender.send(&request).await;
    }

    let acts = index < config.drivers || index < config.shooters;
    let shared = Arc::new(Shared {
        keep: if index < config.shooters && config.aim_delay > 0 {
            config.aim_delay as usize + 1
        } else {
            0
        },
        ..Shared::default()
    });
    let events_counters = counters.clone();
    let payments = Arc::new(Mutex::new(HashMap::<u64, Instant>::new()));
    let own = Arc::new(AtomicU32::new(u32::MAX));
    let (answered, own_id, payees) = (payments.clone(), own.clone(), config.payees.clone());
    let events_shared = shared.clone();
    tokio::spawn(async move {
        let mut joined = joined;
        while let Ok((_, packet)) = event_receiver.recv().await {
            match ServerEvent::decode(&packet[PacketHeader::SIZE..]) {
                Ok(ServerEvent::Joined { entity, tick_hz }) => {
                    own_id.store(entity, Ordering::Relaxed);
                    events_shared.tick_hz.store(tick_hz, Ordering::Relaxed);
                    if let Ok(mut payees) = payees.lock() {
                        payees.push(entity);
                    }
                    if let Some(tx) = joined.take() {
                        let _ = tx.send(Joined { entity, tick_hz });
                    }
                }
                Ok(ServerEvent::Paid { key, result, .. }) => {
                    let sent = answered.lock().ok().and_then(|mut p| p.remove(&key));
                    if let Some(sent) = sent {
                        events_counters.payment_answer.record(sent.elapsed());
                    }
                    let counter = match result {
                        PayResult::Done => &events_counters.payments_done,
                        PayResult::InsufficientFunds => &events_counters.payments_short,
                        _ => &events_counters.payments_refused,
                    };
                    Counters::add(counter, 1);
                }
                Ok(ServerEvent::Stats(stats)) => {
                    if let Ok(mut slot) = events_counters.server.lock() {
                        *slot = Some(stats);
                    }
                }
                Ok(ServerEvent::Seat {
                    vehicle,
                    seat,
                    answer,
                }) => {
                    let counter = match answer {
                        SeatAnswer::Granted => &events_counters.seats_granted,
                        SeatAnswer::Left => &events_counters.seats_left,
                        _ => &events_counters.seats_refused,
                    };
                    Counters::add(counter, 1);
                    if let Ok(mut seats) = events_shared.seats.lock() {
                        seats.push((vehicle, seat, answer));
                    }
                }
                Ok(ServerEvent::Ownership { entity, epoch, yours }) => {
                    if let Ok(mut owned) = events_shared.owned.lock() {
                        owned.retain(|(id, _)| *id != entity);
                        if yours {
                            owned.push((entity, epoch));
                        }
                    }
                }
                // An honest bot stays within the limits; a correction would show up in the server's counts.
                // Other events are not for bots.
                Ok(_) | Err(_) => {}
            }
        }
    });

    // Payments, seat requests and shots share the one event stream.
    let events = Arc::new(tokio::sync::Mutex::new(event_sender));
    let payer = config.pay_every.map(|every| {
        tokio::spawn(pay(
            events.clone(),
            every,
            index,
            own,
            config.payees.clone(),
            payments,
            counters.clone(),
        ))
    });

    let recorder = config.record.clone().filter(|r| r.records(index));
    let ack = Arc::new(AtomicU32::new(0));
    let reader = tokio::spawn(read_snapshots(
        datagrams.clone(),
        ack.clone(),
        counters.clone(),
        recorder.map(|r| (r, index)),
        index < config.decoders || acts,
        acts.then(|| shared.clone()),
    ));
    let ended_early = send_states(
        &config,
        index,
        (&datagrams, &events),
        &ack,
        &counters,
        &shared,
        stop,
    )
    .await;
    datagrams.close();
    reader.abort();
    if let Some(payer) = payer {
        payer.abort();
    }
    if ended_early {
        Counters::add(&counters.dropped, 1);
    }
}

/// Sends the walker's state every client tick until `stop`, with a driver's vehicle state and seat requests and a
/// shooter's shots. Returns whether the connection ended first.
async fn send_states(
    config: &BotConfig,
    index: u64,
    (datagrams, events): (&Datagrams, &tokio::sync::Mutex<EventSender>),
    ack: &AtomicU32,
    counters: &Counters,
    shared: &Shared,
    stop: Instant,
) -> bool {
    let period = Duration::from_secs(1) / config.send_hz.max(1);
    let mut walker = Walker::new(config.area, index);
    let mut driver = (index < config.drivers).then(|| Driver::new(index));
    let shooter = index < config.shooters;
    let shot_every = Duration::from_secs(1) / config.shot_hz.max(1);
    let mut next_shot = Instant::now() + shot_every;
    let mut seen = Seen::default();
    let mut owned = Vec::new();
    let mut interval = tokio::time::interval(period);
    // A late generator must show as lateness, not catch up in bursts that hide it.
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut sequence = 0u16;
    let mut last = Instant::now();
    loop {
        let scheduled = interval.tick().await;
        let now = Instant::now();
        if now >= stop {
            return false;
        }
        counters
            .send_lateness
            .record(now.saturating_duration_since(scheduled));
        let dt = (now - last).as_secs_f32();
        last = now;
        let mut fresh = false;
        let mut arrived = Duration::ZERO;
        if driver.is_some() || shooter {
            if let Ok(latest) = shared.seen.lock() {
                arrived = latest
                    .0
                    .map_or(Duration::MAX, |at| now.saturating_duration_since(at));
                fresh = arrived <= FRESH;
                seen.clone_from(&latest.1);
            }
            if let Ok(o) = shared.owned.lock() {
                owned.clone_from(&*o);
            }
        }
        let mut vehicle = None;
        if let Some(driver) = driver.as_mut() {
            // The answers to its earlier requests first, so it drives only a seat the server granted.
            if let Ok(mut seats) = shared.seats.lock() {
                for (vehicle, seat, answer) in seats.drain(..) {
                    driver.answer(vehicle, seat, answer, &mut walker);
                }
            }
            let ids: Vec<u32> = owned.iter().map(|(id, _)| *id).collect();
            let event = match driver.step(dt, &mut walker, &ids, &seen) {
                Request::None => None,
                Request::Enter { vehicle, seat } => Some(ClientEvent::EnterSeat { vehicle, seat }),
                Request::Leave => Some(ClientEvent::LeaveSeat),
            };
            if let Some(event) = event {
                if send_event(events, &event).await.is_err() {
                    return true;
                }
                Counters::add(&counters.seat_requests, 1);
            }
            vehicle = driver
                .driving()
                .and_then(|v| owned.iter().find(|(id, _)| *id == v).copied());
        }
        if !driver.as_ref().is_some_and(Driver::holds) {
            walker.step(dt);
        }
        if shooter && now >= next_shot {
            next_shot = now + shot_every;
            if let Some(shot) = fresh
                .then(|| aim(shared, &seen, arrived, config.aim_delay, walker.position()))
                .flatten()
            {
                if send_event(events, &ClientEvent::Shot(Box::new(shot)))
                    .await
                    .is_err()
                {
                    return true;
                }
                Counters::add(&counters.shots_sent, 1);
            }
        }
        let Some(state) = client_state(&walker) else {
            continue;
        };
        let mut ped = ped_block(&walker);
        if shooter {
            ped.weapon = WEAPON;
        }
        // A driver's vehicle goes where its walker goes; the server places the driver in it.
        let owned_states: Vec<OwnedState> = vehicle
            .and_then(|(id, epoch)| OwnedState::new(id, epoch, state, &DRIVEN.to_fields()).ok())
            .into_iter()
            .collect();
        let mut packet = header(PacketKind::ClientState, sequence, ack.load(Ordering::Relaxed));
        sequence = sequence.wrapping_add(1);
        if state.encode_with_owned(&ped, &owned_states, &mut packet).is_err() {
            continue;
        }
        Counters::add(&counters.owned_states_sent, owned_states.len() as u64);
        let len = packet.len() as u64;
        if datagrams.send(Bytes::from(packet)).is_err() {
            return true;
        }
        Counters::add(&counters.sent_datagrams, 1);
        Counters::add(&counters.sent_bytes, len);
    }
}

/// Pays a random other bot 1 to 100 units every `every` on average, the pause drawn between half and one and a
/// half of it, until the connection ends. Answers arrive on the events task, which matches them by key.
async fn pay(
    sender: Arc<tokio::sync::Mutex<EventSender>>,
    every: Duration,
    index: u64,
    own: Arc<AtomicU32>,
    payees: Arc<Mutex<Vec<u32>>>,
    payments: Arc<Mutex<HashMap<u64, Instant>>>,
    counters: Arc<Counters>,
) {
    let mut rng = Rng::new(index.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x0070_6179);
    // A random first pause, so bots that joined together do not pay together.
    tokio::time::sleep(every.mul_f32(rng.range(0.0, 1.0))).await;
    let mut key = 0u64;
    loop {
        let me = own.load(Ordering::Relaxed);
        let to = payees.lock().ok().and_then(|p| {
            if p.len() < 2 {
                return None;
            }
            let pick = p[(rng.unit() * p.len() as f32) as usize % p.len()];
            Some(pick).filter(|&to| to != me)
        });
        if let (Some(to), true) = (to, me != u32::MAX) {
            key += 1;
            let amount = 1 + (rng.unit() * 100.0) as u32;
            let mut packet = header(PacketKind::Event, 0, 0);
            ClientEvent::Pay { key, to, amount }.encode(&mut packet);
            if let Ok(mut p) = payments.lock() {
                p.insert(key, Instant::now());
            }
            if sender.lock().await.send(&packet).await.is_err() {
                return;
            }
            Counters::add(&counters.payments_sent, 1);
        }
        tokio::time::sleep(every.mul_f32(rng.range(0.5, 1.5))).await;
    }
}

/// A driven vehicle's block: intact with its engine running.
const DRIVEN: VehicleBlock = VehicleBlock {
    flags: VehicleBlock::ENGINE,
    ..VehicleBlock::INTACT
};

async fn send_event(events: &tokio::sync::Mutex<EventSender>, event: &ClientEvent) -> Result<(), ()> {
    let mut packet = header(PacketKind::Event, 0, 0);
    event.encode(&mut packet);
    events.lock().await.send(&packet).await.map_err(|_| ())
}

/// Decoded snapshots a bot keeps as baselines, as many as the server keeps history for.
const KEPT_TICKS: usize = 16;

async fn read_snapshots(
    datagrams: Datagrams,
    ack: Arc<AtomicU32>,
    counters: Arc<Counters>,
    record: Option<(Arc<Recorder>, u64)>,
    decode: bool,
    shared: Option<Arc<Shared>>,
) {
    let mut reassembler = Reassembler::new();
    let mut kept: Vec<(Option<u32>, Vec<EntityState>, Vec<Info>)> =
        vec![(None, Vec::new(), Vec::new()); KEPT_TICKS];
    let mut decoded = Vec::new();
    let mut infos = Vec::new();
    while let Ok((header, packet)) = datagrams.recv().await {
        if header.kind != PacketKind::DeltaSnapshot {
            continue;
        }
        Counters::add(&counters.received_datagrams, 1);
        Counters::add(&counters.received_bytes, packet.len() as u64);
        if let Some((recorder, index)) = &record {
            recorder.write(*index, &packet);
        }
        let Ok(Some(payload)) = reassembler.push(header.tick, &packet[PacketHeader::SIZE..]) else {
            continue;
        };
        let tick = header.tick;
        let base = match baseline_age(payload) {
            Ok(0) => None,
            Ok(age) => match tick.checked_sub(age) {
                Some(t) if kept[t as usize % KEPT_TICKS].0 == Some(t) => Some(Baseline {
                    tick: t,
                    entities: &kept[t as usize % KEPT_TICKS].1,
                }),
                // A baseline this bot no longer has: wait for one against a tick it acknowledged since.
                _ => continue,
            },
            Err(_) => continue,
        };
        Counters::add(&counters.ticks_seen, 1);
        if !decode {
            // Only the tick is kept: a later snapshot against it is checked against the same rule as above.
            kept[tick as usize % KEPT_TICKS].0 = Some(tick);
            ack.fetch_max(tick, Ordering::Relaxed);
            continue;
        }
        let result = match &shared {
            // A driver or shooter needs each entity's kind, so it keeps the infos too.
            Some(_) => {
                let base_infos = base
                    .as_ref()
                    .map_or(&[][..], |b| &kept[b.tick as usize % KEPT_TICKS].2[..]);
                decode_delta_with_info(payload, tick, base, base_infos, &mut decoded, &mut infos)
            }
            None => decode_delta(payload, tick, base, &mut decoded),
        };
        if result.is_err() {
            continue;
        }
        if let Some(shared) = &shared
            && let Ok(mut seen) = shared.seen.lock()
        {
            if shared.keep > 0
                && seen.0.is_some()
                && let Ok(mut past) = shared.past.lock()
            {
                // The snapshot being replaced joins the ones a delayed shooter draws from.
                if past.len() == shared.keep {
                    past.pop_front();
                }
                past.push_back(seen.1.clone());
            }
            seen.0 = Some(Instant::now());
            seen.1.tick = tick;
            seen.1.entities.clone_from(&decoded);
            seen.1.infos.clone_from(&infos);
        }
        Counters::add(&counters.received_entities, decoded.len() as u64);
        Counters::add(&counters.decoded_ticks, 1);
        let slot = &mut kept[tick as usize % KEPT_TICKS];
        slot.0 = Some(tick);
        std::mem::swap(&mut slot.1, &mut decoded);
        std::mem::swap(&mut slot.2, &mut infos);
        ack.fetch_max(tick, Ordering::Relaxed);
    }
}

/// A shooter's shot at the vehicles in `newest`, which arrived `arrived` ago. With an aim delay it aims at its
/// render time, that many ticks behind, moving on with the time since the newest snapshot; `None` when the
/// snapshots around the render time are not kept (one was lost) or there is nothing to hit.
fn aim(shared: &Shared, newest: &Seen, arrived: Duration, delay: u32, from: [f32; 3]) -> Option<Shot> {
    if delay == 0 {
        return shot_at_vehicles(newest, None, 0, from);
    }
    let hz = match shared.tick_hz.load(Ordering::Relaxed) {
        0 => 20.0,
        hz => f32::from(hz),
    };
    let (tick, frac) = render_time(newest.tick, arrived.as_secs_f32() * hz, delay);
    let past = shared.past.lock().ok()?;
    let at = |t: u32| {
        (t == newest.tick)
            .then_some(newest)
            .or_else(|| past.iter().find(|s| s.tick == t))
    };
    let then = at(tick)?;
    let next = if frac > 0 { Some(at(tick + 1)?) } else { None };
    shot_at_vehicles(then, next, frac, from)
}

fn client_state(walker: &Walker) -> Option<ClientState> {
    Some(ClientState {
        position: Position::from_metres(walker.position()).ok()?,
        velocity: Velocity::from_metres_per_second(walker.velocity()).ok()?,
        rotation: Rotation::from_quaternion(walker.rotation()).ok()?,
    })
}

/// The bot's ped block: standing while it pauses, otherwise moving by its speed, so the server relays block
/// changes as a crowd of real players would make them.
fn ped_block(walker: &Walker) -> PedBlock {
    let [vx, vy, _] = walker.velocity();
    let speed = vx.hypot(vy);
    let motion = if speed < 0.1 {
        Motion::Idle
    } else if speed < 2.5 {
        Motion::Walk
    } else if speed < 10.0 {
        Motion::Run
    } else {
        // The walker's driving legs; a bot drives no vehicle until BOT-5.
        Motion::Sprint
    };
    PedBlock {
        motion,
        ..PedBlock::default()
    }
}

pub(crate) fn header(kind: PacketKind, sequence: u16, tick: u32) -> Vec<u8> {
    let mut out = vec![0; PacketHeader::SIZE];
    let header = PacketHeader { kind, sequence, tick };
    // The buffer is exactly the header size, so encoding cannot fail.
    let _ = header.encode(&mut out);
    out
}
