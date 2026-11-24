//! Compression benchmark for NET-5. See the README one directory up for what it measures and how to run it.
//!
//! Commands (each prints one JSON object per line):
//!   sizes          bytes and datagrams per snapshot for every encoding and compressor, and for small messages
//!   loss           delta-encoded snapshot size by round trip and loss
//!   speed          time to compress and decompress a snapshot
//!   fuzz METHOD N  decode N random and N corrupted packets with METHOD (e.g. delta/zstd-3-dict16k)
//!   recorded TRAIN TEST sizes|loss|speed
//!                  the same suite on snapshots recorded from the real server under the load-test bots:
//!                  dictionaries trained on the TRAIN recording, everything measured on the TEST recording

mod codec;
mod delta;
mod model;
mod recorded;
mod sim;

use std::collections::VecDeque;
use std::hint::black_box;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use codec::{Codec, Lz4, Plain, Zstd};
use model::*;
use sim::{Observer, Rng, World};

/// The simulated area: 800 entities on 600 x 600 m, so a player's 100 nearest are within about 120 m.
const ENTITIES: usize = 800;
const AREA_M: f64 = 600.0;
/// Players whose snapshots are recorded.
const OBSERVERS: usize = 32;
/// Ticks simulated before recording, so the visible sets and the acknowledgements have settled.
const WARM_UP: u32 = 200;
/// A baseline older than this many ticks (1.6 s) is not kept; the snapshot is then sent whole.
const MAX_BASELINE_AGE: u32 = 32;
/// Largest datagram quinn 0.11 sends once path MTU discovery reaches its default ceiling (1,452-byte UDP
/// payload), measured with `transport-bench datagram-size`; and with the 1,200-byte minimum every QUIC path
/// supports, the same 38 bytes of QUIC overhead less.
const DATAGRAM: usize = 1414;
const DATAGRAM_MIN: usize = 1162;
/// A snapshot split over several datagrams needs a fragment header (sequence, index, count) in each.
const FRAGMENT_HEADER: usize = 3;

/// Datagrams needed for a packet of `n` bytes.
fn datagrams(n: usize, payload: usize) -> usize {
    if n <= payload {
        1
    } else {
        n.div_ceil(payload - FRAGMENT_HEADER)
    }
}

/// One player's link: which snapshots arrive, and when the server learns which one the client has.
///
/// Each client state the client sends (20 a second, half a tick after each snapshot) carries the tick of the
/// newest snapshot it has received, so a lost acknowledgement is replaced by the next one. Snapshots and client
/// states are each lost independently with probability `loss`, and arrive half a round trip after they are sent.
struct Link {
    rng: Rng,
    rtt_ms: f64,
    loss: f64,
    history: VecDeque<Snapshot>,
    in_flight: VecDeque<(f64, u32)>,
    acks: VecDeque<(f64, u32)>,
    client_latest: Option<u32>,
    known: Option<u32>,
}

impl Link {
    fn new(seed: u64, rtt_ms: f64, loss: f64) -> Self {
        Self {
            rng: Rng(seed | 1),
            rtt_ms,
            loss,
            history: VecDeque::new(),
            in_flight: VecDeque::new(),
            acks: VecDeque::new(),
            client_latest: None,
            known: None,
        }
    }

    /// The newest snapshot the server knows the client has, if it is recent enough to encode against.
    fn baseline(&mut self, tick: u32, now: f64) -> Option<&Snapshot> {
        while self.acks.front().is_some_and(|&(at, _)| at <= now) {
            let (_, t) = self.acks.pop_front().expect("front");
            self.known = Some(self.known.map_or(t, |k| k.max(t)));
        }
        let known = self.known?;
        if tick - known > MAX_BASELINE_AGE {
            return None;
        }
        self.history.iter().find(|s| s.tick == known)
    }

    /// Records a snapshot sent at `now`, and runs the client until it sends its next state.
    fn sent(&mut self, snap: Snapshot, now: f64) {
        if !self.rng.chance(self.loss) {
            self.in_flight.push_back((now + self.rtt_ms / 2.0, snap.tick));
        }
        self.history.push_back(snap);
        while self.history.len() > MAX_BASELINE_AGE as usize + 1 {
            self.history.pop_front();
        }
        let send_at = now + sim::DT * 1000.0 / 2.0;
        while self.in_flight.front().is_some_and(|&(at, _)| at <= send_at) {
            let (_, t) = self.in_flight.pop_front().expect("front");
            self.client_latest = Some(self.client_latest.map_or(t, |c| c.max(t)));
        }
        if let Some(t) = self.client_latest
            && !self.rng.chance(self.loss)
        {
            self.acks.push_back((send_at + self.rtt_ms / 2.0, t));
        }
    }
}

