//! The first milestone with the game, without the game: two modules, each in its own fake game with a local
//! player, connect to the real server over loopback, and each comes to show the other player's ped where that
//! player stands.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sawgrass_client_module::assets::AssetTable;
use sawgrass_client_module::connection::{ConnectConfig, Connection, Status};
use sawgrass_client_module::fake::FakeRuntime;
use sawgrass_client_module::game::{EntityKind, FakeGame, GameAsset, Pose};
use sawgrass_client_module::link::NetLink;
use sawgrass_client_module::module::{Module, all_functions};
use sawgrass_client_module::runtime::{GameBuild, RuntimeKind};
use sawgrass_client_module::table::FunctionTable;
use sawgrass_gateway::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sawgrass_gateway::{Gateway, RootCertStore};
use sawgrass_netcode::ClientConfig;
use sawgrass_server::net::{self, ListenConfig};
use sawgrass_server::{TickConfig, TickLoop};

const BUILD_KEY: &str = "victory/1.0.0/00ff";
const PED: GameAsset = GameAsset(0x2000);

fn gateway() -> (Gateway, RootCertStore) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let mut roots = RootCertStore::empty();
    roots.add(der.clone()).expect("root");
    let gateway = Gateway::bind(([127, 0, 0, 1], 0).into(), vec![der], key).expect("bind");
    (gateway, roots)
}

fn module() -> Module<FakeRuntime> {
    let build = GameBuild {
        runtime: RuntimeKind::Victory,
        title_version: "1.0.0".into(),
        image_digest: "00ff".into(),
    };
    let names = all_functions();
    let mut image = vec![0u8; 0x10 * names.len()];
    let mut table = format!("build = {BUILD_KEY}\n");
    for (i, name) in names.iter().enumerate() {
        image[0x10 * i..0x10 * i + 4].copy_from_slice(&[0x5A, i as u8, 0xA5, 0x3C]);
        table += &format!("{name} = 5A {i:02X} A5 3C\n");
    }
    let functions = FunctionTable::parse(&table).expect("functions");
    let assets = AssetTable::parse(&format!("build = {BUILD_KEY}\nped = 0x2000\n")).expect("assets");
    let Ok(module) = Module::start(FakeRuntime::new(build, 0x4000, image), &functions, assets) else {
        panic!("module starts");
    };
    module
}

/// One player: a module in a fake game whose local player stands at `at`, connected to the server.
struct Player {
    module: Module<FakeRuntime>,
    game: FakeGame,
    link: NetLink,
    connection: Connection,
}

impl Player {
    fn join(at: [f32; 3], server: ConnectConfig) -> Self {
        let mut game = FakeGame::default();
        let me = game.create_natively(
            EntityKind::Ped,
            PED,
            Pose {
                position: at,
                heading: 0.0,
            },
        );
        game.local_player = Some(me);
        let link = NetLink::joining(ClientConfig::default());
        let connection = Connection::start(server, &link).expect("connection thread");
        Self {
            module: module(),
            game,
            link,
            connection,
        }
    }

    fn frame(&mut self, dt: f32) {
        self.module
            .frame(dt, &mut self.game, &mut self.link)
            .expect("frame");
    }

    fn entity(&self) -> Option<u32> {
        self.link.client().map(|c| c.entity())
    }

    /// Where this player's game shows the server entity `id`.
    fn shows(&self, id: u32) -> Option<(EntityKind, GameAsset, [f32; 3])> {
        let handle = self.module.entities().handle(id)?;
        self.game
            .entities
            .get(&handle)
            .map(|(kind, asset, pose)| (*kind, *asset, pose.position))
    }
}

#[test]
fn two_clients_see_each_other() {
    // The server's network side on a runtime of its own, the tick loop on its own thread, as in the server binary.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let guard = runtime.enter();
    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    let mut players = net::listen(gateway, ListenConfig::default());
    drop(guard);
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let server = std::thread::spawn(move || {
        let mut tick_loop = TickLoop::new(TickConfig::default());
        tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
        tick_loop
            .run(|_| {
                if !server_done.load(Ordering::Relaxed) {
                    std::ops::ControlFlow::Continue(())
                } else {
                    std::ops::ControlFlow::Break(())
                }
            })
            .expect("no system panicked");
    });

    let config = ConnectConfig {
        server: addr,
        server_name: "localhost".into(),
        roots,
    };
    let (a_at, b_at) = ([10.0, 20.0, 30.0], [14.0, 23.0, 30.0]);
    let mut a = Player::join(a_at, config.clone());
    let mut b = Player::join(b_at, config);

    let dt = 1.0 / 60.0;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);
    let seen = loop {
        a.frame(dt);
        b.frame(dt);
        if let (Some(ia), Some(ib)) = (a.entity(), b.entity())
            && let (Some(a_sees_b), Some(b_sees_a)) = (a.shows(ib), b.shows(ia))
        {
            break (a_sees_b, b_sees_a);
        }
        assert!(
            Instant::now() < deadline,
            "no sight within 10 s: a {:?}, b {:?}",
            a.connection.status(),
            b.connection.status()
        );
        std::thread::sleep(Duration::from_secs_f32(dt));
    };

    eprintln!("both players see each other after {:?}", started.elapsed());
    // Let the interpolation settle on the standing players, then check where each sees the other.
    for _ in 0..60 {
        a.frame(dt);
        b.frame(dt);
        std::thread::sleep(Duration::from_secs_f32(dt));
    }
    let (ia, ib) = (a.entity().expect("a joined"), b.entity().expect("b joined"));
    assert_ne!(ia, ib);
    for (viewer, id, at) in [(&a, ib, b_at), (&b, ia, a_at)] {
        let (kind, asset, position) = viewer.shows(id).expect("still shown");
        assert_eq!(
            (kind, asset),
            (EntityKind::Ped, PED),
            "a player is shown as a ped with the table's ped model"
        );
        let off = (0..3).map(|k| (position[k] - at[k]).abs()).fold(0.0f32, f32::max);
        assert!(off < 0.05, "shown {position:?}, standing at {at:?}");
        assert!(
            viewer.shows(viewer.entity().expect("joined")).is_none(),
            "no one is shown their own player"
        );
    }
    assert_eq!(a.connection.status(), Status::Connected);
    assert_eq!((a.link.world_errors, b.link.world_errors), (0, 0));
    let _ = seen;
    drop((a, b));
    done.store(true, Ordering::Relaxed);
    server.join().expect("server thread");
}
