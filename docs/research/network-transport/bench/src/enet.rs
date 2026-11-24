//! ENet through `rusty_enet`, a pure Rust port of the C library. Channel 0 is reliable, channel 1 unreliable.

use std::collections::BTreeSet;
use std::net::{SocketAddr, UdpSocket};

use rusty_enet::{Event, Host, HostSettings, Packet, PeerID};

use crate::common::{any_addr, shard_addr, shards, udp_socket};
use crate::poll::{PollClient, PollServer, Received};

const RELIABLE: u8 = 0;
const UNRELIABLE: u8 = 1;

fn host(addr: SocketAddr, peers: usize) -> Host<UdpSocket> {
    let socket = udp_socket(addr).expect("bind");
    Host::new(
        socket,
        HostSettings {
            peer_limit: peers,
            channel_limit: 2,
            ..Default::default()
        },
    )
    .expect("host")
}

fn packet(reliable: bool, data: &[u8]) -> Packet {
    if reliable {
        Packet::reliable(data)
    } else {
        Packet::unreliable(data)
    }
}

#[allow(missing_debug_implementations)]
pub struct Server {
    hosts: Vec<Host<UdpSocket>>,
    connected: BTreeSet<u64>,
}

/// One ENet host holds at most 4095 peers (a 12-bit peer id on the wire).
const MAX_PER_HOST: usize = 4095;

fn conn(host: usize, peer: usize) -> u64 {
    (host as u64) << 40 | peer as u64
}

impl Server {
    pub fn bind(addr: SocketAddr) -> Self {
        let hosts = (0..shards())
            .map(|i| host(SocketAddr::new(addr.ip(), addr.port() + i), MAX_PER_HOST))
            .collect();
        Self {
            hosts,
            connected: BTreeSet::new(),
        }
    }
}

impl PollServer for Server {
    fn sockets(&self) -> Vec<&UdpSocket> {
        self.hosts.iter().map(Host::socket).collect()
    }

    fn receive(&mut self) -> Vec<Received> {
        let mut out = Vec::new();
        for (i, host) in self.hosts.iter_mut().enumerate() {
            while let Ok(Some(event)) = host.service() {
                match event {
                    Event::Connect { peer, .. } => {
                        self.connected.insert(conn(i, peer.id().0));
                    }
                    Event::Disconnect { peer, .. } => {
                        self.connected.remove(&conn(i, peer.id().0));
                    }
                    Event::Receive {
                        peer,
                        channel_id,
                        packet,
                    } => {
                        out.push((
                            conn(i, peer.id().0),
                            channel_id == RELIABLE,
                            packet.data().to_vec(),
                        ));
                    }
                }
            }
        }
        out
    }

    fn send(&mut self, conn: u64, reliable: bool, data: &[u8]) {
        let channel = if reliable { RELIABLE } else { UNRELIABLE };
        let (host, peer) = ((conn >> 40) as usize, (conn & ((1 << 40) - 1)) as usize);
        if let Some(peer) = self.hosts[host].get_peer_mut(PeerID(peer)) {
            let _ = peer.send(channel, &packet(reliable, data));
        }
    }

    fn connections(&self) -> Vec<u64> {
        self.connected.iter().copied().collect()
    }

    fn flush(&mut self) {
        for host in &mut self.hosts {
            host.flush();
        }
    }
}

#[allow(missing_debug_implementations)]
pub struct Client {
    host: Host<UdpSocket>,
    peer: PeerID,
    connected: bool,
}

impl PollClient for Client {
    fn connect(server: SocketAddr, id: u32) -> Self {
        let server = shard_addr(server, id);
        let mut host = host(any_addr(server), 1);
        let peer = host.connect(server, 2, id).expect("connect").id();
        Self {
            host,
            peer,
            connected: false,
        }
    }

    fn socket(&self) -> Option<&UdpSocket> {
        Some(self.host.socket())
    }

    fn receive(&mut self) -> Vec<(bool, Vec<u8>)> {
        let mut out = Vec::new();
        while let Ok(Some(event)) = self.host.service() {
            match event {
                Event::Connect { .. } => self.connected = true,
                Event::Disconnect { .. } => self.connected = false,
                Event::Receive {
                    channel_id, packet, ..
                } => {
                    out.push((channel_id == RELIABLE, packet.data().to_vec()));
                }
            }
        }
        out
    }

    fn send(&mut self, reliable: bool, data: &[u8]) {
        let channel = if reliable { RELIABLE } else { UNRELIABLE };
        let _ = self
            .host
            .peer_mut(self.peer)
            .send(channel, &packet(reliable, data));
    }

    fn connected(&self) -> bool {
        self.connected
    }

    fn flush(&mut self) {
        self.host.flush();
    }
}
