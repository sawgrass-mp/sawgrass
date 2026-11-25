//! Clients connect through the in-process gateway: the server places each one in its world where the client
//! says it is, and sends each a snapshot with the other. A flood of states costs one slot, not a queue, and
//! connections beyond the player cap are refused. Each connection is held to its rates: states beyond them are
//! dropped, a sustained flood or an undecodable event closes the connection, and one address gets only so many
//! connections. A client that teleports stays where it was, and is told so, unless the rules only report.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use sawgrass_gateway::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sawgrass_gateway::{Gateway, RootCertStore, Session, connect};
use sawgrass_protocol::delta::{Reassembler, decode_delta};
use sawgrass_protocol::game::{ClientState, ServerEvent};
use sawgrass_protocol::{
    EntityState, PROTOCOL_VERSION, PacketHeader, PacketKind, Position, Rotation, Velocity,
};
use sawgrass_server::limits::Limits;
use sawgrass_server::movement::MovementRules;
use sawgrass_server::net::{self, ListenConfig, NetId, PlayerState, Roster};
use sawgrass_server::{TickConfig, TickLoop};

fn gateway() -> (Gateway, RootCertStore) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let mut roots = RootCertStore::empty();
    roots.add(der.clone()).expect("root");
    let gateway = Gateway::bind(([127, 0, 0, 1], 0).into(), vec![der], key).expect("bind");
    (gateway, roots)
}

fn state_packet(x: f32) -> Bytes {
    let state = ClientState {
        position: Position::from_metres([x, 5.0, 30.0]).expect("position"),
        velocity: Velocity::from_metres_per_second([1.0, 0.0, 0.0]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    };
    let mut packet = vec![0; PacketHeader::SIZE + ClientState::SIZE];
    let header = PacketHeader {
        kind: PacketKind::ClientState,
        sequence: 0,
        tick: 0,
    };
    header.encode(&mut packet).expect("header");
    state.encode(&mut packet[PacketHeader::SIZE..]).expect("state");
    Bytes::from(packet)
}

async fn joined(session: &mut Session) -> u32 {
    let (_, packet) = session.recv_event().await.expect("event");
    match ServerEvent::decode(&packet[PacketHeader::SIZE..]) {
        Ok(ServerEvent::Joined { entity, tick_hz }) => {
            assert_eq!(tick_hz, 20);
            entity
        }
        other => panic!("expected Joined, got {other:?}"),
    }
}

/// The next `n` seat answers `session` is sent (NET-21), as (vehicle, seat, answer), skipping other events.
fn seat_answers(
    runtime: &tokio::runtime::Runtime,
    session: &mut Session,
    n: usize,
) -> Vec<(u32, u8, sawgrass_protocol::game::SeatAnswer)> {
    let mut answers = Vec::new();
    while answers.len() < n {
        let (_, packet) = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), session.recv_event()).await })
            .expect("a seat answer in time")
            .expect("event");
        if let Ok(ServerEvent::Seat {
            vehicle,
            seat,
            answer,
        }) = ServerEvent::decode(&packet[PacketHeader::SIZE..])
        {
            answers.push((vehicle, seat, answer));
        }
    }
    answers
}

/// Runs a tick every 10 ms on the test's thread until `done` holds or two seconds pass.
fn tick_until(tick_loop: &mut TickLoop, mut done: impl FnMut(&TickLoop) -> bool) -> bool {
    for _ in 0..200 {
        tick_loop.run_tick();
        if done(tick_loop) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn position_of(tick_loop: &TickLoop, id: u32) -> Option<[f32; 3]> {
    let world = tick_loop.world();
    let mut query = world.query::<(&NetId, &PlayerState)>();
    query
        .iter()
        .find(|(net, _)| net.0 == id)
        .map(|(_, state)| state.0.position.to_metres())
}

#[test]
fn players_are_placed_and_see_each_other() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, mut players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let mut tick_loop = TickLoop::new(TickConfig::default());

    let (mut a, mut b) = runtime.block_on(async {
        let a = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
        let b = connect(addr, "localhost", roots, PROTOCOL_VERSION).await;
        (a.expect("a connects"), b.expect("b connects"))
    });
    // Joins reach the tick thread through the channel; ticks send the Joined events.
    let roster = players.roster();
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
    let (id_a, id_b) = std::thread::scope(|scope| {
        let ids = scope.spawn(|| runtime.block_on(async { (joined(&mut a).await, joined(&mut b).await) }));
        tick_until(&mut tick_loop, |_| ids.is_finished());
        ids.join().expect("joined")
    });
    assert_ne!(id_a, id_b);

    // Each client says where it is; the server takes it as is.
    a.send_datagram(state_packet(10.0)).expect("send a");
    b.send_datagram(state_packet(20.0)).expect("send b");
    assert!(tick_until(&mut tick_loop, |t| {
        position_of(t, id_a) == Some([10.0, 5.0, 30.0]) && position_of(t, id_b) == Some([20.0, 5.0, 30.0])
    }));

    // A moves; the server follows.
    a.send_datagram(state_packet(12.5)).expect("send a");
    assert!(tick_until(&mut tick_loop, |t| position_of(t, id_a)
        == Some([12.5, 5.0, 30.0])));

    // B's snapshots carry A, 7.5 m away, and never B itself. Older snapshots may still be queued, so read until
    // A's new position shows.
    let seen: Vec<EntityState> = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            runtime.block_on(async {
                let mut reassembler = Reassembler::new();
                loop {
                    let (header, packet) = b.recv_datagram().await.expect("snapshot");
                    assert_eq!(header.kind, PacketKind::DeltaSnapshot);
                    // B never acknowledges (its states carry tick 0), so every snapshot is whole.
                    let Some(payload) = reassembler
                        .push(header.tick, &packet[PacketHeader::SIZE..])
                        .expect("fragment")
                    else {
                        continue;
                    };
                    let mut states = Vec::new();
                    decode_delta(payload, header.tick, None, &mut states).expect("decode");
                    assert!(states.iter().all(|s| s.id != id_b), "B sees itself");
                    if states.iter().any(|s| s.position.to_metres()[0] == 12.5) {
                        return states;
                    }
                }
            })
        });
        tick_until(&mut tick_loop, |_| reader.is_finished());
        reader.join().expect("reader")
    });
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, id_a);
    assert_eq!(seen[0].position.to_metres(), [12.5, 5.0, 30.0]);

    // A leaves; its entity goes.
    a.close();
    assert!(tick_until(&mut tick_loop, |t| position_of(t, id_a).is_none()));
    assert!(position_of(&tick_loop, id_b).is_some());

    // Later systems (scripts) heard both joins, then A leaving.
    let changes: Vec<Roster> = roster.try_iter().collect();
    assert_eq!(changes.len(), 3, "{changes:?}");
    assert!(changes[..2].contains(&Roster::Joined(id_a)) && changes[..2].contains(&Roster::Joined(id_b)));
    assert_eq!(changes[2], Roster::Left(id_a));
}

/// Snapshots leave through the gateway's send task once per tick (NET-9): each tick's datagram reaches the client,
/// in the order the ticks sent them.
#[test]
fn snapshots_arrive_in_tick_order() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, mut players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
    let mut client = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("connect");
    let id = std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut client)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    client.send_datagram(state_packet(1.0)).expect("send");
    assert!(tick_until(&mut tick_loop, |t| position_of(t, id).is_some()));

    let headers: Vec<PacketHeader> = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            runtime.block_on(async {
                let mut headers = Vec::new();
                while headers.len() < 20 {
                    let (header, _) = client.recv_datagram().await.expect("snapshot");
                    assert_eq!(header.kind, PacketKind::DeltaSnapshot);
                    headers.push(header);
                }
                headers
            })
        });
        tick_until(&mut tick_loop, |_| reader.is_finished());
        reader.join().expect("reader")
    });
    for pair in headers.windows(2) {
        assert!(
            pair[1].sequence > pair[0].sequence,
            "sequence went back: {pair:?}"
        );
        assert!(
            pair[1].tick > pair[0].tick,
            "one datagram per tick when alone: {pair:?}"
        );
    }
}

