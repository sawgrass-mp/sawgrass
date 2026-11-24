//! A client connects to a gateway over loopback, exchanges a datagram and an event, and an incompatible
//! version is refused.

use bytes::Bytes;
use quinn::rustls::RootCertStore;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sawgrass_gateway::{Error, Gateway, Session, connect};
use sawgrass_protocol::{PROTOCOL_VERSION, PacketHeader, PacketKind};

fn gateway() -> (Gateway, RootCertStore) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let mut roots = RootCertStore::empty();
    roots.add(der.clone()).expect("root");
    let gateway = Gateway::bind(([127, 0, 0, 1], 0).into(), vec![der], key).expect("bind");
    (gateway, roots)
}

fn packet(kind: PacketKind, tick: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; PacketHeader::SIZE];
    let header = PacketHeader {
        kind,
        sequence: 7,
        tick,
    };
    header.encode(&mut out).expect("encode");
    out.extend_from_slice(payload);
    out
}

async fn accept_one(gateway: &Gateway) -> Result<Session, Error> {
    gateway.accept().await.expect("incoming").handshake().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_exchanges_datagram_and_event() {
    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    let (server, client) = tokio::join!(
        accept_one(&gateway),
        connect(addr, "localhost", roots, PROTOCOL_VERSION)
    );
    let mut server = server.expect("server handshake");
    let mut client = client.expect("client handshake");
    assert_eq!(server.peer_version(), PROTOCOL_VERSION);
    assert_eq!(client.peer_version(), PROTOCOL_VERSION);

    // Unreliable state, client to gateway. Loopback does not drop it.
    let state = packet(PacketKind::ClientState, 41, b"state");
    client
        .send_datagram(Bytes::from(state.clone()))
        .expect("send datagram");
    let (header, received) = server.recv_datagram().await.expect("recv datagram");
    assert_eq!(header.kind, PacketKind::ClientState);
    assert_eq!(header.tick, 41);
    assert_eq!(received, state);

    // Reliable events in both directions.
    let event = packet(PacketKind::Event, 42, b"chat: hello");
    server.send_event(&event).await.expect("send event");
    let (header, received) = client.recv_event().await.expect("recv event");
    assert_eq!(header.kind, PacketKind::Event);
    assert_eq!(received, event);

    let reply = packet(PacketKind::Event, 43, &[0xAB; 2000]);
    client.send_event(&reply).await.expect("send reply");
    assert_eq!(server.recv_event().await.expect("recv reply").1, reply);

    client.close();
    gateway.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_version_is_refused() {
    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    let old = PROTOCOL_VERSION + 1;
    let (server, client) = tokio::join!(accept_one(&gateway), connect(addr, "localhost", roots, old));
    match server {
        Err(Error::IncompatibleClient { client }) => assert_eq!(client, old),
        other => panic!("gateway accepted version {old}: {other:?}"),
    }
    match client {
        Err(Error::VersionRefused { gateway }) => assert_eq!(gateway, PROTOCOL_VERSION),
        other => panic!("client was not refused: {other:?}"),
    }
    gateway.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packets_on_the_wrong_channel_are_rejected() {
    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    let (server, client) = tokio::join!(
        accept_one(&gateway),
        connect(addr, "localhost", roots, PROTOCOL_VERSION)
    );
    let _server = server.expect("server handshake");
    let mut client = client.expect("client handshake");

    let event = packet(PacketKind::Event, 1, b"");
    assert!(matches!(
        client.send_datagram(Bytes::from(event)),
        Err(Error::WrongChannel(PacketKind::Event))
    ));
    let state = packet(PacketKind::Snapshot, 1, b"");
    assert!(matches!(
        client.send_event(&state).await,
        Err(Error::WrongChannel(PacketKind::Snapshot))
    ));
    assert!(matches!(
        client
            .send_event(&vec![3u8; sawgrass_gateway::MAX_EVENT_LEN + 1])
            .await,
        Err(Error::EventTooLong(_))
    ));
    gateway.close();
}

/// A bare quinn connection to the gateway, without the Sawgrass handshake, to act as a misbehaving client.
async fn raw_client(addr: std::net::SocketAddr, roots: RootCertStore) -> quinn::Connection {
    use quinn::rustls;
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![sawgrass_gateway::ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC TLS");
    let mut endpoint = quinn::Endpoint::client(([127, 0, 0, 1], 0).into()).expect("endpoint");
    endpoint.set_default_client_config(quinn::ClientConfig::new(std::sync::Arc::new(crypto)));
    endpoint
        .connect(addr, "localhost")
        .expect("connect")
        .await
        .expect("QUIC handshake")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_grants_one_stream_and_no_more() {
    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    let (server, connection) = tokio::join!(accept_one(&gateway), async {
        let connection = raw_client(addr, roots).await;
        let (mut send, mut recv) = connection.open_bi().await.expect("event stream");
        let mut hello = sawgrass_gateway::MAGIC.to_vec();
        hello.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        send.write_all(&hello).await.expect("hello");
        let mut welcome = [0u8; 4];
        recv.read_exact(&mut welcome).await.expect("welcome");
        connection
    });
    let _server = server.expect("server handshake");

    // The gateway grants no credit for a second bidirectional stream or any unidirectional one, so opening them
    // waits forever instead of making the gateway buffer data nobody reads.
    let wait = std::time::Duration::from_millis(300);
    assert!(tokio::time::timeout(wait, connection.open_bi()).await.is_err());
    assert!(tokio::time::timeout(wait, connection.open_uni()).await.is_err());
    gateway.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_magic_is_refused() {
    let (gateway, roots) = gateway();
    let addr = gateway.local_addr().expect("addr");
    let (server, ()) = tokio::join!(accept_one(&gateway), async {
        let connection = raw_client(addr, roots).await;
        let (mut send, _recv) = connection.open_bi().await.expect("stream");
        send.write_all(b"HTTP/1.1").await.expect("hello");
        // Keep the connection open until the gateway has judged the hello.
        let _ = connection.closed().await;
    });
    assert!(matches!(server, Err(Error::BadHandshake)), "{server:?}");
    gateway.close();
}
