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

/// One frame of entity `id` as the client showed it: render time in seconds, position, and how it was obtained.
type Shown = (f64, [f32; 3], Kind);

/// Runs `secs` of play over `link` and returns every frame in which the client showed entity `id`.
fn play(link: Link, secs: f64, seed: u64, id: u32) -> Vec<Shown> {
    let mut rng = Rng(seed);
    let mut client = Client::new(99, TICK_HZ, ClientConfig::default());
    let mut encoder = DeltaEncoder::default();
    // Datagrams in flight: (arrival time, bytes).
    let mut flight: Vec<(f64, Vec<u8>)> = Vec::new();
    // The server's sent snapshots, as baselines for the delta against the client's acknowledgement.
    let mut sent: Vec<(u32, Vec<EntityState>)> = Vec::new();
    let mut shown = Vec::new();
    let mut out: Vec<Sample> = Vec::new();
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
        if let Some(s) = out.iter().find(|s| s.id == id) {
            shown.push((render / f64::from(TICK_HZ), s.pose.position, s.kind));
        }
    }
    shown
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Measures entity `id` while it is on a smooth stretch of its path: after the first second, and away from its
/// corners and teleports, which the tests below measure on their own.
fn run(link: Link, secs: f64, seed: u64, id: u32) -> Figures {
    let mut figures = Figures::default();
    let mut previous: Option<[f32; 3]> = None;
    for (t, e, kind) in play(link, secs, seed, id) {
        if t < 1.0 || near_a_break(id, t) {
            previous = None;
            continue;
        }
        let (p, _) = truth(id, t);
        figures.errors.push(distance(p, e));
        figures.frames += 1;
        match kind {
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

#[test]
fn corners_of_a_fast_car_are_cut_by_less_than_a_tick_of_travel() {
    let shown = play(BAD, 60.0, 5, 2);
    let mut errors: Vec<f32> = shown
        .iter()
        .filter(|&&(t, _, _)| t > 1.0 && near_a_break(2, t))
        .map(|&(t, e, _)| distance(truth(2, t).0, e))
        .collect();
    errors.sort_by(f32::total_cmp);
    let at = |p: f64| errors[((errors.len() - 1) as f64 * p) as usize];
    let (p50, p95, max) = (at(0.5), at(0.95), at(1.0));
    println!(
        "car corners, bad link: {} frames within 0.3 s of a right-angle turn, error p50 {:.1} cm, p95 {:.1} cm, \
         max {:.1} cm",
        errors.len(),
        p50 * 100.0,
        p95 * 100.0,
        max * 100.0
    );
    assert!(errors.len() > 500, "{} frames", errors.len());
    // The turns fall on a snapshot, whose velocity already points along the new leg, so the curve from the sample
    // before bulges inside the corner. At 18 m/s a tick covers 0.9 m, and 1.8 m when the snapshot before the
    // corner is lost: the cut stays under half of that.
    assert!(max < 0.75, "max {max}");
    assert!(p95 < 0.3, "p95 {p95}");
}

#[test]
fn a_teleport_is_one_jump_with_nothing_in_between() {
    let shown = play(BAD, 60.0, 6, 3);
    let tick = 1.0 / f64::from(TICK_HZ);
    let (mut streak, mut early) = (0, 0);
    let mut jumps = 0;
    for pair in shown.windows(2) {
        let ((_, a, _), (t1, b, _)) = (pair[0], pair[1]);
        if distance(a, b) > 100.0 {
            jumps += 1;
        }
        // Every frame shows the walker where it was at the render time, or where it was one tick later: the
        // interpolator jumps at the start of the snapshot interval that holds the teleport. Anything else would
        // be a frame between the two places.
        let now = distance(truth(3, t1).0, b);
        let next = distance(truth(3, t1 + tick).0, b);
        if now > 1.0 && next > 1.0 {
            streak += 1;
        } else if now > 1.0 {
            early += 1;
        }
    }
    println!(
        "walker teleports, bad link: {jumps} jumps over 60 s, {streak} frames between the two places, {early} frames \
         shown at the new place up to a tick early"
    );
    // A jump every 4 s, after the first second.
    assert!((13..=15).contains(&jumps), "{jumps} jumps");
    assert_eq!(streak, 0);
    // At most one tick (three 60 Hz frames) early per jump.
    assert!(early <= 3 * jumps, "{early} early frames for {jumps} jumps");
}
