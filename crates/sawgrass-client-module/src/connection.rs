//! The module's connection to a server.
//!
//! The module opens its own QUIC connection through `sawgrass-gateway` (ADR 0004: host networking from our code,
//! not the title's). It runs on a thread of its own with its own runtime, because the game's threads are not ours
//! to block. It only moves bytes: datagrams from the server go into [`NetLink::inbox`], datagrams from
//! [`NetLink::outbox`] go to the server, and server events are decoded into [`NetLink::events`]. Everything else
//! happens on the game's frame, in [`NetLink`].

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use sawgrass_gateway::{RootCertStore, connect};
use sawgrass_protocol::game::ServerEvent;
use sawgrass_protocol::{PROTOCOL_VERSION, PacketHeader};

use crate::link::{Datagrams, NetLink, ServerEvents};

/// How often queued datagrams are sent. Well under a frame, so a state is not held back.
const SEND_PERIOD: Duration = Duration::from_millis(2);

const CLOSED: &str = "closed by the module";

/// Resolves once `stop` is set, checking it every [`SEND_PERIOD`].
async fn stopped(stop: &AtomicBool) {
    let mut interval = tokio::time::interval(SEND_PERIOD);
    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
    }
}

/// Where to connect.
#[derive(Debug, Clone)]
pub struct ConnectConfig {
    pub server: SocketAddr,
    /// The name in the gateway's certificate.
    pub server_name: String,
    /// The certificates the gateway's must chain to. The launcher provides them; no system store is read.
    pub roots: RootCertStore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Connected,
    /// The connection failed or ended, with the reason to show the player.
    Closed(String),
}

/// A running connection. Dropping it closes the connection and waits for its thread.
#[derive(Debug)]
pub struct Connection {
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Connection {
    /// Connects to `config.server` for `link`, on a new thread.
    pub fn start(config: ConnectConfig, link: &NetLink) -> std::io::Result<Self> {
        let status = Arc::new(Mutex::new(Status::Connecting));
        let stop = Arc::new(AtomicBool::new(false));
        let queues = (link.inbox(), link.outbox(), link.events());
        let (thread_status, thread_stop) = (status.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("sawgrass-connection".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(e) => return set(&thread_status, Status::Closed(e.to_string())),
                };
                let reason = runtime.block_on(run(config, queues, &thread_status, &thread_stop));
                set(&thread_status, Status::Closed(reason));
            })?;
        Ok(Self {
            status,
            stop,
            thread: Some(thread),
        })
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn set(status: &Mutex<Status>, value: Status) {
    *status.lock().unwrap_or_else(PoisonError::into_inner) = value;
}

/// Connects and moves bytes until the connection ends or `stop` is set. Returns why it ended.
async fn run(
    config: ConnectConfig,
    (inbox, outbox, events): (Datagrams, Datagrams, ServerEvents),
    status: &Mutex<Status>,
    stop: &AtomicBool,
) -> String {
    // The handshake can wait up to the gateway's idle timeout for a server that does not answer, so it races the
    // stop flag: dropping the connection while it connects must not hold the game's thread.
    let connecting = connect(config.server, &config.server_name, config.roots, PROTOCOL_VERSION);
    let session = tokio::select! {
        result = connecting => match result {
            Ok(session) => session,
            Err(e) => return format!("could not connect: {e}"),
        },
        () = stopped(stop) => return CLOSED.to_owned(),
    };
    set(status, Status::Connected);
    // The event sender is kept so the stream stays open; the module sends no events yet.
    let (datagrams, _event_sender, mut event_receiver) = session.split();

    let receiver = datagrams.clone();
    let mut datagrams_in = tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok((_, packet)) => inbox.push(packet.to_vec()),
                Err(e) => return e.to_string(),
            }
        }
    });
    let mut events_in = tokio::spawn(async move {
        loop {
            match event_receiver.recv().await {
                Ok((_, packet)) => {
                    // An event this build cannot read is skipped: a newer server may send more kinds.
                    let payload = packet.get(PacketHeader::SIZE..).unwrap_or_default();
                    if let Ok(event) = ServerEvent::decode(payload) {
                        events.push(event);
                    }
                }
                Err(e) => return e.to_string(),
            }
        }
    });

    let mut interval = tokio::time::interval(SEND_PERIOD);
    let reason = loop {
        tokio::select! {
            ended = &mut datagrams_in => break ended.unwrap_or_else(|e| e.to_string()),
            ended = &mut events_in => break ended.unwrap_or_else(|e| e.to_string()),
            _ = interval.tick() => {
                if stop.load(Ordering::Relaxed) {
                    break CLOSED.to_owned();
                }
                for datagram in outbox.take() {
                    if let Err(e) = datagrams.send(Bytes::from(datagram)) {
                        datagrams.close();
                        return e.to_string();
                    }
                }
            }
        }
    };
    datagrams.close();
    datagrams_in.abort();
    events_in.abort();
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use sawgrass_netcode::ClientConfig;
    use std::time::Instant;

    #[test]
    fn dropping_while_connecting_returns_at_once() {
        // A UDP port nobody answers on: the handshake would wait for the idle timeout.
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").expect("socket");
        let config = ConnectConfig {
            server: silent.local_addr().expect("addr"),
            server_name: "localhost".into(),
            roots: RootCertStore::empty(),
        };
        let link = NetLink::joining(ClientConfig::default());
        let connection = Connection::start(config, &link).expect("thread");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(connection.status(), Status::Connecting);
        let started = Instant::now();
        drop(connection);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "drop took {:?}",
            started.elapsed()
        );
    }
}
