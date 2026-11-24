//! renet over its netcode transport (`renet_netcode`): UDP with its own reliability and encryption. Channels are
//! renet's defaults: 0 unreliable, 2 reliable ordered (resend after 300 ms).
//!
//! One netcode server holds at most 1024 clients, so more connections take several servers on consecutive ports
//! (`SHARDS` environment variable, read by both sides).

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use renet::{ConnectionConfig, DefaultChannel, RenetClient, RenetServer, ServerEvent};
use renet_netcode::{
    ClientAuthentication, NetcodeClientTransport, NetcodeServerTransport, ServerAuthentication, ServerConfig,
};

use crate::common::{any_addr, shard_addr, shards, udp_socket};
use crate::poll::{PollClient, PollServer, Received};

const PROTOCOL_ID: u64 = 7;
const MAX_PER_SHARD: usize = 1024;

fn wall() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock")
}

fn channel(reliable: bool) -> DefaultChannel {
    if reliable {
        DefaultChannel::ReliableOrdered
    } else {
        DefaultChannel::Unreliable
    }
}

#[allow(missing_debug_implementations)]
pub struct Server {
    shards: Vec<(NetcodeServerTransport, RenetServer)>,
    /// Clones of the transports' sockets, only to wait for readiness.
    sockets: Vec<UdpSocket>,
    last: Instant,
}

impl Server {
    pub fn bind(addr: SocketAddr) -> Self {
        let mut list = Vec::new();
        let mut sockets = Vec::new();
        for i in 0..shards() {
            let a = SocketAddr::new(addr.ip(), addr.port() + i);
            let socket = udp_socket(a).expect("bind");
            sockets.push(socket.try_clone().expect("clone"));
            let config = ServerConfig {
                current_time: wall(),
                max_clients: MAX_PER_SHARD,
                protocol_id: PROTOCOL_ID,
                public_addresses: vec![a],
                authentication: ServerAuthentication::Unsecure,
            };
            let transport = NetcodeServerTransport::new(config, socket).expect("transport");
            list.push((transport, RenetServer::new(ConnectionConfig::default())));
        }
        Self {
            shards: list,
            sockets,
            last: Instant::now(),
        }
    }
}

fn conn(shard: usize, client: u64) -> u64 {
    (shard as u64) << 40 | client
}

impl PollServer for Server {
    fn sockets(&self) -> Vec<&UdpSocket> {
        self.sockets.iter().collect()
    }

    fn receive(&mut self) -> Vec<Received> {
        let dt = self.last.elapsed();
        self.last = Instant::now();
        let mut out = Vec::new();
        for (i, (transport, server)) in self.shards.iter_mut().enumerate() {
            server.update(dt);
            let _ = transport.update(dt, server);
            while let Some(event) = server.get_event() {
                if let ServerEvent::ClientDisconnected { client_id, reason } = event {
                    eprintln!("renet client {client_id} disconnected: {reason}");
                }
            }
            for client in server.clients_id() {
                for reliable in [false, true] {
                    while let Some(m) = server.receive_message(client, channel(reliable)) {
                        out.push((conn(i, client), reliable, m.to_vec()));
                    }
                }
            }
        }
        out
    }

    fn send(&mut self, conn: u64, reliable: bool, data: &[u8]) {
        let (shard, client) = ((conn >> 40) as usize, conn & ((1 << 40) - 1));
        self.shards[shard]
            .1
            .send_message(client, channel(reliable), data.to_vec());
    }

    fn connections(&self) -> Vec<u64> {
        let mut v = Vec::new();
        for (i, (_, server)) in self.shards.iter().enumerate() {
            v.extend(server.clients_id_iter().map(|c| conn(i, c)));
        }
        v
    }

    fn flush(&mut self) {
        for (transport, server) in &mut self.shards {
            transport.send_packets(server);
        }
    }
}

#[allow(missing_debug_implementations)]
pub struct Client {
    transport: NetcodeClientTransport,
    client: RenetClient,
    socket: UdpSocket,
    last: Instant,
}

impl PollClient for Client {
    fn connect(server: SocketAddr, id: u32) -> Self {
        let server = shard_addr(server, id);
        let socket = udp_socket(any_addr(server)).expect("bind");
        let clone = socket.try_clone().expect("clone");
        let auth = ClientAuthentication::Unsecure {
            protocol_id: PROTOCOL_ID,
            client_id: u64::from(id),
            server_addr: server,
            user_data: None,
        };
        let transport = NetcodeClientTransport::new(wall(), auth, socket).expect("transport");
        Self {
            transport,
            client: RenetClient::new(ConnectionConfig::default()),
            socket: clone,
            last: Instant::now(),
        }
    }

    fn socket(&self) -> Option<&UdpSocket> {
        Some(&self.socket)
    }

    fn receive(&mut self) -> Vec<(bool, Vec<u8>)> {
        let dt = self.last.elapsed();
        self.last = Instant::now();
        self.client.update(dt);
        let _ = self.transport.update(dt, &mut self.client);
        let mut out = Vec::new();
        for reliable in [false, true] {
            while let Some(m) = self.client.receive_message(channel(reliable)) {
                out.push((reliable, m.to_vec()));
            }
        }
        out
    }

    fn send(&mut self, reliable: bool, data: &[u8]) {
        self.client.send_message(channel(reliable), data.to_vec());
    }

    fn connected(&self) -> bool {
        self.client.is_connected()
    }

    fn flush(&mut self) {
        let _ = self.transport.send_packets(&mut self.client);
    }
}
