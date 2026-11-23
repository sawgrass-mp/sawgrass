//! Round-trip and precision tests for the wire format.

use super::*;
use crate::quant::{
    POSITION_LIMIT, POSITION_MAX_ERROR, ROTATION_MAX_ERROR_DEGREES, VELOCITY_LIMIT, VELOCITY_MAX_ERROR,
};

/// Deterministic xorshift generator, so the tests need no dependency and never flake.
struct Rng(u64);

impl Rng {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        // 24 random bits mapped to [-1, 1].
        (self.0 >> 40) as f32 / (1 << 23) as f32 - 1.0
    }
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

#[test]
fn same_version_is_compatible() {
    assert!(is_compatible(PROTOCOL_VERSION));
}

#[test]
fn other_version_is_not_compatible() {
    assert!(!is_compatible(PROTOCOL_VERSION + 1));
}

#[test]
fn header_round_trips_with_payload() {
    let header = PacketHeader {
        kind: PacketKind::Snapshot,
        sequence: 0xBEEF,
        tick: 0x0102_0304,
    };
    let mut buf = [0; PacketHeader::SIZE + 2];
    assert_eq!(header.encode(&mut buf), Ok(PacketHeader::SIZE));
    buf[PacketHeader::SIZE..].copy_from_slice(&[9, 8]);
    assert_eq!(buf[..PacketHeader::SIZE], [1, 0xEF, 0xBE, 4, 3, 2, 1]);
    assert_eq!(PacketHeader::decode(&buf), Ok((header, &[9, 8][..])));
}

#[test]
fn header_rejects_bad_input() {
    assert_eq!(PacketHeader::decode(&[1, 0, 0]), Err(DecodeError::Truncated));
    assert_eq!(PacketHeader::decode(&[0; 7]), Err(DecodeError::UnknownKind(0)));
    assert_eq!(
        PacketHeader::decode(&[200; 7]),
        Err(DecodeError::UnknownKind(200))
    );
    let header = PacketHeader {
        kind: PacketKind::Event,
        sequence: 1,
        tick: 2,
    };
    assert_eq!(header.encode(&mut [0; 6]), Err(EncodeError::BufferTooSmall));
}

#[test]
fn entity_state_round_trips() {
    let state = EntityState {
        id: 4095,
        position: Position::from_metres([-1234.56, 7890.12, 35.5]).unwrap_or_else(|e| panic!("{e}")),
        velocity: Velocity::from_metres_per_second([12.34, -0.01, 0.0]).unwrap_or_else(|e| panic!("{e}")),
        rotation: Rotation::from_quaternion([0.0, 0.0, 0.6, 0.8]).unwrap_or_else(|e| panic!("{e}")),
    };
    let mut buf = [0; EntityState::SIZE];
    assert_eq!(state.encode(&mut buf), Ok(23));
    assert_eq!(EntityState::decode(&buf), Ok((state, &[][..])));
    assert_eq!(EntityState::decode(&buf[..22]), Err(DecodeError::Truncated));
}