/// Recordings from the real server (training, test), when the `recorded` command set them.
static RECORDED: OnceLock<(recorded::Streams, recorded::Streams)> = OnceLock::new();

/// Runs the world for `ticks` after the warm-up and calls `f` with every recorded snapshot, its baseline, and the
/// player's own entity. With recordings loaded, replays the training recording for `TRAIN_SEED` and the test
/// recording otherwise, at most `ticks` snapshots per bot, with no own entity (a default one is passed).
fn simulate(
    seed: u64,
    ticks: u32,
    rtt_ms: f64,
    loss: f64,
    mut f: impl FnMut(&Snapshot, Option<&Snapshot>, &Entity),
) {
    if let Some((train, test)) = RECORDED.get() {
        let streams = if seed == TRAIN_SEED { train } else { test };
        let mut links: Vec<Link> = (0..streams.len())
            .map(|i| Link::new(seed * 1000 + i as u64, rtt_ms, loss))
            .collect();
        let me = Entity {
            id: u32::MAX,
            pos: [0; 3],
            vel: [0; 3],
            rot: 0,
        };
        // Tick by tick across the bots, as the simulation goes, so `sample` takes every bot's snapshots.
        for i in 0..ticks as usize {
            for (stream, link) in streams.iter().zip(&mut links) {
                let Some(snap) = stream.get(i) else { continue };
                let now = f64::from(snap.tick) * sim::DT * 1000.0;
                let base = link.baseline(snap.tick, now);
                f(snap, base, &me);
                link.sent(snap.clone(), now);
            }
        }
        return;
    }
    let mut world = World::new(seed, ENTITIES, AREA_M);
    let mut observers: Vec<Observer> = (0..OBSERVERS).map(Observer::new).collect();
    let mut links: Vec<Link> = (0..OBSERVERS)
        .map(|i| Link::new(seed * 1000 + i as u64, rtt_ms, loss))
        .collect();
    assert!(world.len() > OBSERVERS);
    for _ in 0..WARM_UP + ticks {
        world.step();
        let all = world.entities();
        let tick = world.tick;
        let now = f64::from(tick) * sim::DT * 1000.0;
        for (o, link) in observers.iter_mut().zip(&mut links) {
            let snap = o.snapshot(tick, &all);
            if tick > WARM_UP {
                let base = link.baseline(tick, now);
                f(&snap, base, &all[o.me]);
            } else {
                link.baseline(tick, now);
            }
            link.sent(snap, now);
        }
    }
}

/// How a snapshot is put into bytes before compression.
#[derive(Clone, Copy, PartialEq)]
enum Encoding {
    /// The whole snapshot with postcard, as NET-4 chose.
    Full,
    /// Against the acknowledged baseline.
    Delta,
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Encoding::Full => "full",
            Encoding::Delta => "delta",
        }
    }
}

/// Turns snapshots into bytes and back, either way.
#[derive(Default)]
struct Encoder {
    delta: delta::Encoder,
    buf: Vec<u8>,
}

impl Encoder {
    fn encode(&mut self, enc: Encoding, cur: &Snapshot, base: Option<&Snapshot>) -> &[u8] {
        match enc {
            Encoding::Full => model::encode_snapshot(cur, &mut self.buf),
            Encoding::Delta => self.delta.encode(cur, base, &mut self.buf),
        }
        &self.buf
    }
}

fn decode(enc: Encoding, bytes: &[u8], base: Option<&Snapshot>) -> Option<Snapshot> {
    match enc {
        Encoding::Full => match model::decode(bytes)? {
            Packet::Snapshot(s) => Some(s),
            _ => None,
        },
        Encoding::Delta => delta::decode(bytes, base),
    }
}

/// Training samples for the dictionaries come from a different world (another seed) than the measurements.
const TRAIN_SEED: u64 = 2;
const TEST_SEED: u64 = 1;
const DICT_BYTES: usize = 16 * 1024;

fn train(enc: Encoding, bytes: usize) -> Vec<u8> {
    let mut samples = Vec::new();
    let mut e = Encoder::default();
    simulate(TRAIN_SEED, 100, 100.0, 0.0, |s, b, _| {
        samples.push(e.encode(enc, s, b).to_vec());
    });
    zstd::dict::from_samples(&samples, bytes).expect("train")
}