#[test]
fn a_flood_of_states_keeps_only_the_latest() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, mut players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
    let mut client = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("connect");
    let id = std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut client)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });

    // 10,000 states with no tick in between, the last one at x = 99.99 m. Loopback may drop some of them when
    // the burst outruns quinn's buffers, so the client repeats the last until the server shows it.
    for i in 0..10_000u32 {
        let _ = client.send_datagram(state_packet(i as f32 / 100.0));
    }
    let last = state_packet(99.99);
    assert!(tick_until(&mut tick_loop, |t| {
        let _ = client.send_datagram(last.clone());
        position_of(t, id) == Some([99.99, 5.0, 30.0])
    }));
    // One entity, and nothing left queued: the next ticks change nothing.
    assert_eq!(tick_loop.world().query::<&NetId>().iter().count(), 1);
    for _ in 0..5 {
        tick_loop.run_tick();
    }
    assert_eq!(position_of(&tick_loop, id), Some([99.99, 5.0, 30.0]));
}

#[test]
fn connections_beyond_the_cap_are_refused() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, _players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        let config = ListenConfig {
            max_players: 1,
            ..ListenConfig::default()
        };
        (addr, roots, net::listen(gateway, config))
    };
    runtime.block_on(async {
        let first = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
        assert!(first.is_ok(), "{first:?}");
        let second = connect(addr, "localhost", roots, PROTOCOL_VERSION).await;
        assert!(second.is_err(), "second player admitted");
    });
}

/// Starts a server held to `limits` and connects one client to it, returning the client, the server's counters
/// and the server, which must be kept alive.
fn limited(
    runtime: &tokio::runtime::Runtime,
    limits: Limits,
) -> (Session, std::sync::Arc<net::Traffic>, net::Players) {
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        let config = ListenConfig {
            limits,
            ..ListenConfig::default()
        };
        (addr, roots, net::listen(gateway, config))
    };
    let client = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("connect");
    let traffic = players.traffic();
    (client, traffic, players)
}

/// Waits up to two seconds for `counter` to reach `at_least`, calling `meanwhile` every millisecond.
fn wait_for(counter: &AtomicU64, at_least: u64, mut meanwhile: impl FnMut()) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if counter.load(Ordering::Relaxed) >= at_least {
            return true;
        }
        meanwhile();
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

#[test]
fn states_beyond_the_rate_are_dropped_and_counted() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (client, traffic, _players) = limited(&runtime, Limits::default());
    // 200 at once against a burst of 30: loopback may lose a few, but well over a hundred are dropped.
    for i in 0..200u32 {
        let _ = client.send_datagram(state_packet(i as f32));
    }
    assert!(
        wait_for(&traffic.dropped_states, 100, || ()),
        "dropped {:?}",
        traffic.dropped_states
    );
    // Dropping is all: one burst does not close the connection.
    assert_eq!(traffic.closed_floods.load(Ordering::Relaxed), 0);
    assert!(client.send_datagram(state_packet(1.0)).is_ok());
}

#[test]
fn a_sustained_flood_of_states_closes_the_connection() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let limits = Limits {
        window: Duration::from_millis(50),
        ..Limits::default()
    };
    let (client, traffic, _players) = limited(&runtime, limits);
    // Ten states a millisecond is far above twice 60 a second in every window.
    let closed = wait_for(&traffic.closed_floods, 1, || {
        for _ in 0..10 {
            let _ = client.send_datagram(state_packet(1.0));
        }
    });
    assert!(closed, "not closed");
    let gone = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(2), client.recv_datagram()).await });
    assert!(matches!(gone, Ok(Err(_))), "{gone:?}");
}

#[test]
fn an_undecodable_event_closes_the_connection() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (mut client, traffic, _players) = limited(&runtime, Limits::default());
    let mut packet = vec![0; PacketHeader::SIZE];
    let header = PacketHeader {
        kind: PacketKind::Event,
        sequence: 0,
        tick: 0,
    };
    header.encode(&mut packet).expect("header");
    packet.extend_from_slice(&[0xEE, 1, 2, 3]);
    runtime.block_on(client.send_event(&packet)).expect("send");
    assert!(wait_for(&traffic.closed_bad_events, 1, || ()), "not closed");
    let gone = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(2), client.recv_datagram()).await });
    assert!(matches!(gone, Ok(Err(_))), "{gone:?}");
}

#[test]
fn connections_beyond_the_per_address_cap_are_refused() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        let config = ListenConfig {
            limits: Limits {
                max_per_address: 2,
                ..Limits::default()
            },
            ..ListenConfig::default()
        };
        (addr, roots, net::listen(gateway, config))
    };
    let traffic = players.traffic();
    runtime.block_on(async {
        let first = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
        let second = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
        assert!(first.is_ok() && second.is_ok(), "{first:?} {second:?}");
        let third = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
        assert!(third.is_err(), "third connection from one address admitted");
        assert_eq!(traffic.refused_per_address.load(Ordering::Relaxed), 1);
        // A connection that ends gives its place back.
        let first = first.expect("first");
        first.close();
        drop(first);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let again = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
            if again.is_ok() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "place not given back");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    drop(players);
}

/// Forwards every datagram it receives to `server` and nothing back, like a sender with a forged source address:
/// it never hears the server's answer. Returns its address and a flag that stops it.
fn one_way_relay(
    server: std::net::SocketAddr,
) -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("relay");
    socket
        .set_read_timeout(Some(Duration::from_millis(20)))
        .expect("timeout");
    let addr = socket.local_addr().expect("addr");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while !stopped.load(Ordering::Relaxed) {
            if let Ok((len, from)) = socket.recv_from(&mut buf)
                && from != server
            {
                let _ = socket.send_to(&buf[..len], server);
            }
        }
    });
    (addr, stop)
}

#[test]
fn spoofed_attempts_take_no_place_and_a_real_client_still_connects() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    // One handshake and one connection per address: a single attempt that kept its place would shut out the client.
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        let config = ListenConfig {
            limits: Limits {
                max_handshakes: 1,
                max_per_address: 1,
                ..Limits::default()
            },
            ..ListenConfig::default()
        };
        (addr, roots, net::listen(gateway, config))
    };
    let traffic = players.traffic();
    let (relay, stop) = one_way_relay(addr);
    runtime.block_on(async {
        // Ten attempts from 127.0.0.1 that never hear back: each Initial gets a Retry that goes nowhere.
        let spoofed: Vec<_> = (0..10)
            .map(|_| tokio::spawn(connect(relay, "localhost", roots.clone(), PROTOCOL_VERSION)))
            .collect();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while traffic.retried_handshakes.load(Ordering::Relaxed) < 10 {
            assert!(tokio::time::Instant::now() < deadline, "attempts not retried");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // A real client from the same address proves it and gets both places.
        let real = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
        assert!(real.is_ok(), "{real:?}");
        assert_eq!(traffic.refused_handshakes.load(Ordering::Relaxed), 0);
        assert_eq!(traffic.refused_per_address.load(Ordering::Relaxed), 0);
        // A second validated client from that address is beyond the per-address cap.
        let second = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
        assert!(second.is_err(), "second connection from one address admitted");
        for attempt in spoofed {
            attempt.abort();
        }
    });
    stop.store(true, Ordering::Relaxed);
}

