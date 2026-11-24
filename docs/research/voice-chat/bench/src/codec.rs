//! Opus through libopus: packet sizes, encode and decode cost, look-ahead, and how much of a lossy stream
//! packet loss concealment, in-band FEC and redundant frames recover.

use std::time::Instant;

use opus::{Application, Bitrate, Channels, Decoder, Encoder, FrameSize};

use crate::common::{Rng, flush, pct};

pub const RATE: u32 = 48_000;

/// Synthetic speech, generated so that no recording is needed: syllables of a glottal pulse train shaped by
/// vowel formants, some unvoiced fricatives, and short pauses between words, at 48 kHz mono.
pub fn speech(secs: f64, seed: u64) -> Vec<i16> {
    const VOWELS: [[f64; 3]; 5] = [
        [730.0, 1090.0, 2440.0],
        [530.0, 1840.0, 2480.0],
        [270.0, 2290.0, 3010.0],
        [570.0, 840.0, 2410.0],
        [300.0, 870.0, 2240.0],
    ];
    let fs = f64::from(RATE);
    let total = (secs * fs) as usize;
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(total);
    let mut phase = 0.0f64;
    let mut f0 = 120.0 + rng.f64() * 80.0;
    let mut state = [[0.0f64; 2]; 3];
    while out.len() < total {
        let syllable = (0.12 + rng.f64() * 0.13) * fs;
        let voiced = rng.f64() < 0.8;
        let vowel = VOWELS[rng.below(VOWELS.len())];
        f0 = (f0 + (rng.f64() - 0.5) * 30.0).clamp(90.0, 230.0);
        for n in 0..syllable as usize {
            let t = n as f64 / syllable;
            let env = (t * 8.0).min(1.0) * ((1.0 - t) * 5.0).min(1.0);
            let excitation = if voiced {
                phase += (f0 * (1.0 + 0.03 * (t * 6.0).sin())) / fs;
                if phase >= 1.0 {
                    phase -= 1.0;
                    1.0
                } else {
                    0.0
                }
            } else {
                rng.f64() - 0.5
            };
            let mut y_sum = 0.0;
            for (k, &f) in vowel.iter().enumerate() {
                let f = if voiced { f } else { f * 2.5 };
                let bw = 80.0 + 40.0 * k as f64;
                let r = (-std::f64::consts::PI * bw / fs).exp();
                let a1 = 2.0 * r * (std::f64::consts::TAU * f / fs).cos();
                let a2 = -r * r;
                let y = excitation + a1 * state[k][0] + a2 * state[k][1];
                state[k][1] = state[k][0];
                state[k][0] = y;
                y_sum += y / (k as f64 + 1.0);
            }
            let noise = (rng.f64() - 0.5) * 30.0;
            out.push((y_sum * env * 900.0 + noise).clamp(-32_000.0, 32_000.0) as i16);
        }
        if rng.f64() < 0.3 {
            let pause = ((0.05 + rng.f64() * 0.15) * fs) as usize;
            out.extend((0..pause).map(|_| ((rng.f64() - 0.5) * 30.0) as i16));
        }
    }
    out.truncate(total);
    out
}

fn frame_size(ms: u32) -> FrameSize {
    match ms {
        10 => FrameSize::Ms10,
        40 => FrameSize::Ms40,
        60 => FrameSize::Ms60,
        _ => FrameSize::Ms20,
    }
}

pub struct Config {
    pub bitrate: i32,
    pub frame_ms: u32,
    pub complexity: i32,
    pub fec_loss: Option<i32>,
}

fn encoder(c: &Config) -> Encoder {
    let mut e = Encoder::new(RATE, Channels::Mono, Application::Voip).expect("encoder");
    e.set_bitrate(Bitrate::Bits(c.bitrate)).expect("bitrate");
    e.set_complexity(c.complexity).expect("complexity");
    e.set_expert_frame_duration(frame_size(c.frame_ms))
        .expect("frame");
    if let Some(loss) = c.fec_loss {
        e.set_inband_fec(true).expect("fec");
        e.set_packet_loss_perc(loss).expect("loss");
    }
    e
}

/// Encodes all of `pcm` and returns the packets and nanoseconds per frame.
pub fn encode_all(c: &Config, pcm: &[i16]) -> (Vec<Vec<u8>>, f64) {
    let n = (RATE * c.frame_ms / 1000) as usize;
    let mut e = encoder(c);
    let mut buf = [0u8; 1500];
    let mut packets = Vec::with_capacity(pcm.len() / n);
    let t = Instant::now();
    for chunk in pcm.chunks_exact(n) {
        let len = e.encode(chunk, &mut buf).expect("encode");
        packets.push(buf[..len].to_vec());
    }
    let ns = t.elapsed().as_nanos() as f64 / packets.len().max(1) as f64;
    (packets, ns)
}

