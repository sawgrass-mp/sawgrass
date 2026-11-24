//! Valve's GameNetworkingSockets through the `game-networking-sockets` bindings (built only with the `gns`
//! feature). Reliable and unreliable messages share the default lane; every send sets `NO_NAGLE`, so nothing waits
//! for GNS's 5 ms Nagle timer and the loops decide when to send, as with the other transports.
//!
//! GNS owns its sockets and receives on its own service thread, so there is no socket to wait on: in echo mode
//! the loops spin instead (see `poll.rs`), which favours GNS's latency slightly and is not used for CPU figures.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::net::UdpSocket;

use gns::sys::ESteamNetworkingConnectionState as State;
use gns::{GnsConnection, GnsGlobal, GnsSocket, IsClient, IsServer, SendFlags};

use crate::poll::{PollClient, PollServer, Received};

fn flags(reliable: bool) -> SendFlags {
    if reliable {
        SendFlags::RELIABLE | SendFlags::NO_NAGLE
    } else {
        SendFlags::UNRELIABLE | SendFlags::NO_NAGLE
    }
}

fn global() -> &'static GnsGlobal {
    GnsGlobal::get().expect("GameNetworkingSockets init")
}

#[allow(missing_debug_implementations)]
pub struct Server {
    socket: GnsSocket<IsServer>,
    /// Our index for each connection handle, and the handle for each index.
    index: HashMap<GnsConnection, u64>,
    handles: Vec<GnsConnection>,
    connected: BTreeSet<u64>,
}

impl Server {
    pub fn bind(addr: SocketAddr) -> Self {
        let socket = GnsSocket::new(global())
            .listen(addr.ip(), addr.port())
            .expect("listen");
        Self {
            socket,
            index: HashMap::new(),
            handles: Vec::new(),
            connected: BTreeSet::new(),
        }
    }

    fn id(&mut self, c: GnsConnection) -> u64 {
        let next = self.handles.len() as u64;
        let id = *self.index.entry(c).or_insert(next);
        if id == next {
            self.handles.push(c);
        }
        id
    }
}

impl PollServer for Server {
    fn sockets(&self) -> Vec<&UdpSocket> {
        Vec::new()
    }

    fn receive(&mut self) -> Vec<Received> {
        global().poll_callbacks();
        let events: Vec<_> = self.socket.receive_events().collect();
        for ev in events {
            let conn = ev.connection();
            match ev.info().state() {
                State::k_ESteamNetworkingConnectionState_Connecting => {
                    let _ = self.socket.accept(conn);
                }
                State::k_ESteamNetworkingConnectionState_Connected => {
                    let id = self.id(conn);
                    self.connected.insert(id);
                }
                State::k_ESteamNetworkingConnectionState_ClosedByPeer
                | State::k_ESteamNetworkingConnectionState_ProblemDetectedLocally => {
                    let id = self.id(conn);
                    self.connected.remove(&id);
                    let _ = self.socket.close_connection(conn, 0, None, false);
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        loop {
            let Ok(batch) = self.socket.receive_messages::<256>() else {
                break;
            };
            let before = out.len();
            for m in batch {
                let reliable = m.flags().contains(SendFlags::RELIABLE);
                let id = self.id(m.connection());
                out.push((id, reliable, m.payload().to_vec()));
            }
            if out.len() - before < 256 {
                break;
            }
        }
        out
    }

    fn send(&mut self, conn: u64, reliable: bool, data: &[u8]) {
        let conn = self.handles[conn as usize];
        let m = global()
            .utils()
            .allocate_message(conn, flags(reliable), data.to_vec());
        let _ = self.socket.send_message(m);
    }

    fn connections(&self) -> Vec<u64> {
        self.connected.iter().copied().collect()
    }

    fn flush(&mut self) {}
}

#[allow(missing_debug_implementations)]
pub struct Client {
    socket: GnsSocket<IsClient>,
    connected: bool,
}

impl PollClient for Client {
    fn connect(server: SocketAddr, _id: u32) -> Self {
        let socket = GnsSocket::new(global())
            .connect(server.ip(), server.port())
            .expect("connect");
        Self {
            socket,
            connected: false,
        }
    }

    fn socket(&self) -> Option<&UdpSocket> {
        None
    }

    fn receive(&mut self) -> Vec<(bool, Vec<u8>)> {
        global().poll_callbacks();
        for ev in self.socket.receive_events() {
            match ev.info().state() {
                State::k_ESteamNetworkingConnectionState_Connected => self.connected = true,
                State::k_ESteamNetworkingConnectionState_ClosedByPeer
                | State::k_ESteamNetworkingConnectionState_ProblemDetectedLocally => self.connected = false,
                _ => {}
            }
        }
        let mut out = Vec::new();
        if let Ok(batch) = self.socket.receive_messages::<64>() {
            for m in batch {
                out.push((m.flags().contains(SendFlags::RELIABLE), m.payload().to_vec()));
            }
        }
        out
    }

    fn send(&mut self, reliable: bool, data: &[u8]) {
        let conn = self.socket.connection();
        let m = global()
            .utils()
            .allocate_message(conn, flags(reliable), data.to_vec());
        let _ = self.socket.send_message(m);
    }

    fn connected(&self) -> bool {
        self.connected
    }

    fn flush(&mut self) {}
}
