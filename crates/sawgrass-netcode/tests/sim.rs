//! A deterministic network between a simulated server and a client: snapshots of entities on known paths cross
//! a link with latency, jitter, loss and reordering, and the client's interpolated poses are compared with where
//! the entities really were at the render time.
//!
//! `cargo test -p sawgrass-netcode --test sim -- --nocapture` prints the figures.

use sawgrass_netcode::interp::{Kind, Sample};
use sawgrass_netcode::{Client, ClientConfig};
use sawgrass_protocol::delta::{Baseline, DeltaEncoder, fragments};
use sawgrass_protocol::{EntityState, PacketHeader, PacketKind, Position, Rotation, Velocity};

const TICK_HZ: u16 = 20;
const FRAME_HZ: f64 = 60.0;

/// A small deterministic generator (xorshift), so every run sees the same network.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// How the link treats datagrams.
#[derive(Clone, Copy)]
struct Link {
    latency: f64,
    jitter: f64,
    loss: f64,
}

/// Entity 1 runs a 15 m circle at 5.5 m/s; entity 2 drives straight at 18 m/s and turns at right angles every
/// 3 s; entity 3 walks at 1.4 m/s and is teleported 300 m every 4 s.
fn truth(id: u32, t: f64) -> ([f32; 3], [f32; 3]) {
    match id {
        1 => {
            let (r, w) = (15.0, 5.5 / 15.0);
            let a = w * t;
            (
                [(r * a.cos()) as f32, (r * a.sin()) as f32, 30.0],
                [(-r * w * a.sin()) as f32, (r * w * a.cos()) as f32, 0.0],
            )
        }
        2 => {
            let leg = (t / 3.0).floor();
            let s = t - leg * 3.0;
            // Legs alternate +x and +y, so the path is a staircase.
            let (done_x, done_y) = (((leg + 1.0) / 2.0).floor() * 54.0, (leg / 2.0).floor() * 54.0);
            if leg as i64 % 2 == 0 {
                (
                    [(done_x + 18.0 * s) as f32, done_y as f32, 30.0],
                    [18.0, 0.0, 0.0],
                )
            } else {
                (
                    [done_x as f32, (done_y + 18.0 * s) as f32, 30.0],
                    [0.0, 18.0, 0.0],
                )
            }
        }
        _ => {
            let jump = (t / 4.0).floor();
            (
                [(jump * 300.0 + 1.4 * (t - jump * 4.0)) as f32, -50.0, 30.0],
                [1.4, 0.0, 0.0],
            )
        }
    }
}

fn state(id: u32, t: f64) -> EntityState {
    let (p, v) = truth(id, t);
    let yaw = v[1].atan2(v[0]) / 2.0;
    EntityState {
        id,
        position: Position::from_metres(p).expect("position"),
        velocity: Velocity::from_metres_per_second(v).expect("velocity"),
        rotation: Rotation::from_quaternion([0.0, 0.0, yaw.sin(), yaw.cos()]).expect("rotation"),
    }
}

#[derive(Debug, Default)]
struct Figures {
    errors: Vec<f32>,
    extrapolated: usize,
    held: usize,
    frames: usize,
    largest_step: f32,
}

impl Figures {
    fn percentile(&mut self, p: f64) -> f32 {
        self.errors.sort_by(f32::total_cmp);
        self.errors[((self.errors.len() - 1) as f64 * p) as usize]
    }
}