fn decode_ns(c: &Config, packets: &[Vec<u8>]) -> f64 {
    let n = (RATE * c.frame_ms / 1000) as usize;
    let mut d = Decoder::new(RATE, Channels::Mono).expect("decoder");
    let mut out = vec![0i16; n];
    let t = Instant::now();
    for p in packets {
        d.decode(p, &mut out, false).expect("decode");
    }
    t.elapsed().as_nanos() as f64 / packets.len().max(1) as f64
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Sizes and speed of one configuration: median of five rounds over 60 s of speech.
pub fn measure(c: &Config, pcm: &[i16], label: &str) {
    let mut enc = Vec::new();
    let mut dec = Vec::new();
    let mut sizes = Vec::new();
    for _ in 0..5 {
        let (packets, ns) = encode_all(c, pcm);
        enc.push(ns);
        dec.push(decode_ns(c, &packets));
        sizes = packets.iter().map(Vec::len).collect::<Vec<_>>();
    }
    let mean = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
    sizes.sort_unstable();
    let kbps = mean * 8.0 * 1000.0 / f64::from(c.frame_ms) / 1000.0;
    println!(
        "codec label={label} bitrate={} frame_ms={} complexity={} fec_loss={} mean_bytes={mean:.1} p99_bytes={} max_bytes={} payload_kbps={kbps:.1} encode_ns={:.0} decode_ns={:.0}",
        c.bitrate,
        c.frame_ms,
        c.complexity,
        c.fec_loss.unwrap_or(-1),
        pct(&sizes, 99.0),
        sizes.last().copied().unwrap_or(0),
        median(enc),
        median(dec),
    );
    flush();
}

/// Loss pattern: independent with probability `p`, or bursts (Gilbert model) with the same mean loss and a
/// mean burst of `burst` packets.
pub fn losses(frames: usize, p: f64, burst: f64, seed: u64) -> Vec<bool> {
    let mut rng = Rng::new(seed);
    if burst <= 1.0 {
        return (0..frames).map(|_| rng.f64() < p).collect();
    }
    let leave_bad = 1.0 / burst;
    let enter_bad = p * leave_bad / (1.0 - p);
    let mut bad = false;
    (0..frames)
        .map(|_| {
            bad = if bad {
                rng.f64() >= leave_bad
            } else {
                rng.f64() < enter_bad
            };
            bad
        })
        .collect()
}

/// What became of the frames of a lossy stream.
#[derive(Debug, Default, Clone, Copy)]
pub struct Recovery {
    /// Frames decoded from their own packet.
    pub direct: usize,
    /// Lost frames rebuilt from the next packet: its in-band FEC, or its redundant copy.
    pub repaired: usize,
    /// Lost frames with nothing to rebuild them from, filled by packet loss concealment.
    pub concealed: usize,
    /// Sum over lost frames of the signal-to-error ratio, in dB, of the frame the client played against the
    /// frame it would have played had the packet arrived (capped at 60 dB).
    pub snr_sum: f64,
}

fn snr_db(out: &[i16], reference: &[i16]) -> f64 {
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&o, &r) in out.iter().zip(reference) {
        sig += f64::from(r) * f64::from(r);
        err += (f64::from(o) - f64::from(r)).powi(2);
    }
    if err == 0.0 {
        60.0
    } else {
        (10.0 * (sig.max(1.0) / err).log10()).min(60.0)
    }
}

/// Decodes a stream with losses, the way a client would. `red` sends each frame again in the next packet, as
/// RFC 2198 redundancy does; `fec` decodes a lost frame from the next packet's in-band FEC. When the next
/// packet carries no FEC data, libopus conceals instead, which shows as a low ratio against the reference.
pub fn recover(packets: &[Vec<u8>], lost: &[bool], fec: bool, red: bool, frame: usize) -> Recovery {
    let mut reference = Decoder::new(RATE, Channels::Mono).expect("decoder");
    let mut d = Decoder::new(RATE, Channels::Mono).expect("decoder");
    let mut want = vec![0i16; frame];
    let mut out = vec![0i16; frame];
    let mut r = Recovery::default();
    for i in 0..packets.len() {
        reference.decode(&packets[i], &mut want, false).expect("decode");
        if !lost[i] {
            d.decode(&packets[i], &mut out, false).expect("decode");
            r.direct += 1;
            continue;
        }
        let next_ok = i + 1 < packets.len() && !lost[i + 1];
        if red && next_ok {
            d.decode(&packets[i], &mut out, false).expect("decode");
            r.repaired += 1;
        } else if fec && next_ok {
            d.decode(&packets[i + 1], &mut out, true).expect("decode fec");
            r.repaired += 1;
        } else {
            d.decode(&[], &mut out, false).expect("conceal");
            r.concealed += 1;
        }
        r.snr_sum += snr_db(&out, &want);
    }
    r
}