#[test]
fn position_codes_are_exact_centimetres() {
    let p = Position::from_metres([1.0, -0.004, 0.006]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(p.cm(), [100, 0, 1]);
    assert_eq!(
        Position::from_metres([-0.016, 0.0, 0.0]).map(|p| p.cm()[0]),
        Ok(-2)
    );
}

#[test]
fn position_error_is_bounded_over_the_whole_range() {
    let mut rng = Rng(0x5EED);
    let mut worst = 0.0_f32;
    for i in 0..200_000 {
        // Half the samples near the origin, half spread over the full range, plus the edges.
        let scale = if i % 2 == 0 { 100.0 } else { POSITION_LIMIT };
        let p = [
            rng.next_f32() * scale,
            rng.next_f32() * scale,
            rng.next_f32() * scale,
        ];
        let q = Position::from_metres(p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let mut buf = [0; Position::SIZE];
        let mut w = Writer::new(&mut buf, Position::SIZE).unwrap_or_else(|e| panic!("{e}"));
        q.write(&mut w);
        let decoded = Position::read(&mut Reader { input: &buf }).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(decoded, q);
        worst = worst.max(max_abs_diff(&p, &decoded.to_metres()));
    }
    for edge in [POSITION_LIMIT, -POSITION_LIMIT] {
        let p = [edge; 3];
        let decoded = Position::from_metres(p)
            .unwrap_or_else(|e| panic!("{e}"))
            .to_metres();
        worst = worst.max(max_abs_diff(&p, &decoded));
    }
    println!("worst position error {worst} m");
    assert!(worst <= POSITION_MAX_ERROR, "worst position error {worst} m");
}

#[test]
fn position_rejects_out_of_range_and_non_finite() {
    assert_eq!(
        Position::from_metres([POSITION_LIMIT + 1.0, 0.0, 0.0]),
        Err(EncodeError::OutOfRange)
    );
    assert_eq!(
        Position::from_metres([0.0, -90_000.0, 0.0]),
        Err(EncodeError::OutOfRange)
    );
    assert_eq!(
        Position::from_metres([0.0, 0.0, f32::NAN]),
        Err(EncodeError::NotFinite)
    );
    assert_eq!(
        Position::from_metres([f32::INFINITY, 0.0, 0.0]),
        Err(EncodeError::NotFinite)
    );
}

#[test]
fn velocity_error_is_bounded() {
    let mut rng = Rng(0xFACE);
    let mut worst = 0.0_f32;
    for _ in 0..100_000 {
        let v = [0; 3].map(|_: i32| rng.next_f32() * VELOCITY_LIMIT);
        let q = Velocity::from_metres_per_second(v).unwrap_or_else(|e| panic!("{v:?}: {e}"));
        worst = worst.max(max_abs_diff(&v, &q.to_metres_per_second()));
    }
    println!("worst velocity error {worst} m/s");
    assert!(worst <= VELOCITY_MAX_ERROR, "worst velocity error {worst} m/s");
    assert_eq!(
        Velocity::from_metres_per_second([400.0, 0.0, 0.0]),
        Err(EncodeError::OutOfRange)
    );
    assert_eq!(
        Velocity::from_metres_per_second([0.0, f32::NAN, 0.0]),
        Err(EncodeError::NotFinite)
    );
}

fn angle_degrees(a: [f32; 4], b: [f32; 4]) -> f32 {
    let dot: f64 = a.iter().zip(&b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum();
    (2.0 * dot.abs().min(1.0).acos()).to_degrees() as f32
}

#[test]
fn rotation_error_is_bounded() {
    let mut rng = Rng(0xC0FFEE);
    let mut worst = 0.0_f32;
    for _ in 0..200_000 {
        let raw = [0; 4].map(|_: i32| rng.next_f32());
        let len = raw.iter().map(|c| c * c).sum::<f32>().sqrt();
        if len < 1e-3 {
            continue;
        }
        let q = raw.map(|c| c / len);
        let decoded = Rotation::from_quaternion(q)
            .unwrap_or_else(|e| panic!("{q:?}: {e}"))
            .to_quaternion();
        let unit = decoded.iter().map(|c| c * c).sum::<f32>();
        assert!(
            (unit - 1.0).abs() < 1e-5,
            "decoded quaternion is not unit: {decoded:?}"
        );
        worst = worst.max(angle_degrees(q, decoded));
    }
    println!("worst rotation error {worst} degrees");
    assert!(
        worst <= ROTATION_MAX_ERROR_DEGREES,
        "worst rotation error {worst} degrees"
    );
}

#[test]
fn rotation_handles_sign_and_axes() {
    let identity = [0.0, 0.0, 0.0, 1.0];
    for q in [
        identity,
        [0.0, 0.0, 0.0, -1.0],
        [1.0, 0.0, 0.0, 0.0],
        [0.0, -1.0, 0.0, 0.0],
    ] {
        let decoded = Rotation::from_quaternion(q)
            .unwrap_or_else(|e| panic!("{e}"))
            .to_quaternion();
        assert!(
            angle_degrees(q, decoded) <= ROTATION_MAX_ERROR_DEGREES,
            "{q:?} -> {decoded:?}"
        );
    }
    // An unnormalized input is the same rotation as its normalized form.
    let scaled = Rotation::from_quaternion([0.0, 0.0, 0.0, 5.0]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        scaled,
        Rotation::from_quaternion(identity).unwrap_or_else(|e| panic!("{e}"))
    );
    assert_eq!(Rotation::from_quaternion([0.0; 4]), Err(EncodeError::OutOfRange));
    assert_eq!(
        Rotation::from_quaternion([f32::NAN, 0.0, 0.0, 1.0]),
        Err(EncodeError::NotFinite)
    );
}

#[test]
fn position_from_cm_checks_the_i24_range() {
    let max = (1 << 23) - 1;
    assert_eq!(
        Position::from_cm([max, -max - 1, 0]).map(Position::cm),
        Ok([max, -max - 1, 0])
    );
    assert_eq!(Position::from_cm([max + 1, 0, 0]), Err(EncodeError::OutOfRange));
    assert_eq!(Position::from_cm([0, 0, -max - 2]), Err(EncodeError::OutOfRange));
}

#[test]
fn rotation_survives_huge_and_tiny_inputs() {
    for scale in [1e20, 1e-20] {
        let q = [1.0, 1.0, 1.0, 2.0].map(|c: f32| c * scale);
        let unit = [1.0, 1.0, 1.0, 2.0].map(|c: f32| c / 7.0_f32.sqrt());
        let decoded = Rotation::from_quaternion(q)
            .unwrap_or_else(|e| panic!("{scale}: {e}"))
            .to_quaternion();
        assert!(
            angle_degrees(unit, decoded) <= ROTATION_MAX_ERROR_DEGREES,
            "{scale}: {decoded:?}"
        );
    }
    let max = Rotation::from_quaternion([f32::MAX; 4])
        .unwrap_or_else(|e| panic!("{e}"))
        .to_quaternion();
    assert!(
        angle_degrees([0.5; 4], max) <= ROTATION_MAX_ERROR_DEGREES,
        "{max:?}"
    );
}

/// Wire bytes come from the network, so every value must decode to something sane.
#[test]
fn arbitrary_wire_bytes_decode_to_valid_values() {
    let mut rng = Rng(0xBAD);
    let specials = [0, u32::MAX, 0x3FFF_FFFF, 0xC000_0000, 0x4000_0000, 0x8000_0000];
    for bits in specials
        .into_iter()
        .chain((0..100_000).map(|_| rng.next_f32().to_bits().wrapping_mul(0x9E37_79B9)))
    {
        let q = Rotation { bits }.to_quaternion();
        assert!(q.iter().all(|c| c.is_finite()), "{bits:#x}: {q:?}");
        let unit = q.iter().map(|c| c * c).sum::<f32>();
        assert!((unit - 1.0).abs() < 1e-5, "{bits:#x}: {q:?}");
    }
    // The decoder accepts -2^23, which the encoder never produces from metres but `from_cm` allows.
    let mut r = Reader {
        input: &[0, 0, 0x80, 0xFF, 0xFF, 0x7F, 0, 0, 0],
    };
    let p = Position::read(&mut r).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(p.cm(), [-(1 << 23), (1 << 23) - 1, 0]);
    assert_eq!(Position::from_cm(p.cm()), Ok(p));
}
