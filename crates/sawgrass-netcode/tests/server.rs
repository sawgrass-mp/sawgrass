//! The network core against the real server and a bot over loopback: an observer client built on [`Client`]
//! interpolates a walking bot, and its poses are compared with where the server held the bot at each tick.
//!
//! `cargo test -p sawgrass-netcode --test server -- --nocapture` prints the figures.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use sawgrass_bots::stats::Counters;
use sawgrass_bots::walker::Area;
use sawgrass_bots::{BotConfig, Joined, run};
use sawgrass_gateway::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sawgrass_gateway::{Gateway, RootCertStore, connect};
use sawgrass_netcode::interp::{Event, Sample};
use sawgrass_netcode::local::Correction;
use sawgrass_netcode::{Client, ClientConfig, Pose};
use sawgrass_protocol::entity::EntityKind;
use sawgrass_protocol::game::ServerEvent;
use sawgrass_protocol::{PROTOCOL_VERSION, PacketHeader};
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

/// The server's position at a fractional tick, linear between the ticks recorded.
fn truth(track: &[(u32, [f32; 3])], tick: f64) -> Option<[f32; 3]> {
    let i = track.partition_point(|(t, _)| f64::from(*t) <= tick);
    let (t0, a) = *track.get(i.checked_sub(1)?)?;
    let (t1, b) = *track.get(i)?;
    if t1 != t0 + 1 {
        return None;
    }
    let s = (tick - f64::from(t0)) as f32;
    Some([0, 1, 2].map(|k| a[k] + (b[k] - a[k]) * s))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observer_interpolates_a_walking_bot_close_to_the_server() {
    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    let mut players = net::listen(gateway, ListenConfig::default());
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));

    let secs = 6;
    let stop = Instant::now() + Duration::from_secs(secs);
    // The bot walks a 60 m square around the observer, who stands at its centre.
    let config = Arc::new(BotConfig {
        server: addr,
        server_name: "localhost".to_owned(),
        roots: roots.clone(),
        send_hz: 20,
        area: Area {
            size: 60.0,
            hotspots: 0,
            ..Area::default()
        },
        record: None,
        decoders: u64::MAX,
        sockets: Vec::new(),
        pay_every: None,
        payees: Arc::default(),
    });
    let (tx, bot_joined) = tokio::sync::oneshot::channel::<Joined>();
    tokio::spawn(run(
        config,
        0,
        Arc::new(Counters::default()),
        false,
        stop,
        Some(tx),
    ));

    // The tick loop on its own thread at its own schedule, as in the server binary, recording where the server
    // holds every entity after each tick.
    let ticks = std::thread::spawn(move || {
        let mut held: Vec<(u32, u32, [f32; 3])> = Vec::new();
        tick_loop
            .run(|tick_loop| {
                if let Some(tick) = tick_loop.tick().checked_sub(1) {
                    for id in 0..4 {
                        if let Some(p) = position(tick_loop, id) {
                            held.push((id, tick as u32, p));
                        }
                    }
                }
                if std::time::Instant::now() < stop.into_std() {
                    std::ops::ControlFlow::Continue(())
                } else {
                    std::ops::ControlFlow::Break(())
                }
            })
            .expect("no system panicked");
        held
    });

    let bot = tokio::time::timeout(Duration::from_secs(5), bot_joined)
        .await
        .expect("bot joined in time")
        .expect("joined");

    // The observer: a client on the network core, standing still at the centre of the bot's area.
    let session = connect(addr, "localhost", roots, PROTOCOL_VERSION)
        .await
        .expect("connect");
    let (datagrams, _events_out, mut events) = session.split();
    let (entity, tick_hz) = loop {
        let (_, event) = events.recv().await.expect("event");
        if let Ok(ServerEvent::Joined { entity, tick_hz }) = ServerEvent::decode(&event[PacketHeader::SIZE..])
        {
            break (entity, tick_hz);
        }
    };
    assert_ne!(entity, bot.entity);
    let mut client = Client::new(entity, tick_hz, ClientConfig::default());
    let start = std::time::Instant::now();
    let (arrived_tx, mut arrived) = tokio::sync::mpsc::unbounded_channel::<(f64, Bytes)>();
    let reader_datagrams = datagrams.clone();
    let reader = tokio::spawn(async move {
        while let Ok((_, packet)) = reader_datagrams.recv().await {
            if arrived_tx.send((start.elapsed().as_secs_f64(), packet)).is_err() {
                break;
            }
        }
    });

    let me = Pose::at([0.0, 0.0, 30.0]);
    let mut frames = tokio::time::interval(Duration::from_secs(1) / 60);
    let mut shown: Vec<(f64, [f32; 3])> = Vec::new();
    let mut out: Vec<Sample> = Vec::new();
    let mut frame_count = 0usize;
    let mut appeared = 0usize;
    while Instant::now() < stop {
        frames.tick().await;
        let now = start.elapsed().as_secs_f64();
        while let Ok((at, packet)) = arrived.try_recv() {
            let _ = client.receive(&packet, at);
        }
        client.remote(now, &mut out);
        if let (Some(render), Some(s)) = (client.render_tick(), out.iter().find(|s| s.id == bot.entity)) {
            shown.push((render, s.pose.position));
            if s.event == Some(Event::Appeared) {
                appeared += 1;
            }
            assert_eq!(s.info.kind, EntityKind::Player);
        }
        assert!(
            !client.removed().contains(&bot.entity),
            "the bot never left the view"
        );
        if client.clock().synced() {
            frame_count += 1;
        }
        if let Some(packet) = client.outgoing(now, &me) {
            let _ = datagrams.send(Bytes::from(packet));
        }
    }
    reader.abort();
    datagrams.close();

    let held = tokio::task::spawn_blocking(move || ticks.join().expect("tick thread"))
        .await
        .expect("join");
    let track: Vec<(u32, [f32; 3])> = held
        .iter()
        .filter(|(id, _, _)| *id == bot.entity)
        .map(|&(_, t, p)| (t, p))
        .collect();
    assert!(track.len() > 60, "server held the bot for {} ticks", track.len());

    let mut errors: Vec<f32> = shown
        .iter()
        .filter_map(|&(render, p)| {
            let q = truth(&track, render)?;
            Some(((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt())
        })
        .collect();
    errors.sort_by(f32::total_cmp);
    let pct = |p: f64| errors[((errors.len() - 1) as f64 * p) as usize];
    let (p50, p95) = (pct(0.5), pct(0.95));
    println!(
        "observer: {} frames synced, bot shown in {}, compared {}, error p50 {:.1} cm, p95 {:.1} cm, max {:.1} cm, \
         render delay {:.2} ticks, jitter {:.1} ms",
        frame_count,
        shown.len(),
        errors.len(),
        p50 * 100.0,
        p95 * 100.0,
        errors.last().copied().unwrap_or(0.0) * 100.0,
        client.clock().delay_ticks(),
        client.clock().jitter_secs() * 1000.0,
    );
    assert!(errors.len() > 100, "compared {} frames", errors.len());
    assert_eq!(appeared, 1, "the bot is spawned once");
    // The bot was shown in almost every frame once the clock had a snapshot.
    assert!(
        shown.len() * 10 >= frame_count * 9,
        "shown {} of {frame_count}",
        shown.len()
    );
    assert!(p50 < 0.05, "p50 {p50}");
    assert!(p95 < 0.30, "p95 {p95}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_move_is_corrected_back_to_where_the_server_holds_the_player() {
    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    let mut players = net::listen(gateway, ListenConfig::default());
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
    let stop = Instant::now() + Duration::from_secs(4);
    let ticks = std::thread::spawn(move || {
        tick_loop
            .run(|_| {
                if std::time::Instant::now() < stop.into_std() {
                    std::ops::ControlFlow::Continue(())
                } else {
                    std::ops::ControlFlow::Break(())
                }
            })
            .expect("no system panicked");
    });

    let session = connect(addr, "localhost", roots, PROTOCOL_VERSION)
        .await
        .expect("connect");
    let (datagrams, _events_out, mut events) = session.split();
    let (entity, tick_hz) = loop {
        let (_, event) = events.recv().await.expect("event");
        if let Ok(ServerEvent::Joined { entity, tick_hz }) = ServerEvent::decode(&event[PacketHeader::SIZE..])
        {
            break (entity, tick_hz);
        }
    };
    let mut client = Client::new(entity, tick_hz, ClientConfig::default());
    let start = std::time::Instant::now();
    let now = || start.elapsed().as_secs_f64();
    // Events are read on their own task, as a game module would, and handed over each frame.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let reader = tokio::spawn(async move {
        while let Ok((_, event)) = events.recv().await {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });

    // Stand at the spawn point for half a second, so the server accepts it, then jump 1 km in one frame, as a
    // game teleport the server was not told about would.
    let mut pose = Pose::at([0.0, 0.0, 30.0]);
    let mut frames = tokio::time::interval(Duration::from_secs(1) / 60);
    let mut correction = None;
    while Instant::now() < stop && correction.is_none() {
        frames.tick().await;
        if now() > 0.5 {
            pose = Pose::at([1000.0, 0.0, 30.0]);
        }
        if let Some(packet) = client.outgoing(now(), &pose) {
            let _ = datagrams.send(Bytes::from(packet));
        }
        while let Ok(event) = event_rx.try_recv() {
            if let Ok(ServerEvent::Corrected(state)) = ServerEvent::decode(&event[PacketHeader::SIZE..]) {
                correction = client.corrected(&state, &pose);
            }
        }
    }
    reader.abort();
    datagrams.close();
    tokio::task::spawn_blocking(move || ticks.join().expect("tick thread"))
        .await
        .expect("join");

    // 1 km is past the snap distance: the game module puts the player back where the server held it.
    match correction {
        Some(Correction::Snap(back)) => {
            assert!(
                back.position
                    .iter()
                    .zip([0.0, 0.0, 30.0])
                    .all(|(a, b)| (a - b).abs() < 0.01),
                "{back:?}"
            );
        }
        other => panic!("expected a snap back, got {other:?}"),
    }
}