/// The compressors measured for an encoding, with dictionaries trained on that encoding's packets.
fn codecs(enc: Encoding, sweep: bool) -> Vec<Box<dyn Codec>> {
    let dict = train(enc, DICT_BYTES);
    let mut v: Vec<Box<dyn Codec>> = vec![
        Box::new(Plain::default()),
        Box::new(Lz4::new()),
        Box::new(Zstd::new(1, None)),
        Box::new(Zstd::new(3, None)),
        Box::new(Zstd::new(1, Some(&dict))),
        Box::new(Zstd::new(3, Some(&dict))),
    ];
    if sweep {
        for kb in [4, 64] {
            v.push(Box::new(Zstd::new(3, Some(&train(enc, kb * 1024)))));
        }
    }
    v
}

#[derive(Default)]
struct Sizes {
    bytes: Vec<usize>,
    datagrams: usize,
    datagrams_min: usize,
    split: usize,
}

impl Sizes {
    fn add(&mut self, n: usize) {
        self.bytes.push(n);
        self.datagrams += datagrams(n, DATAGRAM);
        self.datagrams_min += datagrams(n, DATAGRAM_MIN);
        self.split += usize::from(n > DATAGRAM);
    }

    fn json(&mut self) -> String {
        self.bytes.sort_unstable();
        let n = self.bytes.len().max(1);
        let pct = |p: f64| self.bytes[((n - 1) as f64 * p) as usize];
        format!(
            "\"snapshots\":{},\"mean_bytes\":{:.1},\"p50_bytes\":{},\"p99_bytes\":{},\"max_bytes\":{},\"mean_datagrams\":{:.3},\"mean_datagrams_mtu1200\":{:.3},\"split_pct\":{:.2}",
            self.bytes.len(),
            self.bytes.iter().sum::<usize>() as f64 / n as f64,
            pct(0.5),
            pct(0.99),
            self.bytes[n - 1],
            self.datagrams as f64 / n as f64,
            self.datagrams_min as f64 / n as f64,
            100.0 * self.split as f64 / n as f64,
        )
    }
}

/// Seconds of play recorded for the size suite.
const SIZE_TICKS: u32 = 1200;

fn sizes() {
    for enc in [Encoding::Full, Encoding::Delta] {
        let mut cs = codecs(enc, true);
        let mut stats: Vec<Sizes> = cs.iter().map(|_| Sizes::default()).collect();
        let mut e = Encoder::default();
        let (mut entities, mut changed, mut added, mut fulls, mut ages) =
            (0usize, 0usize, 0usize, 0usize, 0u64);
        simulate(TEST_SEED, SIZE_TICKS, 100.0, 0.0, |s, b, _| {
            entities += s.entities.len();
            match b {
                Some(b) => {
                    ages += u64::from(s.tick - b.tick);
                    added += s
                        .entities
                        .iter()
                        .filter(|x| b.entities.binary_search_by_key(&x.id, |y| y.id).is_err())
                        .count();
                    changed += s
                        .entities
                        .iter()
                        .filter(|x| b.entities.iter().any(|y| y.id == x.id && y != *x))
                        .count();
                }
                None => fulls += 1,
            }
            let bytes = e.encode(enc, s, b).to_vec();
            for (c, st) in cs.iter_mut().zip(&mut stats) {
                let packed = c.compress(&bytes).to_vec();
                st.add(packed.len());
                let back = c.decompress(&packed).expect("round trip");
                assert_eq!(
                    decode(enc, back, b).as_ref(),
                    Some(s),
                    "{} {} round trip",
                    enc.name(),
                    c.name()
                );
            }
        });
        let n = stats[0].bytes.len().max(1) as f64;
        println!(
            "{{\"suite\":\"stream\",\"encoding\":\"{}\",\"mean_entities\":{:.1},\"mean_changed\":{:.1},\"mean_added\":{:.2},\"no_baseline\":{},\"mean_baseline_age_ticks\":{:.2}}}",
            enc.name(),
            entities as f64 / n,
            changed as f64 / n,
            added as f64 / n,
            fulls,
            ages as f64 / (n - fulls as f64).max(1.0),
        );
        for (c, st) in cs.iter().zip(&mut stats) {
            println!(
                "{{\"suite\":\"size\",\"encoding\":\"{}\",\"codec\":\"{}\",{}}}",
                enc.name(),
                c.name(),
                st.json()
            );
        }
    }
    if RECORDED.get().is_none() {
        small_messages();
    }
}

