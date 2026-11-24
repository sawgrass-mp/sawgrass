//! Fan-out of the simulated world without a network: how many frames and datagrams each routing design would
//! send, and what routing costs.

use std::time::Instant;

use crate::common::{flush, pct};
use crate::world::{FLAG_RADIO, Speech, World};

/// Simulates 60 s of speech in 20 ms frames and reports fan-out and batching.
pub fn world(players: usize, share: f64, seed: u64, payload: usize) {
    let world = World::new(players, seed);
    let mut speech = Speech::new(&world, share, seed + 1000);
    let frames = 3000;
    let mut scratch = Vec::new();
    let mut fanout: Vec<u32> = Vec::new();
    let mut radio_out = 0u64;
    let mut speaking = 0u64;
    let mut route_ns = 0u128;
    // Frames per listener in the current 20, 40 and 60 ms windows.
    let mut window: [Vec<u32>; 3] = [vec![0; players], vec![0; players], vec![0; players]];
    let mut batches = [0u64; 3];
    let per_batch = (1_350 - 2) / (6 + payload);
    for f in 0..frames {
        speech.step(&world, 0..players);
        for s in 0..players {
            let Some(flags) = speech.state[s] else { continue };
            speaking += 1;
            let t = Instant::now();
            world.recipients(s as u32, flags, &mut scratch);
            route_ns += t.elapsed().as_nanos();
            fanout.push(scratch.len() as u32);
            if flags & FLAG_RADIO != 0 {
                radio_out += scratch.len() as u64;
            }
            for &r in &scratch {
                for w in &mut window {
                    w[r as usize] += 1;
                }
            }
        }
        for (k, span) in [1usize, 2, 3].into_iter().enumerate() {
            if (f + 1) % span == 0 {
                for c in window[k].iter_mut() {
                    batches[k] += u64::from(*c).div_ceil(per_batch as u64);
                    *c = 0;
                }
            }
        }
    }
    let secs = frames as f64 * 0.02;
    let frames_in = speaking as f64 / secs;
    let out: u64 = fanout.iter().map(|&x| u64::from(x)).sum();
    let zero = fanout.iter().filter(|&&x| x == 0).count() as f64 / fanout.len().max(1) as f64;
    let mut sorted = fanout.clone();
    sorted.sort_unstable();
    // What routing by grid cell would send, over every player as a speaker (no radio or phone).
    let cell = |m: f32| {
        (0..players as u32)
            .map(|s| world.cell_recipients(s, m) as f64)
            .sum::<f64>()
            / players as f64
    };
    let prox_only = {
        let mut sum = 0.0;
        for s in 0..players as u32 {
            world.recipients(s, 1, &mut scratch);
            sum += scratch.len() as f64;
        }
        sum / players as f64
    };
    println!(
        "world players={players} share={share} seed={seed} payload={payload} speaking_mean={:.1} frames_in_per_s={frames_in:.0} fanout_mean={:.2} fanout_p50={} fanout_p99={} fanout_max={} zero_listeners={zero:.3} frames_out_per_s={:.0} radio_out_share={:.3} batch20_datagrams_per_s={:.0} batch40_datagrams_per_s={:.0} batch60_datagrams_per_s={:.0} route_ns_per_frame={:.0} normal_range_fanout={prox_only:.2} cell16_fanout={:.2} cell64_fanout={:.2} cell128_fanout={:.2}",
        speaking as f64 / frames as f64,
        out as f64 / fanout.len().max(1) as f64,
        pct(&sorted, 50.0),
        pct(&sorted, 99.0),
        sorted.last().copied().unwrap_or(0),
        out as f64 / secs,
        radio_out as f64 / out.max(1) as f64,
        batches[0] as f64 / secs,
        batches[1] as f64 / secs,
        batches[2] as f64 / secs,
        route_ns as f64 / speaking.max(1) as f64,
        cell(16.0),
        cell(64.0),
        cell(128.0),
    );
    flush();
}
