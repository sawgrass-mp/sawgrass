//! A roleplay server's players, who can hear whom, and when they speak.
//!
//! Server and load generator build the same world from the same seed, so the generator knows which frames each
//! client should receive and can check that every one arrived.

use std::collections::HashMap;

use crate::common::Rng;

/// Hearing range of each proximity mode, in metres: whisper, normal, shout. Our assumption; see README.md.
pub const RANGES: [f32; 3] = [3.0, 7.0, 15.0];
/// Share of speech spurts in each mode.
const MODE_SHARE: [f64; 3] = [0.10, 0.75, 0.15];
/// Grid cell, at least the largest range, so a query reads the 3 x 3 cells around the speaker.
const CELL: f32 = 16.0;
/// Mean length of a speech spurt, in 20 ms frames (1.5 s).
const MEAN_SPURT_FRAMES: f64 = 75.0;
/// Share of a radio member's spurts that go out on the radio.
const RADIO_SHARE: f64 = 0.30;

/// Flags carried in a voice frame's header.
pub const FLAG_RADIO: u8 = 0x04;
pub const FLAG_PHONE: u8 = 0x08;
pub const MODE_MASK: u8 = 0x03;

#[derive(Debug, Clone)]
pub struct Player {
    pub pos: [f32; 2],
    /// Routing bucket (dimension): 0 is the open world, others are interiors. Players in different buckets
    /// never hear each other by proximity.
    pub bucket: u32,
    pub radio: Option<u32>,
    pub phone: Option<u32>,
}

#[derive(Debug)]
pub struct World {
    pub players: Vec<Player>,
    pub channels: Vec<Vec<u32>>,
    grid: HashMap<(i32, i32, u32), Vec<u32>>,
    /// Fixed rooms instead of a city, for the comparison with SFUs: players `k * size .. (k + 1) * size` form
    /// room `k`, its first player always speaks, and everyone else in the room hears them.
    pub room_size: Option<usize>,
}

/// `ROOMS=size` in the environment selects fixed rooms (see `World::room_size`).
pub fn room_size() -> Option<usize> {
    std::env::var("ROOMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s: &usize| s >= 2)
}

fn cell(p: [f32; 2]) -> (i32, i32) {
    ((p[0] / CELL).floor() as i32, (p[1] / CELL).floor() as i32)
}

impl World {
    /// `n` players: 70% in hotspots of very different sizes (a quarter of them interiors), 30% spread over the
    /// map in groups of one to four, as in cars. 3% are on the police radio, 1.5% on the medics', 10% on small
    /// channels of 4 to 12, and 4% are in phone calls.
    pub fn new(n: usize, seed: u64) -> Self {
        if let Some(size) = room_size() {
            return Self {
                players: (0..n)
                    .map(|_| Player {
                        pos: [0.0; 2],
                        bucket: 0,
                        radio: None,
                        phone: None,
                    })
                    .collect(),
                channels: Vec::new(),
                grid: HashMap::new(),
                room_size: Some(size),
            };
        }
        let mut rng = Rng::new(seed);
        let map = 6000.0;
        let hotspots = (n / 64).max(8);
        let centres: Vec<([f32; 2], f32, u32)> = (0..hotspots)
            .map(|k| {
                let c = [(rng.f64() * map) as f32, (rng.f64() * map) as f32];
                let sigma = 8.0 + rng.f64() as f32 * 32.0;
                let bucket = if k % 4 == 3 { 1 + k as u32 } else { 0 };
                (c, sigma, bucket)
            })
            .collect();
        // Zipf-like popularity: the busiest hotspot draws far more players than the quietest.
        let weights: Vec<f64> = (0..hotspots).map(|k| 1.0 / (k as f64 + 1.0)).collect();
        let total: f64 = weights.iter().sum();
        let mut players = Vec::with_capacity(n);
        let in_hotspots = n * 7 / 10;
        while players.len() < in_hotspots {
            let mut r = rng.f64() * total;
            let mut k = 0;
            while k + 1 < hotspots && r >= weights[k] {
                r -= weights[k];
                k += 1;
            }
            let (c, sigma, bucket) = centres[k];
            let pos = [
                c[0] + (rng.normal() as f32) * sigma,
                c[1] + (rng.normal() as f32) * sigma,
            ];
            players.push(Player {
                pos,
                bucket,
                radio: None,
                phone: None,
            });
        }
        while players.len() < n {
            let c = [(rng.f64() * map) as f32, (rng.f64() * map) as f32];
            let group = 1 + rng.below(4);
            for _ in 0..group.min(n - players.len()) {
                let pos = [c[0] + rng.f64() as f32, c[1] + rng.f64() as f32];
                players.push(Player {
                    pos,
                    bucket: 0,
                    radio: None,
                    phone: None,
                });
            }
        }

        // Radio channels and phone calls, over a random order of players.
        let mut order: Vec<u32> = (0..n as u32).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, rng.below(i + 1));
        }
        let mut next = 0usize;
        let mut channels: Vec<Vec<u32>> = Vec::new();
        for size in [n * 3 / 100, n * 15 / 1000] {
            channels.push(order[next..next + size].to_vec());
            next += size;
        }
        let small_end = next + n / 10;
        while next < small_end {
            let size = (4 + rng.below(9)).min(small_end - next);
            channels.push(order[next..next + size].to_vec());
            next += size;
        }
        for (c, members) in channels.iter().enumerate() {
            for &m in members {
                players[m as usize].radio = Some(c as u32);
            }
        }
        let calls_end = next + (n * 4 / 100) / 2 * 2;
        while next + 1 < calls_end {
            let (a, b) = (order[next], order[next + 1]);
            players[a as usize].phone = Some(b);
            players[b as usize].phone = Some(a);
            next += 2;
        }

        let mut grid: HashMap<(i32, i32, u32), Vec<u32>> = HashMap::new();
        for (i, p) in players.iter().enumerate() {
            let (x, y) = cell(p.pos);
            grid.entry((x, y, p.bucket)).or_default().push(i as u32);
        }
        Self {
            players,
            channels,
            grid,
            room_size: None,
        }
    }

    /// Everyone who hears a frame from `speaker` with `flags`, sorted and without duplicates, into `out`.
    pub fn recipients(&self, speaker: u32, flags: u8, out: &mut Vec<u32>) {
        out.clear();
        if let Some(size) = self.room_size {
            let first = speaker as usize / size * size;
            let end = (first + size).min(self.players.len());
            out.extend((first..end).map(|o| o as u32).filter(|&o| o != speaker));
            return;
        }
        let p = &self.players[speaker as usize];
        let range = RANGES[usize::from(flags & MODE_MASK).min(2)];
        let (cx, cy) = cell(p.pos);
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(list) = self.grid.get(&(cx + dx, cy + dy, p.bucket)) {
                    for &o in list {
                        let q = self.players[o as usize].pos;
                        let (ex, ey) = (q[0] - p.pos[0], q[1] - p.pos[1]);
                        if o != speaker && ex * ex + ey * ey <= range * range {
                            out.push(o);
                        }
                    }
                }
            }
        }
        if flags & FLAG_RADIO != 0
            && let Some(c) = p.radio
        {
            out.extend(
                self.channels[c as usize]
                    .iter()
                    .copied()
                    .filter(|&o| o != speaker),
            );
        }
        if flags & FLAG_PHONE != 0
            && let Some(o) = p.phone
        {
            out.push(o);
        }
        out.sort_unstable();
        out.dedup();
    }

    /// Everyone in the 3 x 3 cells of `cell_m` metres around the speaker, in the same bucket: what a server
    /// that routes by grid cell, leaving the range cut to the client, would send to (no radio or phone).
    pub fn cell_recipients(&self, speaker: u32, cell_m: f32) -> usize {
        let p = &self.players[speaker as usize];
        let c = |q: [f32; 2]| ((q[0] / cell_m).floor() as i32, (q[1] / cell_m).floor() as i32);
        let (cx, cy) = c(p.pos);
        self.players
            .iter()
            .enumerate()
            .filter(|&(i, q)| {
                let (x, y) = c(q.pos);
                i as u32 != speaker && q.bucket == p.bucket && (x - cx).abs() <= 1 && (y - cy).abs() <= 1
            })
            .count()
    }
}