/// Client state, a chat line and a script event: reliable or upstream messages that are not snapshots.
fn small_messages() {
    let mut me = None;
    simulate(TEST_SEED, 1, 100.0, 0.0, |_, _, m| me = Some(*m));
    let messages = [
        (
            "client_state",
            Packet::ClientState(ClientState {
                seq: 4712,
                tick: 1_234_566,
                me: me.expect("one tick"),
                buttons: 0b1000_0000_0101,
                aim: [-3120, 845],
            }),
        ),
        (
            "chat",
            Packet::Chat(Chat {
                seq: 17,
                tick: 1_234_570,
                from: 1042,
                channel: 2,
                text: "Meet me at the pier in five minutes, bring the van.".into(),
            }),
        ),
        (
            "script_event",
            Packet::ScriptEvent(ScriptEvent {
                seq: 18,
                tick: 1_234_571,
                name: "inventory:use".into(),
                args: vec![
                    Arg::Int(1042),
                    Arg::Int(3),
                    Arg::Str("medkit".into()),
                    Arg::Bool(true),
                ],
            }),
        ),
    ];
    let mut cs = codecs(Encoding::Full, false);
    let mut buf = Vec::new();
    for (name, p) in &messages {
        model::encode(p, &mut buf);
        for c in &mut cs {
            let n = c.compress(&buf).len();
            println!(
                "{{\"suite\":\"message\",\"message\":\"{name}\",\"codec\":\"{}\",\"bytes\":{n}}}",
                c.name()
            );
        }
    }
}

/// Delta size by round trip and loss: an older baseline means more has changed since.
fn loss() {
    let dict = train(Encoding::Delta, DICT_BYTES);
    for rtt in [40.0, 100.0, 200.0] {
        for loss in [0.0, 0.01, 0.05, 0.10] {
            let mut cs: Vec<Box<dyn Codec>> =
                vec![Box::new(Plain::default()), Box::new(Zstd::new(3, Some(&dict)))];
            let mut stats: Vec<Sizes> = cs.iter().map(|_| Sizes::default()).collect();
            let mut e = Encoder::default();
            let (mut fulls, mut ages, mut n) = (0usize, 0u64, 0usize);
            simulate(TEST_SEED, 600, rtt, loss, |s, b, _| {
                n += 1;
                match b {
                    Some(b) => ages += u64::from(s.tick - b.tick),
                    None => fulls += 1,
                }
                let bytes = e.encode(Encoding::Delta, s, b).to_vec();
                for (c, st) in cs.iter_mut().zip(&mut stats) {
                    st.add(c.compress(&bytes).len());
                }
            });
            for (c, st) in cs.iter().zip(&mut stats) {
                println!(
                    "{{\"suite\":\"loss\",\"rtt_ms\":{rtt},\"loss_pct\":{},\"codec\":\"{}\",\"no_baseline_pct\":{:.2},\"mean_baseline_age_ticks\":{:.2},{}}}",
                    loss * 100.0,
                    c.name(),
                    100.0 * fulls as f64 / n as f64,
                    ages as f64 / (n - fulls).max(1) as f64,
                    st.json()
                );
            }
        }
    }
}

/// Median nanoseconds per call over five rounds of at least 200 ms each.
fn time(mut op: impl FnMut()) -> f64 {
    for _ in 0..200 {
        op();
    }
    let mut rounds = Vec::new();
    for _ in 0..5 {
        let mut n: u64 = 0;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            for _ in 0..50 {
                op();
            }
            n += 50;
        }
        rounds.push(start.elapsed().as_nanos() as f64 / n as f64);
    }
    rounds.sort_by(f64::total_cmp);
    rounds[2]
}

/// A run of consecutive snapshots of every observer, with their baselines, to time encoders on varied data.
fn sample(count: usize) -> Vec<(Snapshot, Option<Snapshot>)> {
    let mut v = Vec::new();
    simulate(
        TEST_SEED,
        (count / OBSERVERS) as u32 + 1,
        100.0,
        0.0,
        |s, b, _| {
            if v.len() < count {
                v.push((s.clone(), b.cloned()));
            }
        },
    );
    v
}

fn speed() {
    let snaps = sample(2048);
    for enc in [Encoding::Full, Encoding::Delta] {
        for mut c in codecs(enc, false) {
            let mut e = Encoder::default();
            let packed: Vec<Vec<u8>> = snaps
                .iter()
                .map(|(s, b)| c.compress(e.encode(enc, s, b.as_ref())).to_vec())
                .collect();
            let mut i = 0;
            let encode = time(|| {
                let (s, b) = &snaps[i % snaps.len()];
                i += 1;
                let bytes = e.encode(enc, black_box(s), b.as_ref());
                black_box(c.compress(bytes).len());
            });
            let mut i = 0;
            let decode_ns = time(|| {
                let k = i % snaps.len();
                i += 1;
                let bytes = c.decompress(black_box(&packed[k])).expect("valid");
                black_box(decode(enc, bytes, snaps[k].1.as_ref()).expect("valid"));
            });
            println!(
                "{{\"suite\":\"speed\",\"encoding\":\"{}\",\"codec\":\"{}\",\"encode_ns\":{encode:.0},\"decode_ns\":{decode_ns:.0}}}",
                enc.name(),
                c.name()
            );
        }
    }
}