#[test]
fn a_teleport_is_refused_and_counted() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, mut players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let traffic = players.traffic();
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
    let mut client = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("connect");
    let id = std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut client)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    let spawn = state_packet(10.0);
    assert!(tick_until(&mut tick_loop, |t| {
        let _ = client.send_datagram(spawn.clone());
        position_of(t, id) == Some([10.0, 5.0, 30.0])
    }));
    // 5 km away in one step: refused every time, and the player stays put.
    let jump = state_packet(5000.0);
    let refused = |t: &TickLoop| {
        let _ = client.send_datagram(jump.clone());
        assert_eq!(position_of(t, id), Some([10.0, 5.0, 30.0]));
        traffic
            .refused_teleport
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 5
    };
    assert!(tick_until(&mut tick_loop, refused));
    // The client is told where the server holds it, once for a burst of refusals well under the correction
    // interval. The world state it was sent on joining (SRV-17) comes first.
    let packet = runtime.block_on(async {
        loop {
            let (_, packet) = tokio::time::timeout(Duration::from_secs(2), client.recv_event())
                .await
                .expect("a correction in time")
                .expect("event");
            match ServerEvent::decode(&packet[PacketHeader::SIZE..]) {
                Ok(ServerEvent::Environment(_) | ServerEvent::Door(_)) => {}
                _ => break packet,
            }
        }
    });
    match ServerEvent::decode(&packet[PacketHeader::SIZE..]) {
        Ok(ServerEvent::Corrected(state)) => {
            assert_eq!(state.id, id);
            assert_eq!(state.position.to_metres(), [10.0, 5.0, 30.0]);
        }
        other => panic!("expected Corrected, got {other:?}"),
    }
    assert_eq!(traffic.corrections.load(Ordering::Relaxed), 1);
    // Refusals keep coming for 600 ms: one more correction per 250 ms at most.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(600) {
        let _ = client.send_datagram(jump.clone());
        tick_loop.run_tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    let sent = traffic.corrections.load(Ordering::Relaxed);
    assert!((2..=4).contains(&sent), "{sent} corrections");
    // A step it could have walked is taken.
    let step = state_packet(12.0);
    assert!(tick_until(&mut tick_loop, |t| {
        let _ = client.send_datagram(step.clone());
        position_of(t, id) == Some([12.0, 5.0, 30.0])
    }));
}

/// The player list an operator reads: each connected player's id, address, position and refused states, filled
/// only on the tick after it is asked for.
#[test]
fn the_player_list_shows_who_is_where() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, mut players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let list = players.player_list();
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
    let mut client = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("connect");
    let id = std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut client)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    // Not asked for: nothing is filled in.
    tick_loop.run_tick();
    assert!(list.take().is_empty());
    let spawn = state_packet(10.0);
    let jump = state_packet(5000.0);
    assert!(tick_until(&mut tick_loop, |t| {
        let _ = client.send_datagram(spawn.clone());
        position_of(t, id) == Some([10.0, 5.0, 30.0])
    }));
    let _ = client.send_datagram(jump);
    std::thread::sleep(Duration::from_millis(50));
    tick_loop.run_tick();
    list.request();
    tick_loop.run_tick();
    let views = list.take();
    assert_eq!(views.len(), 1);
    let view = views[0];
    assert_eq!(view.id, id);
    assert!(view.address.ip().is_loopback());
    assert_eq!(view.position, Some([10.0, 5.0, 30.0]));
    assert_eq!(view.refused.teleport, 1);
    // Taken once: the next take is empty until asked again.
    assert!(list.take().is_empty());
}

/// With `report_only`, a teleport is counted like a refusal but applied, and nobody is corrected: a first session
/// with the game learns which limit is wrong without fighting it.
#[test]
fn report_only_counts_a_teleport_and_applies_it() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let config = ListenConfig {
        movement: MovementRules {
            report_only: true,
            ..MovementRules::default()
        },
        ..ListenConfig::default()
    };
    let (addr, roots, mut players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, config))
    };
    let traffic = players.traffic();
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
    let mut client = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("connect");
    let id = std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut client)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    let spawn = state_packet(10.0);
    assert!(tick_until(&mut tick_loop, |t| {
        let _ = client.send_datagram(spawn.clone());
        position_of(t, id) == Some([10.0, 5.0, 30.0])
    }));
    let jump = state_packet(5000.0);
    assert!(tick_until(&mut tick_loop, |t| {
        let _ = client.send_datagram(jump.clone());
        position_of(t, id) == Some([5000.0, 5.0, 30.0])
    }));
    assert!(traffic.refused_teleport.load(Ordering::Relaxed) >= 1);
    assert_eq!(traffic.corrections.load(Ordering::Relaxed), 0);
    // A walk from the new place is a normal move.
    let refused = traffic.refused_teleport.load(Ordering::Relaxed);
    let step = state_packet(5002.0);
    assert!(tick_until(&mut tick_loop, |t| {
        let _ = client.send_datagram(step.clone());
        position_of(t, id) == Some([5002.0, 5.0, 30.0])
    }));
    assert_eq!(traffic.refused_teleport.load(Ordering::Relaxed), refused);
}

/// Entities the server spawns (ADR 0006): a vehicle reaches a nearby client's snapshot with its kind, model and
/// block, and leaves it when despawned. Players and positions off the map are not spawned.
#[test]
fn a_spawned_vehicle_is_seen_with_its_info_until_despawned() {
    use sawgrass_protocol::delta::decode_delta_with_info;
    use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
    use sawgrass_protocol::vehicle::VehicleBlock;
    use std::sync::{Arc, Mutex};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let players = Arc::new(Mutex::new(players));
    let parked = |x: f32| ClientState {
        position: Position::from_metres([x, 5.0, 30.0]).expect("position"),
        velocity: Velocity::from_metres_per_second([0.0; 3]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    };
    let block = VehicleBlock {
        flags: VehicleBlock::ENGINE,
        ..VehicleBlock::INTACT
    };
    let info = Info::with_fields(EntityKind::Vehicle, AssetId(77), &block.to_fields()).expect("info");
    let car = {
        let mut p = players.lock().expect("players");
        assert_eq!(p.spawn(Info::PLAYER, parked(15.0)), None);
        assert_eq!(p.spawn(info, parked(9000.0)), None);
        p.spawn(info, parked(15.0)).expect("spawned")
    };
    assert_eq!(
        players.lock().expect("players").spawned(car).map(|(i, _)| i),
        Some(info)
    );

    let mut tick_loop = TickLoop::new(TickConfig::default());
    let system = players.clone();
    tick_loop.add_system("net", move |world, info| {
        system.lock().expect("players").run_tick(world, info);
    });
    let mut a = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("a connects");
    std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut a)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    a.send_datagram(state_packet(10.0)).expect("send a");

    // A never acknowledges, so each snapshot is whole: read until one does (or does not) hold the car.
    let seen = |want: bool, tick_loop: &mut TickLoop| -> Vec<Info> {
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                runtime.block_on(async {
                    let mut reassembler = Reassembler::new();
                    loop {
                        let (header, packet) = a.recv_datagram().await.expect("snapshot");
                        let Some(payload) = reassembler
                            .push(header.tick, &packet[PacketHeader::SIZE..])
                            .expect("fragment")
                        else {
                            continue;
                        };
                        let (mut states, mut infos) = (Vec::new(), Vec::new());
                        decode_delta_with_info(payload, header.tick, None, &[], &mut states, &mut infos)
                            .expect("decode");
                        if states.iter().any(|s| s.id == car) == want {
                            return infos;
                        }
                    }
                })
            });
            tick_until(tick_loop, |_| reader.is_finished());
            reader.join().expect("reader")
        })
    };
    assert_eq!(seen(true, &mut tick_loop), vec![info]);

    assert!(players.lock().expect("players").despawn(car));
    assert!(!players.lock().expect("players").despawn(car));
    assert!(players.lock().expect("players").spawned(car).is_none());
    assert!(seen(false, &mut tick_loop).is_empty());
}

