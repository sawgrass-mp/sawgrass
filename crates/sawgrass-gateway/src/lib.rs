//! The gateway: terminates player connections over QUIC (`docs/architecture.md`, "Gateways").
//!
//! Each connection carries:
//!
//! - one bidirectional stream, opened by the client, for the handshake and then for reliable events;
//! - unreliable datagrams for state ([`PacketKind::Snapshot`] and [`PacketKind::ClientState`]).
//!
//! The handshake is the first thing on the event stream. The client sends a hello: the 4-byte [`MAGIC`] followed
//! by its protocol version (`u32`, little-endian). If [`is_compatible`] accepts that version the gateway answers
//! with its own version (`u32`, little-endian) and the session starts. Otherwise the gateway closes the
//! connection with error code [`VERSION_REFUSED`] and its own version as the 4-byte reason, so the client can
//! tell the player which side is out of date.
//!
//! After the handshake every event is framed as a `u32` little-endian length followed by that many bytes, at
//! most [`MAX_EVENT_LEN`]. Both events and datagrams start with a [`PacketHeader`]; a peer that sends one whose
//! header does not decode, or whose kind does not belong on that channel, gets an error back from the receive
//! call.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use quinn::rustls::{self, RootCertStore};
use quinn::{Connection, ConnectionError, Endpoint, RecvStream, SendStream, VarInt};
use sawgrass_protocol::{DecodeError, PROTOCOL_VERSION, PacketHeader, PacketKind, is_compatible};

/// TLS application protocol name for Sawgrass connections.
pub const ALPN: &[u8] = b"sawgrass";

/// First bytes of a client hello.
pub const MAGIC: [u8; 4] = *b"SWGR";

/// Application error code the gateway closes a connection with when it refuses the client's version.
pub const VERSION_REFUSED: VarInt = VarInt::from_u32(1);

/// Largest event, header included, either side accepts.
pub const MAX_EVENT_LEN: usize = 64 * 1024;