/// Everything the codec suite reports.
pub fn suite() {
    let pcm = speech(60.0, 7);
    let mut e = encoder(&Config {
        bitrate: 24_000,
        frame_ms: 20,
        complexity: 10,
        fec_loss: None,
    });
    println!(
        "lookahead samples={} ms={:.2}",
        e.get_lookahead().unwrap_or(0),
        f64::from(e.get_lookahead().unwrap_or(0)) / 48.0
    );
    println!("libopus version={}", opus::version().replace(' ', "_"));
    flush();

    for (bitrate, frame_ms, complexity) in [
        (16_000, 20, 10),
        (24_000, 20, 10),
        (32_000, 20, 10),
        (24_000, 40, 10),
        (24_000, 60, 10),
        (24_000, 20, 5),
        (24_000, 20, 1),
    ] {
        let c = Config {
            bitrate,
            frame_ms,
            complexity,
            fec_loss: None,
        };
        measure(&c, &pcm, "plain");
    }
    for loss in [5, 10, 20] {
        let c = Config {
            bitrate: 24_000,
            frame_ms: 20,
            complexity: 10,
            fec_loss: Some(loss),
        };
        measure(&c, &pcm, "fec");
    }

    // Recovery under loss, 24 kbit/s, 20 ms frames.
    let frame = 960;
    let (plain, _) = encode_all(
        &Config {
            bitrate: 24_000,
            frame_ms: 20,
            complexity: 10,
            fec_loss: None,
        },
        &pcm,
    );
    for (p, burst) in [
        (0.01f64, 1.0f64),
        (0.05, 1.0),
        (0.10, 1.0),
        (0.20, 1.0),
        (0.05, 3.0),
        (0.10, 3.0),
    ] {
        let loss_perc = ((p * 100.0).round() as i32).max(1);
        let (fec_packets, _) = encode_all(
            &Config {
                bitrate: 24_000,
                frame_ms: 20,
                complexity: 10,
                fec_loss: Some(loss_perc),
            },
            &pcm,
        );
        let mut totals = [Recovery::default(); 3];
        for seed in 0..5u64 {
            let lost = losses(plain.len(), p, burst, 100 + seed);
            for (k, (pk, fec, red)) in [
                (&plain, false, false),
                (&fec_packets, true, false),
                (&plain, false, true),
            ]
            .into_iter()
            .enumerate()
            {
                let r = recover(pk, &lost, fec, red, frame);
                totals[k].direct += r.direct;
                totals[k].repaired += r.repaired;
                totals[k].concealed += r.concealed;
                totals[k].snr_sum += r.snr_sum;
            }
        }
        let mean_plain = plain.iter().map(Vec::len).sum::<usize>() as f64 / plain.len() as f64;
        let mean_fec = fec_packets.iter().map(Vec::len).sum::<usize>() as f64 / fec_packets.len() as f64;
        for (k, (name, bytes)) in [
            ("plc", mean_plain),
            ("fec", mean_fec),
            ("red", 2.0 * mean_plain + 4.0),
        ]
        .into_iter()
        .enumerate()
        {
            let t = totals[k];
            let all = (t.direct + t.repaired + t.concealed) as f64;
            println!(
                "loss method={name} loss={p} burst={burst} mean_bytes={bytes:.1} direct={:.4} repaired={:.4} concealed={:.4} lost_frame_snr_db={:.1}",
                t.direct as f64 / all,
                t.repaired as f64 / all,
                t.concealed as f64 / all,
                t.snr_sum / (t.repaired + t.concealed).max(1) as f64,
            );
        }
        flush();
    }

    mix_costs(&pcm);
}

/// Per-operation costs of mixing on the server: decode a speaker's frame once, add it with a gain into a
/// listener's mix, encode the listener's mix.
fn mix_costs(pcm: &[i16]) {
    let frame = 960;
    let (packets, _) = encode_all(
        &Config {
            bitrate: 24_000,
            frame_ms: 20,
            complexity: 10,
            fec_loss: None,
        },
        pcm,
    );
    let mut decoded: Vec<Vec<i16>> = Vec::new();
    let mut d = Decoder::new(RATE, Channels::Mono).expect("decoder");
    for p in packets.iter().take(64) {
        let mut out = vec![0i16; frame];
        d.decode(p, &mut out, false).expect("decode");
        decoded.push(out);
    }
    // Mix: 8 sources into one accumulator with a gain each, repeated.
    let mut acc = vec![0i32; frame];
    let mut rounds = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let iters = 20_000;
        for i in 0..iters {
            acc.iter_mut().for_each(|a| *a = 0);
            for s in 0..8 {
                let src = &decoded[(i + s) % decoded.len()];
                let gain = 1000 + (s as i32) * 300;
                for (a, &x) in acc.iter_mut().zip(src) {
                    *a += (i32::from(x) * gain) >> 12;
                }
            }
            std::hint::black_box(&acc);
        }
        rounds.push(t.elapsed().as_nanos() as f64 / (iters * 8) as f64);
    }
    println!("mix ns_per_source_frame={:.0}", median(rounds));
    let mixed: Vec<i16> = pcm.to_vec();
    for complexity in [10, 5, 1] {
        let mut enc = Vec::new();
        for _ in 0..3 {
            let (_, ns) = encode_all(
                &Config {
                    bitrate: 24_000,
                    frame_ms: 20,
                    complexity,
                    fec_loss: None,
                },
                &mixed,
            );
            enc.push(ns);
        }
        println!(
            "mix encode complexity={complexity} ns_per_frame={:.0}",
            median(enc)
        );
    }
    flush();
}