/// ADR 0006, section 4: the server gives a spawned vehicle to the nearest player and tells it, with an epoch, and
/// tells it again when the vehicle is gone.
#[test]
fn the_nearest_player_is_told_it_owns_a_vehicle() {
    use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
    use sawgrass_protocol::vehicle::VehicleBlock;
    use std::sync::{Arc, Mutex};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let players = Arc::new(Mutex::new(players));
    let info =
        Info::with_fields(EntityKind::Vehicle, AssetId(3), &VehicleBlock::INTACT.to_fields()).expect("info");
    let parked = ClientState {
        position: Position::from_metres([30.0, 5.0, 30.0]).expect("position"),
        velocity: Velocity::from_metres_per_second([0.0; 3]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    };
    let car = players
        .lock()
        .expect("players")
        .spawn(info, parked)
        .expect("spawned");
    assert_eq!(players.lock().expect("players").owner(car), Some((None, 0)));

    let mut tick_loop = TickLoop::new(TickConfig::default());
    let system = players.clone();
    tick_loop.add_system("net", move |world, info| {
        system.lock().expect("players").run_tick(world, info);
    });
    let mut a = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("a connects");
    let me = std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut a)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    a.send_datagram(state_packet(10.0)).expect("send a");

    let next_ownership = |a: &mut Session, tick_loop: &mut TickLoop| {
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                runtime.block_on(async {
                    loop {
                        let (_, packet) = a.recv_event().await.expect("event");
                        if let Ok(event @ ServerEvent::Ownership { .. }) =
                            ServerEvent::decode(&packet[PacketHeader::SIZE..])
                        {
                            return event;
                        }
                    }
                })
            });
            tick_until(tick_loop, |_| reader.is_finished());
            reader.join().expect("reader")
        })
    };
    assert_eq!(
        next_ownership(&mut a, &mut tick_loop),
        ServerEvent::Ownership {
            entity: car,
            epoch: 1,
            yours: true
        }
    );
    assert_eq!(players.lock().expect("players").owner(car), Some((Some(me), 1)));

    assert!(players.lock().expect("players").despawn(car));
    assert_eq!(
        next_ownership(&mut a, &mut tick_loop),
        ServerEvent::Ownership {
            entity: car,
            epoch: 2,
            yours: false
        }
    );
}

/// ADR 0006, sections 4 and 5: the server takes a vehicle's state only from its owner with the current epoch,
/// near the owner, and the owner's damage values only for a vehicle nobody owns as property.
#[test]
fn only_the_owner_moves_a_vehicle_and_damages_it_by_the_rule() {
    use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
    use sawgrass_protocol::game::OwnedState;
    use sawgrass_protocol::ped::PedBlock;
    use sawgrass_protocol::vehicle::VehicleBlock;
    use std::sync::{Arc, Mutex};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let traffic = players.traffic();
    let players = Arc::new(Mutex::new(players));
    let at = |x: f32| ClientState {
        position: Position::from_metres([x, 5.0, 30.0]).expect("position"),
        velocity: Velocity::from_metres_per_second([0.0; 3]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    };
    let info =
        Info::with_fields(EntityKind::Vehicle, AssetId(3), &VehicleBlock::INTACT.to_fields()).expect("info");
    let car = players
        .lock()
        .expect("players")
        .spawn(info, at(30.0))
        .expect("spawned");

    let mut tick_loop = TickLoop::new(TickConfig::default());
    let system = players.clone();
    tick_loop.add_system("net", move |world, info| {
        system.lock().expect("players").run_tick(world, info);
    });
    let mut a = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("a connects");
    std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut a)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    let send = |x: f32, epoch: u8, car_state: ClientState, block: VehicleBlock| {
        let owned = OwnedState::new(car, epoch, car_state, &block.to_fields()).expect("owned");
        let mut packet = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::ClientState,
            sequence: 0,
            tick: 0,
        }
        .encode(&mut packet)
        .expect("header");
        at(x)
            .encode_with_owned(&PedBlock::default(), &[owned], &mut packet)
            .expect("state");
        a.send_datagram(Bytes::from(packet)).expect("send");
    };
    let car_at = |players: &Mutex<net::Players>| {
        players.lock().expect("players").spawned(car).map(|(i, s)| {
            let block = VehicleBlock::from_fields(i.fields()).expect("block");
            (s.position.to_metres()[0], block.engine)
        })
    };
    // Placed at 10 m, A becomes the owner at epoch 1.
    send(10.0, 0, at(30.0), VehicleBlock::INTACT);
    assert!(tick_until(&mut tick_loop, |_| {
        players
            .lock()
            .expect("players")
            .owner(car)
            .is_some_and(|(o, e)| o.is_some() && e == 1)
    }));
    // A stale epoch is dropped; the right one moves the car and lowers its engine within the budget.
    send(10.0, 0, at(31.0), VehicleBlock::INTACT);
    assert!(wait_for(&traffic.refused_owned_stale, 1, || {
        tick_loop.run_tick();
    }));
    let wrecked = VehicleBlock {
        engine: 0,
        ..VehicleBlock::INTACT
    };
    send(10.0, 1, at(32.0), wrecked);
    assert!(tick_until(&mut tick_loop, |_| car_at(&players)
        .is_some_and(|(x, _)| x == 32.0)));
    let (_, engine) = car_at(&players).expect("car");
    assert!((155..255).contains(&engine), "engine {engine}");
    // Beyond the keep radius of its owner, refused.
    send(10.0, 1, at(200.0), wrecked);
    assert!(wait_for(&traffic.refused_owned_far, 1, || {
        tick_loop.run_tick();
    }));
    // Within the keep radius but too far from the last accepted state, or faster than any vehicle: refused.
    send(10.0, 1, at(-135.0), wrecked);
    assert!(wait_for(&traffic.refused_owned_movement, 1, || {
        tick_loop.run_tick();
    }));
    let speeding = ClientState {
        velocity: Velocity::from_metres_per_second([200.0, 0.0, 0.0]).expect("velocity"),
        ..at(32.5)
    };
    send(10.0, 1, speeding, wrecked);
    assert!(wait_for(&traffic.refused_owned_movement, 2, || {
        tick_loop.run_tick();
    }));
    // A player's vehicle keeps the server's damage, and still moves.
    assert!(players.lock().expect("players").set_property(car, true));
    send(10.0, 1, at(33.0), wrecked);
    assert!(tick_until(&mut tick_loop, |_| car_at(&players)
        .is_some_and(|(x, _)| x == 33.0)));
    assert_eq!(car_at(&players).map(|(_, e)| e), Some(engine));
}

