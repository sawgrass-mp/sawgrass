//! Clients connect through the in-process gateway: the server places each one in its world where the client
//! says it is, and sends each a snapshot with the other.

use std::time::Duration;

use bytes::Bytes;
use sawgrass_gateway::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sawgrass_gateway::{Gateway, RootCertStore, Session, connect};
use sawgrass_protocol::game::{ClientState, ServerEvent, decode_snapshot};
use sawgrass_protocol::{
    EntityState, PROTOCOL_VERSION, PacketHeader, PacketKind, Position, Rotation, Velocity,
};
use sawgrass_server::net::{self, NetId, PlayerState};
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
        (addr, roots, net::listen(gateway, false))
    };
    let mut tick_loop = TickLoop::new(TickConfig::default());

    let (mut a, mut b) = runtime.block_on(async {
        let a = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
        let b = connect(addr, "localhost", roots, PROTOCOL_VERSION).await;
        (a.expect("a connects"), b.expect("b connects"))
    });
    // Joins reach the tick thread through the channel; ticks send the Joined events.
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
                loop {
                    let (header, packet) = b.recv_datagram().await.expect("snapshot");
                    assert_eq!(header.kind, PacketKind::Snapshot);
                    let mut states = Vec::new();
                    decode_snapshot(&packet[PacketHeader::SIZE..], |s| states.push(s)).expect("decode");
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
}
