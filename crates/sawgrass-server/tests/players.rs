//! Clients connect through the in-process gateway: the server places each one in its world where the client
//! says it is, and sends each a snapshot with the other. A flood of states costs one slot, not a queue, and
//! connections beyond the player cap are refused. Each connection is held to its rates: states beyond them are
//! dropped, a sustained flood or an undecodable event closes the connection, and one address gets only so many
//! connections. A client that teleports stays where it was.

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
    // A step it could have walked is taken.
    let step = state_packet(12.0);
    assert!(tick_until(&mut tick_loop, |t| {
        let _ = client.send_datagram(step.clone());
        position_of(t, id) == Some([12.0, 5.0, 30.0])
    }));
}