/// With `report_only`, a spawn and an owner's state that break the movement rules are counted and applied, so a
/// wrong limit in a first session does not freeze the cars clients drive. Ownership is still checked.
#[test]
fn report_only_applies_owned_and_spawned_states_and_counts_them() {
    use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
    use sawgrass_protocol::game::OwnedState;
    use sawgrass_protocol::ped::PedBlock;
    use sawgrass_protocol::vehicle::VehicleBlock;
    use std::sync::{Arc, Mutex};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let config = ListenConfig {
        movement: MovementRules {
            report_only: true,
            ..MovementRules::default()
        },
        ..ListenConfig::default()
    };
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, config))
    };
    let traffic = players.traffic();
    let players = Arc::new(Mutex::new(players));
    let at = |x: f32, speed: f32| ClientState {
        position: Position::from_metres([x, 5.0, 30.0]).expect("position"),
        velocity: Velocity::from_metres_per_second([speed, 0.0, 0.0]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    };
    let info =
        Info::with_fields(EntityKind::Vehicle, AssetId(3), &VehicleBlock::INTACT.to_fields()).expect("info");
    // Faster than the limit: spawned anyway, and counted.
    let car = players
        .lock()
        .expect("players")
        .spawn(info, at(30.0, 200.0))
        .expect("spawned");
    assert_eq!(traffic.reported_spawns.load(Ordering::Relaxed), 1);

    let mut tick_loop = TickLoop::new(TickConfig::default());
    let system = players.clone();
    tick_loop.add_system("net", move |world, info| {
        system.lock().expect("players").run_tick(world, info);
    });
    let mut a = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("a connects");
    std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut a)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    let send = |epoch: u8, car_state: ClientState| {
        let owned = OwnedState::new(car, epoch, car_state, &VehicleBlock::INTACT.to_fields()).expect("owned");
        let mut packet = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::ClientState,
            sequence: 0,
            tick: 0,
        }
        .encode(&mut packet)
        .expect("header");
        at(10.0, 0.0)
            .encode_with_owned(&PedBlock::default(), &[owned], &mut packet)
            .expect("state");
        a.send_datagram(Bytes::from(packet)).expect("send");
    };
    let car_x = |players: &Mutex<net::Players>| {
        players
            .lock()
            .expect("players")
            .spawned(car)
            .map(|(_, s)| s.position.to_metres()[0])
    };
    send(0, at(30.0, 0.0));
    assert!(tick_until(&mut tick_loop, |_| {
        players
            .lock()
            .expect("players")
            .owner(car)
            .is_some_and(|(o, e)| o.is_some() && e == 1)
    }));
    // A jump the movement checks refuse is applied and counted; a stale epoch is still dropped.
    send(1, at(-135.0, 0.0));
    assert!(tick_until(&mut tick_loop, |_| car_x(&players) == Some(-135.0)));
    assert!(traffic.refused_owned_movement.load(Ordering::Relaxed) >= 1);
    send(0, at(-130.0, 0.0));
    assert!(wait_for(&traffic.refused_owned_stale, 1, || {
        tick_loop.run_tick();
    }));
    assert_eq!(car_x(&players), Some(-135.0));
}

/// SRV-19: a player that does not own a vehicle cannot move it, and when the owner leaves at speed, the next owner's
/// first state is checked from the last accepted one, so carrying on at the same speed is not refused.
#[test]
fn a_hand_off_at_speed_is_not_refused() {
    use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
    use sawgrass_protocol::game::OwnedState;
    use sawgrass_protocol::ped::PedBlock;
    use sawgrass_protocol::vehicle::VehicleBlock;
    use std::sync::{Arc, Mutex};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let traffic = players.traffic();
    let players = Arc::new(Mutex::new(players));
    let at = |x: f32, vx: f32| ClientState {
        position: Position::from_metres([x, 5.0, 30.0]).expect("position"),
        velocity: Velocity::from_metres_per_second([vx, 0.0, 0.0]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    };
    let info =
        Info::with_fields(EntityKind::Vehicle, AssetId(3), &VehicleBlock::INTACT.to_fields()).expect("info");
    let car = players
        .lock()
        .expect("players")
        .spawn(info, at(30.0, 0.0))
        .expect("spawned");

    let mut tick_loop = TickLoop::new(TickConfig::default());
    let system = players.clone();
    tick_loop.add_system("net", move |world, info| {
        system.lock().expect("players").run_tick(world, info);
    });
    let join = |tick_loop: &mut TickLoop| {
        let mut session = runtime
            .block_on(connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION))
            .expect("connects");
        let id = std::thread::scope(|scope| {
            let id = scope.spawn(|| runtime.block_on(joined(&mut session)));
            tick_until(tick_loop, |_| id.is_finished());
            id.join().expect("joined")
        });
        (session, id)
    };
    let (a, id_a) = join(&mut tick_loop);
    let (b, id_b) = join(&mut tick_loop);
    let send = |session: &Session, x: f32, owned: &[OwnedState]| {
        let mut packet = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::ClientState,
            sequence: 0,
            tick: 0,
        }
        .encode(&mut packet)
        .expect("header");
        at(x, 0.0)
            .encode_with_owned(&PedBlock::default(), owned, &mut packet)
            .expect("state");
        session.send_datagram(Bytes::from(packet)).expect("send");
    };
    let car_state = |epoch: u8, x: f32| {
        OwnedState::new(car, epoch, at(x, 40.0), &VehicleBlock::INTACT.to_fields()).expect("owned")
    };
    let owner = |players: &Mutex<net::Players>| players.lock().expect("players").owner(car);
    let car_x = |players: &Mutex<net::Players>| {
        players
            .lock()
            .expect("players")
            .spawned(car)
            .map(|(_, s)| s.position.to_metres()[0])
    };
    // A, at 25 m, is nearest and owns the car; it drives the car at 40 m/s.
    send(&b, 5.0, &[]);
    send(&a, 25.0, &[]);
    assert!(tick_until(&mut tick_loop, |_| owner(&players) == Some((Some(id_a), 1))));
    send(&a, 25.0, &[car_state(1, 31.0)]);
    assert!(tick_until(&mut tick_loop, |_| car_x(&players) == Some(31.0)));
    // B, not the owner, cannot move it with A's epoch.
    send(&b, 5.0, &[car_state(1, 50.0)]);
    assert!(wait_for(&traffic.refused_owned_stale, 1, || {
        tick_loop.run_tick();
    }));
    assert_eq!(car_x(&players), Some(31.0));
    // A leaves; B is handed the car and carries on from where A left it.
    a.close();
    assert!(tick_until(&mut tick_loop, |_| owner(&players) == Some((Some(id_b), 3))));
    send(&b, 5.0, &[car_state(3, 34.0)]);
    assert!(tick_until(&mut tick_loop, |_| car_x(&players) == Some(34.0)));
    assert_eq!(traffic.refused_owned_movement.load(Ordering::Relaxed), 0);
}

/// SRV-17, ADR 0006 section 7: a joining player is sent the clock, the weather and every door, paced over ticks,
/// then each change.
#[test]
fn a_joining_player_gets_the_world_state_then_its_changes() {
    use sawgrass_protocol::entity::AssetId;
    use sawgrass_protocol::world::Door;
    use std::sync::{Arc, Mutex};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let players = Arc::new(Mutex::new(players));
    // More doors than one tick sends, and more than the event queue holds.
    let doors = 100u32;
    {
        let mut p = players.lock().expect("players");
        p.set_weather(AssetId(3), AssetId(3), 0);
        for id in 1..=doors {
            p.set_door(Door {
                id: AssetId(id),
                locked: id % 2 == 0,
                open: 0,
            });
        }
    }
    let mut tick_loop = TickLoop::new(TickConfig::default());
    let system = players.clone();
    tick_loop.add_system("net", move |world, info| {
        system.lock().expect("players").run_tick(world, info);
    });
    let mut a = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("connects");
    std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut a)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    let mut read_until = |tick_loop: &mut TickLoop, done: &(dyn Fn(&[ServerEvent]) -> bool + Sync)| {
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                runtime.block_on(async {
                    let mut seen = Vec::new();
                    while !done(&seen) {
                        let (_, packet) = a.recv_event().await.expect("event");
                        if let Ok(event) = ServerEvent::decode(&packet[PacketHeader::SIZE..]) {
                            seen.push(event);
                        }
                    }
                    seen
                })
            });
            tick_until(tick_loop, |_| reader.is_finished());
            reader.join().expect("reader")
        })
    };
    let count_doors =
        |seen: &[ServerEvent]| seen.iter().filter(|e| matches!(e, ServerEvent::Door(_))).count();
    let seen = read_until(&mut tick_loop, &|seen| count_doors(seen) == doors as usize);
    // Each door once, in the order they were registered.
    let ids: Vec<u32> = seen
        .iter()
        .filter_map(|e| match e {
            ServerEvent::Door(d) => Some(d.id.0),
            _ => None,
        })
        .collect();
    assert_eq!(ids, (1..=doors).collect::<Vec<_>>());
    let environment = seen.iter().find_map(|e| match e {
        ServerEvent::Environment(env) => Some(*env),
        _ => None,
    });
    assert_eq!(environment.map(|e| (e.weather, e.rate)), Some((AssetId(3), 200)));
    assert!(seen.contains(&ServerEvent::Door(Door {
        id: AssetId(100),
        locked: true,
        open: 0,
    })));
    // Then a change reaches it by itself.
    let opened = Door {
        id: AssetId(7),
        locked: false,
        open: 255,
    };
    players.lock().expect("players").set_door(opened);
    let seen = read_until(&mut tick_loop, &|seen| seen.contains(&ServerEvent::Door(opened)));
    assert!(!seen.is_empty());
}