/// Who is speaking, frame by frame: each player alternates between silence and spurts of 1.5 s on average,
/// so that `share` of players speak at any moment. Each spurt picks a mode, and whether it goes on the radio
/// or the phone.
#[derive(Debug)]
pub struct Speech {
    rng: Rng,
    p_start: f64,
    p_stop: f64,
    /// Flags of the current spurt, or `None` when silent.
    pub state: Vec<Option<u8>>,
}

impl Speech {
    pub fn new(world: &World, share: f64, seed: u64) -> Self {
        let p_stop = 1.0 / MEAN_SPURT_FRAMES;
        let p_start = share * p_stop / (1.0 - share);
        let mut s = Self {
            rng: Rng::new(seed),
            p_start,
            p_stop,
            state: vec![None; world.players.len()],
        };
        if let Some(size) = world.room_size {
            for i in (0..world.players.len()).step_by(size) {
                s.state[i] = Some(1);
            }
            s.p_start = 0.0;
            s.p_stop = 0.0;
            return s;
        }
        // Start in the steady state.
        for i in 0..world.players.len() {
            if s.rng.f64() < share {
                s.state[i] = Some(s.spurt(&world.players[i]));
            }
        }
        s
    }

    fn spurt(&mut self, p: &Player) -> u8 {
        let r = self.rng.f64();
        let mode = if r < MODE_SHARE[0] {
            0
        } else if r < MODE_SHARE[0] + MODE_SHARE[1] {
            1
        } else {
            2
        };
        let mut flags = mode;
        if p.phone.is_some() {
            flags |= FLAG_PHONE;
        }
        if p.radio.is_some() && self.rng.f64() < RADIO_SHARE {
            flags |= FLAG_RADIO;
        }
        flags
    }

    /// Advances one 20 ms frame for players `range` (a load generator thread owns a slice of them).
    pub fn step(&mut self, world: &World, range: std::ops::Range<usize>) {
        for i in range {
            match self.state[i] {
                Some(_) if self.rng.f64() < self.p_stop => self.state[i] = None,
                None if self.rng.f64() < self.p_start => {
                    self.state[i] = Some(self.spurt(&world.players[i]));
                }
                _ => {}
            }
        }
    }
}
