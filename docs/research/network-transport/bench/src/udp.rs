//! Plain UDP: no connection, no reliability, no encryption. The floor every other transport adds to.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};

use crate::common::{any_addr, udp_socket};
use crate::poll::{PollClient, PollServer, Received};

#[derive(Debug)]
pub struct Server {
    socket: UdpSocket,
    by_addr: HashMap<SocketAddr, u64>,
    addrs: Vec<SocketAddr>,
    buf: Vec<u8>,
}

impl Server {
    pub fn bind(addr: SocketAddr) -> Self {
        let socket = udp_socket(addr).expect("bind");
        socket.set_nonblocking(true).expect("nonblocking");
        Self {
            socket,
            by_addr: HashMap::new(),
            addrs: Vec::new(),
            buf: vec![0; 2048],
        }
    }
}

impl PollServer for Server {
    fn sockets(&self) -> Vec<&UdpSocket> {
        vec![&self.socket]
    }

    fn receive(&mut self) -> Vec<Received> {
        let mut out = Vec::new();
        loop {
            match self.socket.recv_from(&mut self.buf) {
                Ok((n, from)) => {
                    let next = self.addrs.len() as u64;
                    let conn = *self.by_addr.entry(from).or_insert(next);
                    if conn == next {
                        self.addrs.push(from);
                    }
                    out.push((conn, false, self.buf[..n].to_vec()));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return out,
                Err(_) => {}
            }
        }
    }

    fn send(&mut self, conn: u64, _reliable: bool, data: &[u8]) {
        let _ = self.socket.send_to(data, self.addrs[conn as usize]);
    }

    fn connections(&self) -> Vec<u64> {
        (0..self.addrs.len() as u64).collect()
    }

    fn flush(&mut self) {}
}

#[derive(Debug)]
pub struct Client {
    socket: UdpSocket,
    buf: Vec<u8>,
}

impl PollClient for Client {
    fn connect(server: SocketAddr, _id: u32) -> Self {
        let socket = udp_socket(any_addr(server)).expect("bind");
        socket.connect(server).expect("connect");
        socket.set_nonblocking(true).expect("nonblocking");
        Self {
            socket,
            buf: vec![0; 2048],
        }
    }

    fn socket(&self) -> Option<&UdpSocket> {
        Some(&self.socket)
    }

    fn receive(&mut self) -> Vec<(bool, Vec<u8>)> {
        let mut out = Vec::new();
        while let Ok(n) = self.socket.recv(&mut self.buf) {
            out.push((false, self.buf[..n].to_vec()));
        }
        out
    }

    fn send(&mut self, _reliable: bool, data: &[u8]) {
        let _ = self.socket.send(data);
    }

    fn connected(&self) -> bool {
        true
    }

    fn flush(&mut self) {}
}