/// NET-20, ADR 0006 section 5: shots have their own budget, so a burst of shots does not use up the other events'
/// and is dropped beyond its own, without closing the connection.
#[test]
fn shots_are_limited_apart_from_other_events() {
    use sawgrass_protocol::combat::Shot;
    use sawgrass_protocol::entity::AssetId;
    use sawgrass_protocol::game::ClientEvent;

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (mut client, traffic, _players) = limited(&runtime, Limits::default());
    let packet = |event: ClientEvent| {
        let mut packet = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::Event,
            sequence: 0,
            tick: 0,
        }
        .encode(&mut packet)
        .expect("header");
        event.encode(&mut packet);
        packet
    };
    let from = Position::from_metres([1.0, 2.0, 3.0]).expect("position");
    let shot = packet(ClientEvent::Shot(Box::new(Shot::new(1, AssetId(9), from))));
    let other = packet(ClientEvent::RequestStats);
    // 30 shots against a burst of 20, and 30 other events against a burst of 40.
    for _ in 0..30 {
        runtime.block_on(client.send_event(&shot)).expect("send");
        runtime.block_on(client.send_event(&other)).expect("send");
    }
    let limits = Limits::default();
    assert!(wait_for(&traffic.shots_received, limits.shot_burst as u64, || ()));
    assert!(wait_for(&traffic.dropped_events, 1, || ()));
    std::thread::sleep(Duration::from_millis(100));
    let (shots, dropped) = (
        traffic.shots_received.load(Ordering::Relaxed),
        traffic.dropped_events.load(Ordering::Relaxed),
    );
    // Only shots were dropped: the other events stayed within their own burst.
    assert_eq!(shots + dropped, 30, "{shots} shots, {dropped} dropped");
    assert!(shots < 30);
    assert_eq!(traffic.closed_floods.load(Ordering::Relaxed), 0);
}

/// SRV-15, ADR 0006 section 4: the server seats a player who asks, is close and finds the seat free; the driver
/// owns the vehicle and the player moves with it; leaving is always granted; a far or locked request is refused.
#[test]
fn a_player_enters_drives_and_leaves_a_vehicle() {
    use sawgrass_protocol::entity::{AssetId, EntityKind, Info};
    use sawgrass_protocol::game::{ClientEvent, OwnedState};
    use sawgrass_protocol::ped::{Motion, PedBlock};
    use sawgrass_protocol::vehicle::VehicleBlock;
    use std::sync::{Arc, Mutex};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let traffic = players.traffic();
    let players = Arc::new(Mutex::new(players));
    let at = |x: f32| ClientState {
        position: Position::from_metres([x, 5.0, 30.0]).expect("position"),
        velocity: Velocity::from_metres_per_second([0.0; 3]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    };
    let info =
        Info::with_fields(EntityKind::Vehicle, AssetId(3), &VehicleBlock::INTACT.to_fields()).expect("info");
    let car = players
        .lock()
        .expect("players")
        .spawn(info, at(12.0))
        .expect("spawned");
    let far_car = players
        .lock()
        .expect("players")
        .spawn(info, at(60.0))
        .expect("spawned");

    let mut tick_loop = TickLoop::new(TickConfig::default());
    let system = players.clone();
    tick_loop.add_system("net", move |world, info| {
        system.lock().expect("players").run_tick(world, info);
    });
    let mut a = runtime
        .block_on(connect(addr, "localhost", roots, PROTOCOL_VERSION))
        .expect("a connects");
    let me = std::thread::scope(|scope| {
        let id = scope.spawn(|| runtime.block_on(joined(&mut a)));
        tick_until(&mut tick_loop, |_| id.is_finished());
        id.join().expect("joined")
    });
    let send = |a: &Session, x: f32, owned: &[OwnedState]| {
        let mut packet = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::ClientState,
            sequence: 0,
            tick: 0,
        }
        .encode(&mut packet)
        .expect("header");
        at(x)
            .encode_with_owned(&PedBlock::default(), owned, &mut packet)
            .expect("state");
        a.send_datagram(Bytes::from(packet)).expect("send");
    };
    let ask = |a: &mut Session, event: ClientEvent| {
        let mut packet = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::Event,
            sequence: 0,
            tick: 0,
        }
        .encode(&mut packet)
        .expect("header");
        event.encode(&mut packet);
        runtime.block_on(a.send_event(&packet)).expect("send");
    };
    fn lock(players: &Mutex<net::Players>) -> std::sync::MutexGuard<'_, net::Players> {
        players.lock().expect("players")
    }
    let my_x =
        |players: &Mutex<net::Players>| lock(players).entity(me).map(|(_, s)| s.position.to_metres()[0]);
    let my_motion = |players: &Mutex<net::Players>| {
        lock(players)
            .entity(me)
            .map(|(i, _)| PedBlock::from_fields(i.fields()))
    };

    send(&a, 10.0, &[]);
    assert!(tick_until(&mut tick_loop, |_| my_x(&players) == Some(10.0)));
    // Too far from the other car: refused.
    ask(
        &mut a,
        ClientEvent::EnterSeat {
            vehicle: far_car,
            seat: 0,
        },
    );
    assert!(wait_for(&traffic.seats_refused, 1, || {
        tick_loop.run_tick();
    }));
    // 2 m from this one: seated as its driver, which makes A its owner.
    ask(
        &mut a,
        ClientEvent::EnterSeat {
            vehicle: car,
            seat: 0,
        },
    );
    assert!(tick_until(&mut tick_loop, |_| lock(&players).seat_of(me) == Some((car, 0))));
    let ped = my_motion(&players).expect("ped").expect("block");
    assert_eq!((ped.motion, ped.vehicle, ped.seat), (Motion::InVehicle, car, 1));
    assert!(tick_until(&mut tick_loop, |_| {
        lock(&players).owner(car).is_some_and(|(o, _)| o == Some(me))
    }));
    let (_, epoch) = lock(&players).owner(car).expect("owner");
    // Driving: the car's state moves the player with it, whatever the player's own state says.
    let driven = OwnedState::new(car, epoch, at(15.0), &VehicleBlock::INTACT.to_fields()).expect("owned");
    send(&a, 10.0, &[driven]);
    assert!(tick_until(&mut tick_loop, |_| my_x(&players) == Some(15.0)));
    // Leaving is granted, and the first state outside is checked from where the car is.
    ask(&mut a, ClientEvent::LeaveSeat);
    assert!(tick_until(&mut tick_loop, |_| lock(&players)
        .seat_of(me)
        .is_none()));
    assert_eq!(
        my_motion(&players).expect("ped").expect("block").motion,
        Motion::Idle
    );
    send(&a, 16.0, &[]);
    assert!(tick_until(&mut tick_loop, |_| my_x(&players) == Some(16.0)));
    // A locked car refuses.
    assert!(lock(&players).set_locked(car, true));
    let granted = traffic.seats_granted.load(Ordering::Relaxed);
    ask(
        &mut a,
        ClientEvent::EnterSeat {
            vehicle: car,
            seat: 1,
        },
    );
    assert!(wait_for(&traffic.seats_refused, 2, || {
        tick_loop.run_tick();
    }));
    assert_eq!(traffic.seats_granted.load(Ordering::Relaxed), granted);
    assert_eq!(lock(&players).seat_of(me), None);
    // NET-21: leaving from no seat is answered too; a car that is removed takes its passenger out.
    ask(&mut a, ClientEvent::LeaveSeat);
    assert!(lock(&players).set_locked(car, false));
    ask(
        &mut a,
        ClientEvent::EnterSeat {
            vehicle: car,
            seat: 1,
        },
    );
    assert!(tick_until(&mut tick_loop, |_| lock(&players).seat_of(me) == Some((car, 1))));
    assert!(lock(&players).despawn(car));
    // Every request was answered, in order, with what the server did and why.
    use sawgrass_protocol::game::SeatAnswer;
    assert_eq!(
        seat_answers(&runtime, &mut a, 7),
        [
            (far_car, 0, SeatAnswer::CannotReach),
            (car, 0, SeatAnswer::Granted),
            (car, 0, SeatAnswer::Left),
            (car, 1, SeatAnswer::Locked),
            (0, 0, SeatAnswer::NotSeated),
            (car, 1, SeatAnswer::Granted),
            (car, 1, SeatAnswer::Left),
        ]
    );
}

