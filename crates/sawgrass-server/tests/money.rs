//! Players pay each other through the ledger's single writer (task DB-8): every player gets a funded wallet, a
//! payment answers with the payer's balance once it is on disk, a retried key pays once, and the money is there
//! after the server's ledger is reopened.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sawgrass_gateway::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sawgrass_gateway::{Gateway, RootCertStore, Session, connect};
use sawgrass_protocol::game::{ClientEvent, PayResult, ServerEvent};
use sawgrass_protocol::{PROTOCOL_VERSION, PacketHeader, PacketKind};
use sawgrass_server::money::Money;
use sawgrass_server::net::{self, ListenConfig};
use sawgrass_server::{TickConfig, TickLoop};
use sawgrass_store::{Config, Kind, Operation, Outcome, RocksStore, Writer};

fn gateway() -> (Gateway, RootCertStore) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let mut roots = RootCertStore::empty();
    roots.add(der.clone()).expect("root");
    let gateway = Gateway::bind(([127, 0, 0, 1], 0).into(), vec![der], key).expect("bind");
    (gateway, roots)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sawgrass-money-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn next_event(session: &mut Session) -> ServerEvent {
    let (_, packet) = tokio::time::timeout(Duration::from_secs(5), session.recv_event())
        .await
        .expect("an event in time")
        .expect("event");
    ServerEvent::decode(&packet[PacketHeader::SIZE..]).expect("decodes")
}

async fn joined(session: &mut Session) -> u32 {
    match next_event(session).await {
        ServerEvent::Joined { entity, .. } => entity,
        other => panic!("expected Joined, got {other:?}"),
    }
}

async fn pay(session: &mut Session, key: u64, to: u32, amount: u32) -> (PayResult, i64) {
    let mut packet = vec![0; PacketHeader::SIZE];
    let header = PacketHeader {
        kind: PacketKind::Event,
        sequence: 0,
        tick: 0,
    };
    header.encode(&mut packet).expect("header");
    ClientEvent::Pay { key, to, amount }.encode(&mut packet);
    session.send_event(&packet).await.expect("send");
    match next_event(session).await {
        ServerEvent::Paid {
            key: answered,
            result,
            balance,
        } => {
            assert_eq!(answered, key);
            (result, balance)
        }
        other => panic!("expected Paid, got {other:?}"),
    }
}

