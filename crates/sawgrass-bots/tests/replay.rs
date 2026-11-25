//! A session recorded by one server (`--record`) and played against a second one (`sawgrass-bots replay`): the
//! second server sees as many players, every datagram, and each player's last recorded position.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use sawgrass_bots::replay;
use sawgrass_bots::stats::Counters;
use sawgrass_bots::walker::Area;
use sawgrass_bots::{BotConfig, run};
use sawgrass_gateway::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sawgrass_gateway::{Connector, Gateway, RootCertStore};
use sawgrass_protocol::PacketHeader;
use sawgrass_protocol::game::ClientState;
use sawgrass_protocol::recording::RecordKind;
use sawgrass_server::net::{self, ListenConfig, NetId, PlayerState};
use sawgrass_server::recording::Recorder;
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

/// Every player position the server holds, as bits so they can be compared exactly.
fn positions(tick_loop: &TickLoop) -> Vec<[u32; 3]> {
    let world = tick_loop.world();
    let mut query = world.query::<(&NetId, &PlayerState)>();
    query
        .iter()
        .map(|(_, state)| state.0.position.to_metres().map(f32::to_bits))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_session_plays_again_on_another_server() {
    const BOTS: u64 = 3;
    let path = std::env::temp_dir().join(format!("sawgrass-replay-{}.rec", std::process::id()));

    // Record three walking bots on the first server.
    let recorder = Recorder::create(&path).expect("recorder");
    let (first, roots) = gateway();
    let addr = first.local_addr().expect("addr");
    let mut players = net::listen_recorded(first, ListenConfig::default(), Some(recorder.clone()));
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
        pay_every: None,
        payees: Arc::default(),
    });
    let counters = Arc::new(Counters::default());
    let stop = Instant::now() + Duration::from_secs(2);
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
    recorder.finish().expect("finish");
    assert_eq!(recorder.dropped(), 0);

    let file = std::fs::read(&path).expect("read");
    let _ = std::fs::remove_file(&path);
    let scripts = replay::parse(&file).expect("parse");
    assert_eq!(scripts.len(), BOTS as usize);
    let mut datagrams = 0;
    let mut last = HashSet::new();
    for script in scripts.values() {
        assert!(script.join.is_some());
        let states: Vec<_> = script
            .sends
            .iter()
            .filter(|(_, kind, _)| *kind == RecordKind::Datagram)
            .collect();
        assert!(states.len() > 10, "{} states recorded", states.len());
        datagrams += states.len() as u64;
        let (_, _, payload) = states.last().expect("a state");
        let (state, _) = ClientState::decode_with_ped(&payload[PacketHeader::SIZE..]).expect("state");
        last.insert(state.position.to_metres().map(f32::to_bits));
    }

    // Play it on a second server, recording every position it holds.
    let (second, roots_b) = gateway();
    let addr = second.local_addr().expect("addr");
    let mut players = net::listen(second, ListenConfig::default());
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ticking = done.clone();
    let ticks = std::thread::spawn(move || {
        let (mut seen, mut most) = (HashSet::new(), 0);
        while !ticking.load(std::sync::atomic::Ordering::Relaxed) {
            tick_loop.run_tick();
            let now = positions(&tick_loop);
            most = most.max(now.len());
            seen.extend(now);
            std::thread::sleep(Duration::from_millis(20));
        }
        (seen, most)
    });
    let connector = Arc::new(Connector::new(false, roots_b).expect("connector"));
    let replayed = replay::replay(scripts, addr, "localhost", connector, 1.0).await;
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let (seen, most) = tokio::task::spawn_blocking(move || ticks.join().expect("tick thread"))
        .await
        .expect("join");

    let load = |n: &std::sync::atomic::AtomicU64| n.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!((load(&replayed.connected), load(&replayed.failed)), (BOTS, 0));
    assert_eq!(load(&replayed.datagrams), datagrams);
    assert_eq!(most, BOTS as usize);
    for position in &last {
        assert!(
            seen.contains(position),
            "last recorded position {position:?} never held by the second server"
        );
    }
}