/// SRV-16, ADR 0006 section 5: the server applies a hit only from a live shooter holding the weapon, at the fire
/// rate, against a recent tick, and where the target was; the damage is its own, down to death.
#[test]
fn a_checked_hit_damages_and_kills() {
    use sawgrass_protocol::combat::{Hit, Shot, Zone};
    use sawgrass_protocol::entity::AssetId;
    use sawgrass_protocol::entity::{EntityKind, Info};
    use sawgrass_protocol::game::ClientEvent;
    use sawgrass_protocol::ped::{Motion, PedBlock};
    use sawgrass_protocol::vehicle::VehicleBlock;
    use std::sync::{Arc, Mutex};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let traffic = players.traffic();
    let players = Arc::new(Mutex::new(players));
    let mut tick_loop = TickLoop::new(TickConfig::default());
    let system = players.clone();
    tick_loop.add_system("net", move |world, info| {
        system.lock().expect("players").run_tick(world, info);
    });
    let join = |tick_loop: &mut TickLoop| {
        let mut session = runtime
            .block_on(connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION))
            .expect("connects");
        let id = std::thread::scope(|scope| {
            let id = scope.spawn(|| runtime.block_on(joined(&mut session)));
            tick_until(tick_loop, |_| id.is_finished());
            id.join().expect("joined")
        });
        (session, id)
    };
    let (mut a, _) = join(&mut tick_loop);
    let (b, id_b) = join(&mut tick_loop);
    let at = |x: f32| Position::from_metres([x, 5.0, 30.0]).expect("position");
    let gun = AssetId(9);
    let send = |session: &Session, x: f32, weapon: AssetId| {
        let state = ClientState {
            position: at(x),
            velocity: Velocity::from_metres_per_second([0.0; 3]).expect("velocity"),
            rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
        };
        let mut packet = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::ClientState,
            sequence: 0,
            tick: 0,
        }
        .encode(&mut packet)
        .expect("header");
        let ped = PedBlock {
            weapon,
            ..PedBlock::default()
        };
        state.encode_with_ped(&ped, &mut packet).expect("state");
        session.send_datagram(Bytes::from(packet)).expect("send");
    };
    fn lock(players: &Mutex<net::Players>) -> std::sync::MutexGuard<'_, net::Players> {
        players.lock().expect("players")
    }
    let b_ped = |players: &Mutex<net::Players>| {
        let (info, _) = lock(players).entity(id_b).expect("b");
        PedBlock::from_fields(info.fields()).expect("block")
    };
    send(&a, 10.0, gun);
    send(&b, 20.0, AssetId::NONE);
    assert!(tick_until(&mut tick_loop, |_| lock(&players)
        .entity(id_b)
        .is_some()));
    // B drives a car parked a metre from it, so its death must also end its seat and its hold on the car.
    let info =
        Info::with_fields(EntityKind::Vehicle, AssetId(3), &VehicleBlock::INTACT.to_fields()).expect("info");
    let car_state = ClientState {
        position: at(21.0),
        velocity: Velocity::from_metres_per_second([0.0; 3]).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
    };
    let car = lock(&players).spawn(info, car_state).expect("spawned");
    let mut enter = vec![0; PacketHeader::SIZE];
    PacketHeader {
        kind: PacketKind::Event,
        sequence: 0,
        tick: 0,
    }
    .encode(&mut enter)
    .expect("header");
    ClientEvent::EnterSeat {
        vehicle: car,
        seat: 0,
    }
    .encode(&mut enter);
    let mut b = b;
    runtime.block_on(b.send_event(&enter)).expect("send");
    assert!(tick_until(&mut tick_loop, |_| lock(&players)
        .seat_of(id_b)
        .is_some()));
    // Far enough into the game that a shot can name a tick past the rewind limit.
    while lock(&players).tick() < 20 {
        tick_loop.run_tick();
    }
    let mut fire = |tick_loop: &mut TickLoop,
                    weapon: AssetId,
                    ago: u32,
                    from: f32,
                    point: f32,
                    zone: Zone,
                    hits: usize| {
        // Past the fire interval of the last shot.
        std::thread::sleep(Duration::from_millis(80));
        let tick = lock(&players).tick() - ago;
        let mut shot = Shot::new(tick, weapon, at(from));
        for _ in 0..hits {
            assert!(shot.push(Hit {
                target: id_b,
                zone,
                point: at(point),
            }));
        }
        let mut packet = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::Event,
            sequence: 0,
            tick: 0,
        }
        .encode(&mut packet)
        .expect("header");
        ClientEvent::Shot(Box::new(shot)).encode(&mut packet);
        // Counted before sending: the server may count the shot before `send_event` returns.
        let seen = traffic.shots_received.load(Ordering::Relaxed) + 1;
        runtime.block_on(a.send_event(&packet)).expect("send");
        assert!(wait_for(&traffic.shots_received, seen, || {
            tick_loop.run_tick();
        }));
        tick_loop.run_tick();
    };
    // A torso hit where B was a tick ago: 25 points.
    fire(&mut tick_loop, gun, 1, 10.0, 20.3, Zone::Torso, 1);
    assert_eq!(traffic.hits_accepted.load(Ordering::Relaxed), 1);
    assert_eq!(b_ped(&players).health, 75);
    // A weapon A does not hold, a tick past the rewind limit, and a shot from 10 m off where A is: refused whole.
    fire(&mut tick_loop, AssetId(8), 1, 10.0, 20.0, Zone::Torso, 1);
    fire(&mut tick_loop, gun, 11, 10.0, 20.0, Zone::Torso, 1);
    fire(&mut tick_loop, gun, 1, 0.0, 20.0, Zone::Torso, 1);
    assert_eq!(traffic.shots_refused.load(Ordering::Relaxed), 3);
    // More hits than the weapon has pellets: sixteen head hits from one pull of the trigger are refused whole.
    fire(&mut tick_loop, gun, 1, 10.0, 20.0, Zone::Head, 16);
    assert_eq!(traffic.shots_refused.load(Ordering::Relaxed), 4);
    assert_eq!(b_ped(&players).health, 75);
    // A hit point 5 m from where B was: that hit is refused.
    fire(&mut tick_loop, gun, 1, 10.0, 25.0, Zone::Torso, 1);
    assert_eq!(traffic.hits_refused.load(Ordering::Relaxed), 1);
    assert_eq!(b_ped(&players).health, 75);
    // A head hit is 100 points: B is dead, and stays dead whatever its client says.
    fire(&mut tick_loop, gun, 1, 10.0, 20.0, Zone::Head, 1);
    assert_eq!(
        (b_ped(&players).health, b_ped(&players).motion),
        (0, Motion::Dead)
    );
    assert_eq!(lock(&players).seat_of(id_b), None);
    // NET-21: B is told it got the seat, and that it left it when it died.
    use sawgrass_protocol::game::SeatAnswer;
    assert_eq!(
        seat_answers(&runtime, &mut b, 2),
        [(car, 0, SeatAnswer::Granted), (car, 0, SeatAnswer::Left)]
    );
    send(&b, 20.0, AssetId::NONE);
    for _ in 0..3 {
        tick_loop.run_tick();
    }
    assert_eq!(b_ped(&players).motion, Motion::Dead);
}