/// How long a peer may take to complete the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a connection or session operation failed.
#[derive(Debug)]
pub enum Error {
    /// The TLS configuration was rejected.
    Tls(rustls::Error),
    /// The local endpoint could not be created.
    Io(std::io::Error),
    /// The connection could not be started.
    Connect(quinn::ConnectError),
    /// The connection was lost or closed.
    Connection(ConnectionError),
    /// The peer did not finish the handshake within [`HANDSHAKE_TIMEOUT`].
    Timeout,
    /// The peer sent something other than a valid hello or welcome.
    BadHandshake,
    /// The gateway refused this client's protocol version; `gateway` is the version it speaks.
    VersionRefused {
        /// The version the gateway speaks.
        gateway: u32,
    },
    /// The peer's protocol version is not compatible; the gateway closed the connection.
    IncompatibleClient {
        /// The version the client asked for.
        client: u32,
    },
    /// A packet did not decode.
    Decode(DecodeError),
    /// A packet of this kind does not belong on the channel it arrived on.
    WrongChannel(PacketKind),
    /// An event is longer than [`MAX_EVENT_LEN`].
    EventTooLong(usize),
    /// A datagram could not be sent.
    SendDatagram(quinn::SendDatagramError),
    /// The peer closed its event stream.
    StreamClosed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tls(e) => write!(f, "TLS configuration: {e}"),
            Self::Io(e) => write!(f, "endpoint: {e}"),
            Self::Connect(e) => write!(f, "connect: {e}"),
            Self::Connection(e) => write!(f, "connection: {e}"),
            Self::Timeout => f.write_str("handshake timed out"),
            Self::BadHandshake => f.write_str("invalid handshake"),
            Self::VersionRefused { gateway } => write!(
                f,
                "gateway speaks protocol version {gateway}, this client speaks {PROTOCOL_VERSION}"
            ),
            Self::IncompatibleClient { client } => write!(
                f,
                "client speaks protocol version {client}, this gateway speaks {PROTOCOL_VERSION}"
            ),
            Self::Decode(e) => write!(f, "decode: {e}"),
            Self::WrongChannel(kind) => write!(f, "{kind:?} packet on the wrong channel"),
            Self::EventTooLong(len) => write!(f, "event of {len} bytes exceeds {MAX_EVENT_LEN}"),
            Self::SendDatagram(e) => write!(f, "send datagram: {e}"),
            Self::StreamClosed => f.write_str("event stream closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ConnectionError> for Error {
    fn from(e: ConnectionError) -> Self {
        match e {
            ConnectionError::ApplicationClosed(ref close) if close.error_code == VERSION_REFUSED => {
                match <[u8; 4]>::try_from(&close.reason[..]) {
                    Ok(v) => Self::VersionRefused {
                        gateway: u32::from_le_bytes(v),
                    },
                    Err(_) => Self::Connection(e),
                }
            }
            e => Self::Connection(e),
        }
    }
}

impl From<quinn::ReadExactError> for Error {
    fn from(e: quinn::ReadExactError) -> Self {
        match e {
            quinn::ReadExactError::FinishedEarly(_) => Self::StreamClosed,
            quinn::ReadExactError::ReadError(e) => e.into(),
        }
    }
}

impl From<quinn::ReadError> for Error {
    fn from(e: quinn::ReadError) -> Self {
        match e {
            quinn::ReadError::ConnectionLost(e) => e.into(),
            _ => Self::StreamClosed,
        }
    }
}

impl From<quinn::WriteError> for Error {
    fn from(e: quinn::WriteError) -> Self {
        match e {
            quinn::WriteError::ConnectionLost(e) => e.into(),
            _ => Self::StreamClosed,
        }
    }
}

fn tls_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A gateway listening for player connections.
#[derive(Debug)]
pub struct Gateway {
    endpoint: Endpoint,
}

impl Gateway {
    /// Listens on `addr` with the given certificate chain and private key.
    pub fn bind(
        addr: SocketAddr,
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self, Error> {
        let mut tls = rustls::ServerConfig::builder_with_provider(tls_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(Error::Tls)?
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .map_err(Error::Tls)?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = QuicServerConfig::try_from(tls).map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        let endpoint = Endpoint::server(config, addr).map_err(Error::Io)?;
        Ok(Self { endpoint })
    }

    /// The address the gateway listens on.
    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        self.endpoint.local_addr().map_err(Error::Io)
    }

    /// Waits for the next incoming connection; `None` once the gateway is closed.
    ///
    /// Run [`Incoming::handshake`] on its own task so one slow client does not hold up the others.
    pub async fn accept(&self) -> Option<Incoming> {
        self.endpoint.accept().await.map(Incoming)
    }

    /// Closes every connection and stops accepting new ones.
    pub fn close(&self) {
        self.endpoint.close(VarInt::from_u32(0), b"gateway closed");
    }
}

/// A connection attempt that has not completed the handshake yet.
#[derive(Debug)]
pub struct Incoming(quinn::Incoming);

impl Incoming {
    /// Completes the QUIC and Sawgrass handshakes, refusing an incompatible client version.
    pub async fn handshake(self) -> Result<Session, Error> {
        let result = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let connection = self.0.await?;
            let (mut send, mut recv) = connection.accept_bi().await?;
            let mut hello = [0u8; 8];
            recv.read_exact(&mut hello).await?;
            let (magic, version) = hello.split_at(4);
            if magic != MAGIC {
                connection.close(VarInt::from_u32(0), b"bad hello");
                return Err(Error::BadHandshake);
            }
            let client = u32::from_le_bytes([version[0], version[1], version[2], version[3]]);
            if !is_compatible(client) {
                connection.close(VERSION_REFUSED, &PROTOCOL_VERSION.to_le_bytes());
                return Err(Error::IncompatibleClient { client });
            }
            send.write_all(&PROTOCOL_VERSION.to_le_bytes()).await?;
            Ok(Session {
                connection,
                send,
                recv,
                peer_version: client,
            })
        })
        .await;
        result.unwrap_or(Err(Error::Timeout))
    }
}