/// Runs `secs` of play over `link` and measures entity `id` while it is on a smooth stretch of its path.
fn run(link: Link, secs: f64, seed: u64, id: u32) -> Figures {
    let mut rng = Rng(seed);
    let mut client = Client::new(99, TICK_HZ, ClientConfig::default());
    let mut encoder = DeltaEncoder::default();
    // Datagrams in flight: (arrival time, bytes).
    let mut flight: Vec<(f64, Vec<u8>)> = Vec::new();
    // The server's sent snapshots, as baselines for the delta against the client's acknowledgement.
    let mut sent: Vec<(u32, Vec<EntityState>)> = Vec::new();
    let mut figures = Figures::default();
    let mut out: Vec<Sample> = Vec::new();
    let mut previous: Option<[f32; 3]> = None;
    let mut next_tick = 0u32;
    let frames = (secs * FRAME_HZ) as u32;
    for frame in 0..frames {
        let now = f64::from(frame) / FRAME_HZ;
        // The server ticks.
        while f64::from(next_tick) / f64::from(TICK_HZ) <= now {
            let tick = next_tick;
            next_tick += 1;
            let t = f64::from(tick) / f64::from(TICK_HZ);
            let states: Vec<EntityState> = (1..=3).map(|id| state(id, t)).collect();
            let ack = client.ack();
            let base = sent
                .iter()
                .find(|(t, _)| *t == ack && ack > 0)
                .map(|(t, e)| Baseline {
                    tick: *t,
                    entities: e,
                });
            let mut payload = Vec::new();
            encoder.encode(tick, &states, base, &mut payload).expect("encode");
            for (fragment, part) in fragments(&payload).expect("fragments") {
                if rng.unit() < link.loss {
                    continue;
                }
                let mut d = vec![0; PacketHeader::SIZE];
                PacketHeader {
                    kind: PacketKind::DeltaSnapshot,
                    sequence: 0,
                    tick,
                }
                .encode(&mut d)
                .expect("header");
                d.push(fragment);
                d.extend_from_slice(part);
                flight.push((now + link.latency + link.jitter * rng.unit(), d));
            }
            sent.push((tick, states));
            if sent.len() > 16 {
                sent.remove(0);
            }
        }
        // Datagrams that arrived by now, in arrival order: jitter reorders them.
        flight.sort_by(|a, b| a.0.total_cmp(&b.0));
        let due = flight.partition_point(|(at, _)| *at <= now);
        for (at, d) in flight.drain(..due) {
            let _ = client.receive(&d, at);
        }
        client.remote(now, &mut out);
        assert!(
            out.iter().all(|s| s.id != 99),
            "the local player is never interpolated"
        );
        let Some(render) = client.render_tick() else {
            continue;
        };
        let Some(s) = out.iter().find(|s| s.id == id) else {
            continue;
        };
        let t = render / f64::from(TICK_HZ);
        // Skip the second after start-up and the samples around a corner or a teleport, where the true path
        // itself is not smooth.
        if t < 1.0 || near_a_break(id, t) {
            previous = None;
            continue;
        }
        let (p, _) = truth(id, t);
        let e = s.pose.position;
        figures
            .errors
            .push(((p[0] - e[0]).powi(2) + (p[1] - e[1]).powi(2) + (p[2] - e[2]).powi(2)).sqrt());
        figures.frames += 1;
        match s.kind {
            Kind::Extrapolated => figures.extrapolated += 1,
            Kind::Held => figures.held += 1,
            _ => {}
        }
        if let Some(q) = previous {
            let step = ((q[0] - e[0]).powi(2) + (q[1] - e[1]).powi(2)).sqrt();
            figures.largest_step = figures.largest_step.max(step);
        }
        previous = Some(e);
    }
    figures
}

fn near_a_break(id: u32, t: f64) -> bool {
    let period = match id {
        2 => 3.0,
        3 => 4.0,
        _ => return false,
    };
    let into = t % period;
    !(0.3..period - 0.3).contains(&into)
}

fn report(name: &str, f: &mut Figures) -> (f32, f32) {
    let (p50, p99) = (f.percentile(0.5), f.percentile(0.99));
    println!(
        "{name}: {} frames, error p50 {:.1} cm, p99 {:.1} cm, extrapolated {:.1}%, held {}, largest step {:.2} m",
        f.frames,
        p50 * 100.0,
        p99 * 100.0,
        100.0 * f.extrapolated as f64 / f.frames as f64,
        f.held,
        f.largest_step,
    );
    (p50, p99)
}

const GOOD: Link = Link {
    latency: 0.040,
    jitter: 0.010,
    loss: 0.0,
};
const BAD: Link = Link {
    latency: 0.080,
    jitter: 0.060,
    loss: 0.05,
};

#[test]
fn a_curve_on_a_good_link_is_followed_to_centimetres() {
    let mut f = run(GOOD, 30.0, 1, 1);
    let (p50, p99) = report("circle, good link", &mut f);
    assert!(p50 < 0.02 && p99 < 0.04, "p50 {p50} p99 {p99}");
    assert_eq!(f.held, 0);
    // 5.5 m/s at 60 frames a second is 9 cm a frame.
    assert!(f.largest_step < 0.12, "{}", f.largest_step);
}

#[test]
fn a_curve_on_a_bad_link_stays_close_and_smooth() {
    let mut f = run(BAD, 60.0, 2, 1);
    let (p50, p99) = report("circle, bad link", &mut f);
    assert!(p50 < 0.03 && p99 < 0.1, "p50 {p50} p99 {p99}");
    assert!(
        f.extrapolated * 20 < f.frames,
        "extrapolated {} of {}",
        f.extrapolated,
        f.frames
    );
    assert_eq!(f.held, 0);
    assert!(f.largest_step < 0.12, "{}", f.largest_step);
}

#[test]
fn a_fast_car_on_a_bad_link_stays_close() {
    let mut f = run(BAD, 60.0, 3, 2);
    let (p50, p99) = report("car at 18 m/s, bad link", &mut f);
    assert!(p50 < 0.03 && p99 < 0.5, "p50 {p50} p99 {p99}");
    // 18 m/s at 60 frames a second is 30 cm a frame.
    assert!(f.largest_step < 0.35, "{}", f.largest_step);
}

#[test]
fn a_teleported_walker_jumps_without_a_streak() {
    let mut f = run(BAD, 40.0, 4, 3);
    let (p50, _) = report("walker with teleports, bad link", &mut f);
    assert!(p50 < 0.03, "p50 {p50}");
    // Between teleports it never moves more than a walker does in a few frames.
    assert!(f.largest_step < 0.5, "{}", f.largest_step);
}