/// NET-22, ADR 0006 section 5: a client aims at targets where it draws them, interpolated between two snapshot
/// ticks some ticks behind the newest. The server checks a hit there, so an honest hit on a fast target passes;
/// the same hit checked at the shot's tick alone, or at the newest tick, is refused, and so is a shot past the
/// rewind cap.
#[test]
fn a_hit_on_a_moving_target_is_checked_where_the_client_drew_it() {
    use sawgrass_protocol::combat::{Hit, Shot, Zone};
    use sawgrass_protocol::entity::AssetId;
    use sawgrass_protocol::game::ClientEvent;
    use sawgrass_protocol::ped::PedBlock;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (addr, roots, players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        (addr, roots, net::listen(gateway, ListenConfig::default()))
    };
    let traffic = players.traffic();
    let players = Arc::new(Mutex::new(players));
    let mut tick_loop = TickLoop::new(TickConfig::default());
    let system = players.clone();
    tick_loop.add_system("net", move |world, info| {
        system.lock().expect("players").run_tick(world, info);
    });
    let join = |tick_loop: &mut TickLoop| {
        let mut session = runtime
            .block_on(connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION))
            .expect("connects");
        let id = std::thread::scope(|scope| {
            let id = scope.spawn(|| runtime.block_on(joined(&mut session)));
            tick_until(tick_loop, |_| id.is_finished());
            id.join().expect("joined")
        });
        (session, id)
    };
    let (mut a, _) = join(&mut tick_loop);
    let (b, id_b) = join(&mut tick_loop);
    let at = |x: f32| Position::from_metres([x, 5.0, 30.0]).expect("position");
    let gun = AssetId(9);
    let send = |session: &Session, x: f32, weapon: AssetId| {
        let state = ClientState {
            position: at(x),
            velocity: Velocity::from_metres_per_second([0.0; 3]).expect("velocity"),
            rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
        };
        let mut packet = vec![0; PacketHeader::SIZE];
        PacketHeader {
            kind: PacketKind::ClientState,
            sequence: 0,
            tick: 0,
        }
        .encode(&mut packet)
        .expect("header");
        let ped = PedBlock {
            weapon,
            ..PedBlock::default()
        };
        state.encode_with_ped(&ped, &mut packet).expect("state");
        session.send_datagram(Bytes::from(packet)).expect("send");
    };
    fn lock(players: &Mutex<net::Players>) -> std::sync::MutexGuard<'_, net::Players> {
        players.lock().expect("players")
    }
    send(&a, 10.0, gun);
    send(&b, 20.0, AssetId::NONE);
    assert!(tick_until(&mut tick_loop, |_| lock(&players)
        .entity(id_b)
        .is_some_and(|(_, s)| s.position.to_metres()[0] == 20.0)));
    // B runs back and forth at 120 m/s (6 m a tick, within the movement rules' 150 m/s), and where the server
    // had it is noted after every tick.
    let mut seen_at: HashMap<u32, f32> = HashMap::new();
    let mut k = 0u32;
    let mut step = |tick_loop: &mut TickLoop, seen_at: &mut HashMap<u32, f32>| {
        k += 1;
        let leg = k % 30;
        let x = 20.0 + 6.0 * if leg < 15 { leg as f32 } else { (30 - leg) as f32 };
        send(&b, x, AssetId::NONE);
        // Paced like real ticks, so the movement checks see 120 m/s.
        std::thread::sleep(Duration::from_millis(50));
        tick_loop.run_tick();
        let players = lock(&players);
        if let Some((_, s)) = players.entity(id_b) {
            seen_at.insert(players.tick(), s.position.to_metres()[0]);
        }
    };
    for _ in 0..30 {
        step(&mut tick_loop, &mut seen_at);
    }
    // Fires at B where a client 4 ticks behind the newest, halfway to the next tick, draws it; `tick` and `frac`
    // say what the shot names, `point` where it aims.
    let mut fire =
        |tick_loop: &mut TickLoop, seen_at: &mut HashMap<u32, f32>, named: &dyn Fn(u32, u32) -> (u32, u8)| {
            // Past the fire interval, with B still running.
            for _ in 0..2 {
                step(tick_loop, seen_at);
            }
            let newest = lock(&players).tick();
            let render = (newest - 6..newest - 3)
                .find(|t| match (seen_at.get(t), seen_at.get(&(t + 1))) {
                    (Some(a), Some(b)) => (b - a).abs() >= 5.0,
                    _ => false,
                })
                .expect("B moved between two recent ticks");
            let drawn = (seen_at[&render] + seen_at[&(render + 1)]) / 2.0;
            let (tick, frac) = named(render, newest);
            let mut shot = Shot::new(tick, gun, at(10.0));
            shot.frac = frac;
            assert!(shot.push(Hit {
                target: id_b,
                zone: Zone::Torso,
                point: at(drawn),
            }));
            let mut packet = vec![0; PacketHeader::SIZE];
            PacketHeader {
                kind: PacketKind::Event,
                sequence: 0,
                tick: 0,
            }
            .encode(&mut packet)
            .expect("header");
            ClientEvent::Shot(Box::new(shot)).encode(&mut packet);
            let seen = traffic.shots_received.load(Ordering::Relaxed) + 1;
            runtime.block_on(a.send_event(&packet)).expect("send");
            assert!(wait_for(&traffic.shots_received, seen, || {
                step(tick_loop, seen_at);
            }));
            step(tick_loop, seen_at);
        };
    // At the render time, between two ticks: accepted.
    fire(&mut tick_loop, &mut seen_at, &|render, _| (render, 128));
    assert_eq!(traffic.hits_accepted.load(Ordering::Relaxed), 1);
    // The same aim, named at the render tick alone: 3 m from where B was, refused.
    let refused = traffic.hits_refused.load(Ordering::Relaxed);
    fire(&mut tick_loop, &mut seen_at, &|render, _| (render, 0));
    assert_eq!(traffic.hits_refused.load(Ordering::Relaxed), refused + 1);
    // Named at the newest tick, as before NET-22: refused.
    fire(&mut tick_loop, &mut seen_at, &|_, newest| (newest, 0));
    assert_eq!(traffic.hits_refused.load(Ordering::Relaxed), refused + 2);
    // Past the rewind cap of 500 ms (10 ticks): the shot is refused whole.
    let shots_refused = traffic.shots_refused.load(Ordering::Relaxed);
    fire(&mut tick_loop, &mut seen_at, &|_, newest| (newest - 11, 128));
    assert_eq!(traffic.shots_refused.load(Ordering::Relaxed), shots_refused + 1);
    assert_eq!(traffic.hits_accepted.load(Ordering::Relaxed), 1);
}
