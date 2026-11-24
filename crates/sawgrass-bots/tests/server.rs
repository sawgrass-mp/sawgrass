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
use sawgrass_server::net::{self, NetId, PlayerState};
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
    let mut players = net::listen(gateway, false);
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
            // The first bot's entity is number 1: the server numbers connections from 1.
            if let Some(p) = position(&tick_loop, 1) {
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