/// Starts a server on `money` (or none), connects two clients and runs ticks until both joined.
fn two_players(
    runtime: &tokio::runtime::Runtime,
    money: Option<Arc<Money>>,
    money_events: bool,
) -> (Session, Session, u32, u32, TickLoop) {
    let (addr, roots, mut players) = {
        let _guard = runtime.enter();
        let (gateway, roots) = gateway();
        let addr = gateway.local_addr().expect("addr");
        let config = ListenConfig {
            money_events,
            ..ListenConfig::default()
        };
        (addr, roots, net::listen_with_money(gateway, config, money))
    };
    let mut tick_loop = TickLoop::new(TickConfig::default());
    tick_loop.add_system("net", move |world, info| players.run_tick(world, info));
    let (mut a, mut b) = runtime.block_on(async {
        // One after the other, so `a` is connection 1 and `b` connection 2.
        let a = connect(addr, "localhost", roots.clone(), PROTOCOL_VERSION).await;
        let b = connect(addr, "localhost", roots, PROTOCOL_VERSION).await;
        (a.expect("a connects"), b.expect("b connects"))
    });
    let (id_a, id_b) = std::thread::scope(|scope| {
        let ids = scope.spawn(|| runtime.block_on(async { (joined(&mut a).await, joined(&mut b).await) }));
        for _ in 0..500 {
            if ids.is_finished() {
                break;
            }
            tick_loop.run_tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        ids.join().expect("joined")
    });
    (a, b, id_a, id_b, tick_loop)
}

#[test]
fn players_pay_each_other_once_per_key_and_the_money_survives_a_reopen() {
    let dir = temp_dir("pay");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let writer = Writer::start(RocksStore::open(&dir).expect("store"), Config::default()).expect("writer");
    let money = Money::open(writer.handle(), 1_000, 1).expect("money");
    let (mut a, mut b, id_a, id_b, _tick_loop) = two_players(&runtime, Some(money.clone()), true);

    runtime.block_on(async {
        assert_eq!(pay(&mut a, 1, id_b, 300).await, (PayResult::Done, 700));
        // The same key again: answered as before, paid once.
        assert_eq!(pay(&mut a, 1, id_b, 300).await, (PayResult::Done, 700));
        assert_eq!(
            pay(&mut a, 2, id_b, 800).await,
            (PayResult::InsufficientFunds, 700)
        );
        assert_eq!(pay(&mut a, 3, 0xDEAD, 1).await, (PayResult::UnknownPayee, 700));
        assert_eq!(pay(&mut a, 4, id_a, 1).await, (PayResult::Invalid, 700));
        assert_eq!(pay(&mut a, 5, id_b, 0).await, (PayResult::Invalid, 700));
        // B's key 1 is its own, not A's.
        assert_eq!(pay(&mut b, 1, id_a, 50).await, (PayResult::Done, 1_250));
    });
    drop((a, b));
    drop(money);
    drop(runtime);
    writer.stop();

    // Reopened: the same keys name the same wallets, holding what they held.
    let writer = Writer::start(RocksStore::open(&dir).expect("store"), Config::default()).expect("writer");
    let ledger = writer.handle();
    let balance_of = |key: &str, kind| {
        let Ok(Outcome::Opened { wallet, .. }) = ledger
            .submit(Operation::OpenWallet {
                key: key.to_owned(),
                kind,
            })
            .wait()
        else {
            panic!("{key} reopens");
        };
        ledger.balance(wallet).wait().expect("balance")
    };
    assert_eq!(balance_of("player/1/1", Kind::Character), 750);
    assert_eq!(balance_of("player/1/2", Kind::Character), 1_250);
    assert_eq!(balance_of("treasury", Kind::System), -2_000);
    writer.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_retried_key_gets_its_first_answer_after_the_payee_has_left() {
    let dir = temp_dir("left");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let writer = Writer::start(RocksStore::open(&dir).expect("store"), Config::default()).expect("writer");
    let money = Money::open(writer.handle(), 1_000, 1).expect("money");
    let (mut a, b, _, id_b, mut tick_loop) = two_players(&runtime, Some(money.clone()), true);
    runtime.block_on(async {
        assert_eq!(pay(&mut a, 1, id_b, 300).await, (PayResult::Done, 700));
        // Refused: the ledger does not keep key 2, so after B leaves it is a new payment.
        assert_eq!(
            pay(&mut a, 2, id_b, 5_000).await,
            (PayResult::InsufficientFunds, 700)
        );
    });
    drop(b);
    for _ in 0..500 {
        if !money.knows_payee(id_b) {
            break;
        }
        tick_loop.run_tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!money.knows_payee(id_b), "B left");
    runtime.block_on(async {
        // The lost-answer retry: the first answer again, and no money moves twice.
        assert_eq!(pay(&mut a, 1, id_b, 300).await, (PayResult::Done, 700));
        // The same key with another amount is a reused key, as it is while the payee is connected.
        assert_eq!(pay(&mut a, 1, id_b, 301).await, (PayResult::Invalid, 700));
        // The refused key retried with an amount A has: a new payment to a player who left, so no money goes
        // into the ended connection's wallet.
        assert_eq!(pay(&mut a, 2, id_b, 500).await, (PayResult::UnknownPayee, 700));
        // A new key to the player who left.
        assert_eq!(pay(&mut a, 3, id_b, 10).await, (PayResult::UnknownPayee, 700));
    });
    drop((a, tick_loop));
    drop(runtime);
    drop(money);
    writer.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_server_without_money_events_refuses_payments() {
    let dir = temp_dir("refused");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let writer = Writer::start(RocksStore::open(&dir).expect("store"), Config::default()).expect("writer");
    let money = Money::open(writer.handle(), 1_000, 1).expect("money");
    let (mut a, _b, _, id_b, _tick_loop) = two_players(&runtime, Some(money), false);
    assert_eq!(
        runtime.block_on(pay(&mut a, 1, id_b, 10)),
        (PayResult::Refused, 0)
    );
    drop(runtime);
    writer.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