/// Connects to the gateway at `addr`, trusting only `roots`, and performs the handshake announcing `version`.
///
/// Clients and bots pass [`PROTOCOL_VERSION`]; other values exist for tests of the refusal path.
pub async fn connect(
    addr: SocketAddr,
    server_name: &str,
    roots: RootCertStore,
    version: u32,
) -> Result<Session, Error> {
    let mut tls = rustls::ClientConfig::builder_with_provider(tls_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(Error::Tls)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = QuicClientConfig::try_from(tls).map_err(|e| Error::Io(std::io::Error::other(e)))?;
    let local: SocketAddr = if addr.is_ipv6() {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let mut endpoint = Endpoint::client(local).map_err(Error::Io)?;
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));

    let result = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let connection = endpoint
            .connect(addr, server_name)
            .map_err(Error::Connect)?
            .await?;
        let (mut send, mut recv) = connection.open_bi().await?;
        let mut hello = [0u8; 8];
        hello[..4].copy_from_slice(&MAGIC);
        hello[4..].copy_from_slice(&version.to_le_bytes());
        send.write_all(&hello).await?;
        let mut welcome = [0u8; 4];
        recv.read_exact(&mut welcome).await?;
        Ok(Session {
            connection,
            send,
            recv,
            peer_version: u32::from_le_bytes(welcome),
        })
    })
    .await;
    result.unwrap_or(Err(Error::Timeout))
}

/// An established connection on either side.
#[derive(Debug)]
pub struct Session {
    connection: Connection,
    send: SendStream,
    recv: RecvStream,
    peer_version: u32,
}

impl Session {
    /// The protocol version the peer announced in the handshake.
    #[must_use]
    pub fn peer_version(&self) -> u32 {
        self.peer_version
    }

    /// The peer's address.
    #[must_use]
    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// Sends a state packet as an unreliable datagram. It may be dropped or arrive out of order.
    pub fn send_datagram(&self, packet: Bytes) -> Result<(), Error> {
        check_kind(&packet, false)?;
        self.connection.send_datagram(packet).map_err(Error::SendDatagram)
    }

    /// Waits for the next datagram and returns its header with the whole packet.
    pub async fn recv_datagram(&self) -> Result<(PacketHeader, Bytes), Error> {
        let packet = self.connection.read_datagram().await?;
        let header = check_kind(&packet, false)?;
        Ok((header, packet))
    }

    /// Sends an event on the reliable stream. Events arrive in order.
    pub async fn send_event(&mut self, packet: &[u8]) -> Result<(), Error> {
        if packet.len() > MAX_EVENT_LEN {
            return Err(Error::EventTooLong(packet.len()));
        }
        check_kind(packet, true)?;
        let len = u32::try_from(packet.len()).map_err(|_| Error::EventTooLong(packet.len()))?;
        self.send.write_all(&len.to_le_bytes()).await?;
        self.send.write_all(packet).await?;
        Ok(())
    }

    /// Waits for the next event and returns its header with the whole packet.
    pub async fn recv_event(&mut self) -> Result<(PacketHeader, Vec<u8>), Error> {
        let mut len = [0u8; 4];
        self.recv.read_exact(&mut len).await?;
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_EVENT_LEN {
            self.connection.close(VarInt::from_u32(0), b"event too long");
            return Err(Error::EventTooLong(len));
        }
        let mut packet = vec![0u8; len];
        self.recv.read_exact(&mut packet).await?;
        let header = check_kind(&packet, true)?;
        Ok((header, packet))
    }

    /// Closes the connection.
    pub fn close(&self) {
        self.connection.close(VarInt::from_u32(0), b"closed");
    }
}

/// Decodes the header and checks that its kind belongs on the event stream (`event`) or in a datagram.
fn check_kind(packet: &[u8], event: bool) -> Result<PacketHeader, Error> {
    let (header, _) = PacketHeader::decode(packet).map_err(Error::Decode)?;
    if (header.kind == PacketKind::Event) != event {
        return Err(Error::WrongChannel(header.kind));
    }
    Ok(header)
}