fn fuzz(method: &str, n: usize) {
    let (enc, codec) = method.split_once('/').unwrap_or_else(|| usage());
    let enc = match enc {
        "full" => Encoding::Full,
        "delta" => Encoding::Delta,
        _ => usage(),
    };
    let mut c = codecs(enc, false)
        .into_iter()
        .find(|c| c.name() == codec)
        .unwrap_or_else(|| usage());
    let snaps = sample(2048);
    let mut e = Encoder::default();
    let valid: Vec<Vec<u8>> = snaps
        .iter()
        .map(|(s, b)| c.compress(e.encode(enc, s, b.as_ref())).to_vec())
        .collect();
    // A panic is counted, not printed.
    std::panic::set_hook(Box::new(|_| {}));
    let mut rng = Rng(0x5eed);
    for kind in ["random", "mutated"] {
        let (mut rejected, mut accepted, mut panicked) = (0usize, 0usize, 0usize);
        let mut slowest = Duration::ZERO;
        for _ in 0..n {
            let k = (rng.next() as usize) % snaps.len();
            let input = if kind == "random" {
                let len = (rng.next() % 4096) as usize;
                (0..len).map(|_| rng.next() as u8).collect::<Vec<u8>>()
            } else {
                let mut v = valid[k].clone();
                match rng.next() % 4 {
                    0 => v.truncate((rng.next() as usize) % v.len().max(1)),
                    1 => v.extend((0..1 + rng.next() % 16).map(|_| rng.next() as u8)),
                    _ => {
                        for _ in 0..1 + rng.next() % 4 {
                            if !v.is_empty() {
                                let at = (rng.next() as usize) % v.len();
                                v[at] = rng.next() as u8;
                            }
                        }
                    }
                }
                v
            };
            let base = snaps[k].1.as_ref();
            let start = Instant::now();
            let r = catch_unwind(AssertUnwindSafe(|| {
                c.decompress(&input)
                    .and_then(|bytes| decode(enc, bytes, base))
                    .is_some()
            }));
            slowest = slowest.max(start.elapsed());
            match r {
                Ok(true) => accepted += 1,
                Ok(false) => rejected += 1,
                Err(_) => panicked += 1,
            }
        }
        println!(
            "{{\"suite\":\"fuzz\",\"method\":\"{method}\",\"inputs\":\"{kind}\",\"count\":{n},\"rejected\":{rejected},\"accepted\":{accepted},\"panicked\":{panicked},\"slowest_us\":{:.1}}}",
            slowest.as_secs_f64() * 1e6
        );
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: compression-bench sizes | loss | speed | fuzz full|delta/CODEC N | recorded TRAIN TEST sizes|loss|speed"
    );
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("sizes") => sizes(),
        Some("loss") => loss(),
        Some("speed") => speed(),
        Some("recorded") => {
            let load = |i: usize| {
                let path = args.get(i).map_or_else(|| usage(), String::as_str);
                let (streams, bad) = recorded::load(path).unwrap_or_else(|e| {
                    eprintln!("{path}: {e}");
                    std::process::exit(1)
                });
                let snapshots: usize = streams.iter().map(Vec::len).sum();
                let entities: usize = streams.iter().flatten().map(|s| s.entities.len()).sum();
                println!(
                    "{{\"suite\":\"recording\",\"file\":\"{path}\",\"bots\":{},\"snapshots\":{snapshots},\"mean_entities\":{:.1},\"undecodable\":{bad}}}",
                    streams.len(),
                    entities as f64 / snapshots.max(1) as f64
                );
                streams
            };
            let streams = (load(2), load(3));
            let _ = RECORDED.set(streams);
            match args.get(4).map(String::as_str) {
                Some("sizes") => sizes(),
                Some("loss") => loss(),
                Some("speed") => speed(),
                _ => usage(),
            }
        }
        Some("fuzz") => fuzz(
            args.get(2).map_or_else(|| usage(), String::as_str),
            args.get(3)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| usage()),
        ),
        _ => usage(),
    }
}
