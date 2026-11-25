//! Bots connect to a real server through its gateway over loopback: the server places them in its world, they
//! move there, and each one sees the other in its snapshots.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sawgrass_bots::stats::Counters;
use sawgrass_bots::walker::Area;
use sawgrass_bots::{BotConfig, Joined, run};
use sawgrass_gateway::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sawgrass_gateway::{Gateway, RootCertStore};
use sawgrass_server::net::{self, ListenConfig, NetId, PlayerState};
use sawgrass_server::{TickConfig, TickLoop};
use tokio::time::Instant;

fn gateway() -> (Gateway, RootCertStore) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let mut roots = RootCertStore::empty();
    roots.add(der.clone()).expect("root");
    let gateway = Gateway::bind(([127, 0, 0, 1], 0).into(), vec![der], key).expect("bind");
    (gateway, roots)
}

/// Where the server holds entity `id`, if it does.
fn position(tick_loop: &TickLoop, id: u32) -> Option<[f32; 3]> {
    let world = tick_loop.world();
    let mut query = world.query::<(&NetId, &PlayerState)>();
    query
        .iter()
        .find(|(net, _)| net.0 == id)
        .map(|(_, state)| state.0.position.to_metres())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bots_move_on_the_server_and_see_each_other() {
    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    let mut players = net::listen(gateway, ListenConfig::default());
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));

    // Two bots in a 100 m square: always within each other's view radius.
    let config = Arc::new(BotConfig {
        server: addr,
        server_name: "localhost".to_owned(),
        roots,
        send_hz: 20,
        area: Area {
            size: 100.0,
            hotspots: 0,
            ..Area::default()
        },
        record: None,
        decoders: u64::MAX,
        sockets: Vec::new(),
    });
    let counters = Arc::new(Counters::default());
    let stop = Instant::now() + Duration::from_secs(4);
    let mut joined = Vec::new();
    for index in 0..2 {
        let (tx, rx) = tokio::sync::oneshot::channel::<Joined>();
        tokio::spawn(run(
            config.clone(),
            index,
            counters.clone(),
            false,
            stop,
            Some(tx),
        ));
        joined.push(rx);
    }

    // The tick loop runs on its own thread, as in the server binary.
    let ticks = std::thread::spawn(move || {
        let (mut track, mut moves) = (Vec::new(), Vec::new());
        while std::time::Instant::now() < stop.into_std() {
            tick_loop.run_tick();
            track.push(tick_loop.world().query::<&NetId>().iter().count());
            // The bot in the first slot: its entity id is 0 (slot 0, generation 0, IM-4).
            if let Some(p) = position(&tick_loop, 0) {
                moves.push(p);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        (track, moves)
    });

    let first = tokio::time::timeout(Duration::from_secs(5), async {
        let a = joined.remove(0).await.expect("joined");
        let b = joined.remove(0).await.expect("joined");
        (a, b)
    })
    .await
    .expect("bots joined in time");
    assert_ne!(first.0.entity, first.1.entity);
    assert_eq!(first.0.tick_hz, 20);

    let (track, moves) = tokio::task::spawn_blocking(move || ticks.join().expect("tick thread"))
        .await
        .expect("join");
    assert!(track.contains(&2), "both bots placed in the world: {track:?}");
    // The server saw the bot move, and only inside its area.
    assert!(moves.len() > 40, "position held for {} ticks", moves.len());
    assert!(
        moves.iter().all(|p| p[0].abs() <= 50.5 && p[1].abs() <= 50.5),
        "{moves:?}"
    );
    assert!(
        moves.first() != moves.last(),
        "bot did not move: {:?}",
        moves.first()
    );
    let sent = counters.sent_datagrams.load(Ordering::Relaxed);
    let entities = counters.received_entities.load(Ordering::Relaxed);
    let ticks_seen = counters.ticks_seen.load(Ordering::Relaxed);
    assert_eq!(counters.connected.load(Ordering::Relaxed), 2);
    assert!(sent > 100, "sent {sent}");
    assert!(ticks_seen > 60, "snapshots for {ticks_seen} ticks");
    assert!(entities > 60, "each bot saw the other: {entities} states");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn misbehaving_bots_are_stopped_and_an_honest_one_plays_on() {
    use sawgrass_bots::misbehave::{self, Kind, Misbehaving};
    use sawgrass_server::limits::Limits;

    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    // Floods are judged over 100 ms windows, so one is closed within the test.
    let config = ListenConfig {
        limits: Limits {
            window: Duration::from_millis(100),
            ..Limits::default()
        },
        ..ListenConfig::default()
    };
    let mut players = net::listen(gateway, config);
    let traffic = players.traffic();
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));

    let config = Arc::new(BotConfig {
        server: addr,
        server_name: "localhost".to_owned(),
        roots,
        send_hz: 20,
        area: Area {
            size: 100.0,
            hotspots: 0,
            ..Area::default()
        },
        record: None,
        decoders: u64::MAX,
        sockets: Vec::new(),
    });
    let stop = Instant::now() + Duration::from_secs(3);
    let honest = Arc::new(Counters::default());
    tokio::spawn(run(config.clone(), 0, honest.clone(), false, stop, None));
    let bad = Arc::new(Misbehaving::default());
    for (i, kind) in Kind::ALL.into_iter().enumerate() {
        tokio::spawn(misbehave::run(
            config.clone(),
            kind,
            1 + i as u64,
            bad.clone(),
            stop,
        ));
    }
    let ticks = std::thread::spawn(move || {
        while std::time::Instant::now() < stop.into_std() + Duration::from_millis(600) {
            tick_loop.run_tick();
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    tokio::task::spawn_blocking(move || ticks.join().expect("tick thread"))
        .await
        .expect("join");

    let load = |n: &std::sync::atomic::AtomicU64| n.load(Ordering::Relaxed);
    // Closed by the server, and counted on both sides.
    assert!(load(&bad.of(Kind::Flood).closed) >= 1, "flood not closed");
    assert!(load(&traffic.closed_floods) >= 1);
    assert!(load(&bad.of(Kind::BadEvent).closed) >= 1, "bad event not closed");
    assert!(load(&traffic.closed_bad_events) >= 1);
    // Refused, not closed: a teleport is a movement problem, not a protocol one.
    assert!(load(&traffic.refused_teleport) >= 10);
    assert_eq!(load(&bad.of(Kind::Teleport).closed), 0);
    assert!(load(&bad.of(Kind::Reconnect).connects) >= 5);
    // The honest bot played through all of it.
    assert_eq!(load(&honest.connected), 1);
    assert_eq!(load(&honest.dropped), 0);
    assert!(
        load(&honest.ticks_seen) > 30,
        "honest bot saw {} ticks",
        load(&honest.ticks_seen)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_bots_retry_and_light_bots_on_shared_sockets_play() {
    use sawgrass_gateway::Connector;
    use sawgrass_server::limits::Limits;

    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    // One handshake at a time: bots that connect together are refused until they try again (BOT-5).
    let config = ListenConfig {
        limits: Limits {
            max_handshakes: 1,
            ..Limits::default()
        },
        ..ListenConfig::default()
    };
    let mut players = net::listen(gateway, config);
    let traffic = players.traffic();
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));

    const BOTS: u64 = 12;
    let sockets = (0..3)
        .map(|_| Connector::new(false, roots.clone()).expect("socket"))
        .collect();
    let config = Arc::new(BotConfig {
        server: addr,
        server_name: "localhost".to_owned(),
        roots,
        send_hz: 20,
        area: Area {
            size: 100.0,
            hotspots: 0,
            ..Area::default()
        },
        record: None,
        decoders: 2,
        sockets,
    });
    let counters = Arc::new(Counters::default());
    let stop = Instant::now() + Duration::from_secs(5);
    for index in 0..BOTS {
        tokio::spawn(run(config.clone(), index, counters.clone(), false, stop, None));
    }
    let ticks = std::thread::spawn(move || {
        while std::time::Instant::now() < stop.into_std() + Duration::from_millis(300) {
            tick_loop.run_tick();
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    tokio::task::spawn_blocking(move || ticks.join().expect("tick thread"))
        .await
        .expect("join");

    let load = |n: &std::sync::atomic::AtomicU64| n.load(Ordering::Relaxed);
    assert_eq!(load(&counters.connected), BOTS, "every bot got in");
    assert_eq!(load(&counters.failed), 0);
    assert!(
        load(&traffic.refused_handshakes) >= 1 && load(&counters.connect_retries) >= 1,
        "no bot was refused, so none had to retry: {} refused, {} retries",
        load(&traffic.refused_handshakes),
        load(&counters.connect_retries)
    );
    // Every bot acknowledged snapshots; only the first two decoded them.
    let (seen, decoded) = (load(&counters.ticks_seen), load(&counters.decoded_ticks));
    assert!(seen > BOTS * 40, "snapshots for {seen} ticks");
    assert!(
        decoded > 40 && decoded * 3 < seen,
        "{decoded} of {seen} ticks decoded"
    );
    assert!(
        load(&counters.received_entities) > decoded,
        "decoders saw the others"
    );
}
