//! The gateway server on compio: thread per core over io_uring, with `compio-quic`, which drives the same
//! `quinn-proto` state machine as quinn. One compio runtime per thread, each with its own endpoint on the shared
//! port (`SO_REUSEPORT`). Same handshake, framing, settings and snapshot as `gw.rs`.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use bytes::Bytes;
use compio::io::{AsyncReadExt, AsyncWriteExt};
use compio_quic::{Connection, Endpoint, EndpointConfig, RecvStream, SendStream, ServerConfig, VarInt};

use crate::common::*;
use crate::gw::{Counters, MAGIC, TickClock, VERSION};

async fn read_frame(recv: &mut RecvStream) -> Option<Vec<u8>> {
    let len = recv.read_exact([0u8; 4]).await;
    len.0.ok()?;
    let len = u32::from_le_bytes(len.1) as usize;
    if len > MAX_EVENT_LEN {
        return None;
    }
    let data = recv.read_exact(vec![0u8; len]).await;
    data.0.ok()?;
    Some(data.1)
}

async fn handshake(send: &mut SendStream, recv: &mut RecvStream) -> Option<()> {
    let hello = recv.read_exact([0u8; 8]).await;
    hello.0.ok()?;
    let hello = hello.1;
    if hello[..4] != MAGIC || u32::from_le_bytes(hello[4..].try_into().ok()?) != VERSION {
        return None;
    }
    send.write_all(VERSION.to_le_bytes()).await.0.ok()
}

async fn serve(endpoint: Endpoint, counters: Arc<Counters>) {
    let conns: Rc<RefCell<Vec<Connection>>> = Rc::default();
    {
        let (conns, counters) = (Rc::clone(&conns), Arc::clone(&counters));
        compio::runtime::spawn(async move {
            let mut clock = TickClock::new();
            let mut tick = 0u64;
            loop {
                compio::time::sleep(clock.wait()).await;
                tick += 1;
                let list = conns.borrow().clone();
                for (i, c) in list.iter().enumerate() {
                    let snapshot = Bytes::from(message(KIND_SNAPSHOT, i as u32, tick, SNAPSHOT_BYTES));
                    if c.send_datagram(snapshot).is_ok() {
                        counters.sent.fetch_add(1, Relaxed);
                    }
                }
            }
        })
        .detach();
    }
    while let Some(incoming) = endpoint.wait_incoming().await {
        let (conns, counters) = (Rc::clone(&conns), Arc::clone(&counters));
        compio::runtime::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                return;
            };
            if handshake(&mut send, &mut recv).await.is_none() {
                conn.close(VarInt::from_u32(1), b"bad hello");
                return;
            }
            conns.borrow_mut().push(conn.clone());
            counters.conns.fetch_add(1, Relaxed);
            {
                let counters = Arc::clone(&counters);
                compio::runtime::spawn(async move {
                    while let Some(m) = read_frame(&mut recv).await {
                        counters.received.fetch_add(1, Relaxed);
                        if send.write_all(frame(&m)).await.0.is_ok() {
                            counters.sent.fetch_add(1, Relaxed);
                        }
                    }
                })
                .detach();
            }
            while conn.recv_datagram().await.is_ok() {
                counters.received.fetch_add(1, Relaxed);
            }
            let id = conn.stable_id();
            conns.borrow_mut().retain(|c| c.stable_id() != id);
            counters.conns.fetch_sub(1, Relaxed);
        })
        .detach();
    }
}

/// Runs `threads` compio runtimes, one endpoint each, until killed.
pub fn server(addr: SocketAddr, threads: usize, config: ServerConfig, counters: Arc<Counters>) {
    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let (config, counters) = (config.clone(), Arc::clone(&counters));
            std::thread::spawn(move || {
                let rt = compio::runtime::Runtime::new().expect("compio runtime");
                if i == 0 {
                    println!("compio driver={:?}", rt.driver_type());
                    flush();
                }
                rt.block_on(async move {
                    let socket = udp_socket(addr, true).expect("bind");
                    let socket = compio::net::UdpSocket::from_std(socket).expect("socket");
                    let endpoint = Endpoint::new(socket, EndpointConfig::default(), Some(config), None)
                        .expect("endpoint");
                    serve(endpoint, counters).await;
                });
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
}
