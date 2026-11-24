//! A UDP relay that delays every datagram by a fixed time and drops a share of them at random, independently in
//! each direction. Every transport goes through the same relay, so they see the same network.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::common::{any_addr, udp_socket};

struct Pending {
    at: Instant,
    order: u64,
    socket: Arc<UdpSocket>,
    to: SocketAddr,
    data: Vec<u8>,
}

impl PartialEq for Pending {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.order) == (o.at, o.order)
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Pending {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.at, self.order).cmp(&(o.at, o.order))
    }
}

#[derive(Default)]
struct Queue {
    heap: BinaryHeap<Reverse<Pending>>,
    order: u64,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    delay: Duration,
    loss: f64,
    rng: Mutex<u64>,
    dropped: Mutex<[u64; 2]>,
}

impl Shared {
    fn random(&self) -> f64 {
        let mut s = self.rng.lock().expect("rng");
        // xorshift64*
        *s ^= *s >> 12;
        *s ^= *s << 25;
        *s ^= *s >> 27;
        (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn push(&self, socket: &Arc<UdpSocket>, to: SocketAddr, data: &[u8], dir: usize) {
        if self.random() < self.loss {
            self.dropped.lock().expect("dropped")[dir] += 1;
            return;
        }
        let mut q = self.queue.lock().expect("queue");
        q.order += 1;
        let order = q.order;
        q.heap.push(Reverse(Pending {
            at: Instant::now() + self.delay,
            order,
            socket: Arc::clone(socket),
            to,
            data: data.to_vec(),
        }));
        self.ready.notify_one();
    }
}

pub fn run(listen: SocketAddr, target: SocketAddr, delay_ms: f64, loss_pct: f64, seed: u64) {
    let front = Arc::new(udp_socket(listen).expect("bind relay"));
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        ready: Condvar::new(),
        delay: Duration::from_secs_f64(delay_ms / 1000.0),
        loss: loss_pct / 100.0,
        rng: Mutex::new(seed | 1),
        dropped: Mutex::new([0; 2]),
    });
    println!("relay {listen} -> {target} delay_ms={delay_ms} loss_pct={loss_pct}");
    crate::common::flush();

    // Sender: waits for the earliest datagram's time and sends it.
    {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let mut q = shared.queue.lock().expect("queue");
            loop {
                let now = Instant::now();
                match q.heap.peek() {
                    Some(Reverse(p)) if p.at <= now => {
                        let Reverse(p) = q.heap.pop().expect("peeked");
                        drop(q);
                        let _ = p.socket.send_to(&p.data, p.to);
                        q = shared.queue.lock().expect("queue");
                    }
                    Some(Reverse(p)) => {
                        let wait = p.at - now;
                        q = shared.ready.wait_timeout(q, wait).expect("wait").0;
                    }
                    None => q = shared.ready.wait(q).expect("wait"),
                }
            }
        });
    }

    // Report drops now and then.
    {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(10));
                let d = *shared.dropped.lock().expect("dropped");
                println!("relay dropped to_server={} to_client={}", d[0], d[1]);
                crate::common::flush();
            }
        });
    }

    // Front: one upstream socket per client address, each with a thread reading the server's replies.
    let mut upstream: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let Ok((n, from)) = front.recv_from(&mut buf) else {
            continue;
        };
        let up = upstream.entry(from).or_insert_with(|| {
            let s = Arc::new(udp_socket(any_addr(target)).expect("bind upstream"));
            let reader = Arc::clone(&s);
            let front = Arc::clone(&front);
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                loop {
                    if let Ok((n, _)) = reader.recv_from(&mut buf) {
                        shared.push(&front, from, &buf[..n], 1);
                    }
                }
            });
            s
        });
        let up = Arc::clone(up);
        shared.push(&up, target, &buf[..n], 0);
    }
}
