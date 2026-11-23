//! Quantized encodings of positions, velocities and rotations.
//!
//! | Value | Wire size | Step | Range | Worst-case error |
//! |---|---|---|---|---|
//! | [`Position`] | 9 bytes, three `i24` | 1 cm | ±83 886.07 m per axis | 0.5 cm per axis before `f32` rounding, see below |
//! | [`Velocity`] | 6 bytes, three `i16` | 1 cm/s | ±327.67 m/s per axis | 0.5 cm/s per axis |
//! | [`Rotation`] | 4 bytes, smallest three | 10 bits per component | any unit quaternion | [`ROTATION_MAX_ERROR_DEGREES`] |
//!
//! Positions and velocities round to the nearest step (in `f64`, so exactly), so the error of the integer code is
//! at most half a step. Decoding back to `f32` adds the rounding of `f32` itself, which grows with distance from the origin: half a
//! unit in the last place is 0.5 mm at 10 km and 3.9 mm at the edge of the range. [`POSITION_MAX_ERROR`] covers
//! both over the whole range, and the tests assert it.
//!
//! Encoding fails on a value that is not finite or lies outside the range, instead of clamping it, so a broken
//! or hostile input never turns into a plausible position.

use crate::{DecodeError, EncodeError, Reader, Writer};

/// Size of one position or velocity step, in metres (or metres per second).
pub const STEP: f32 = 0.01;
/// Largest absolute coordinate a [`Position`] can carry, in metres.
pub const POSITION_LIMIT: f32 = I24_MAX as f32 * STEP;
/// Largest absolute component a [`Velocity`] can carry, in metres per second.
pub const VELOCITY_LIMIT: f32 = i16::MAX as f32 * STEP;
/// Worst-case error of a decoded [`Position`] coordinate over the whole range, in metres: half a step plus the
/// `f32` rounding of the decoded value at the edge of the range.
pub const POSITION_MAX_ERROR: f32 = 0.005 + 0.004;
/// Worst-case error of a decoded [`Velocity`] component, in metres per second.
pub const VELOCITY_MAX_ERROR: f32 = 0.005 + 1e-4;
/// Worst-case angle between a rotation and its decoded value, in degrees.
///
/// Each stored component is off by at most half a step, `h = √2 / 1023 / 2 ≈ 0.00069`, so the three stored
/// components `a` are off by `|Δa| ≤ √3·h`. Rebuilding the largest component `w ≥ 1/2` from the unit length moves
/// it by `Δw = -(a·Δa) / w`, which at worst doubles the error perpendicular to the quaternion: `2·√3·h`. The
/// rotation angle is twice that, `4·√3·h ≈ 0.0048 rad ≈ 0.274°`. The tests measure about 0.24°.
pub const ROTATION_MAX_ERROR_DEGREES: f32 = 0.28;

const I24_MAX: i32 = (1 << 23) - 1;
const ROTATION_BITS: u32 = 10;
const ROTATION_MAX_CODE: u32 = (1 << ROTATION_BITS) - 1;
/// Smallest-three components lie in [-1/√2, 1/√2].
const ROTATION_RANGE: f32 = std::f32::consts::FRAC_1_SQRT_2;

fn quantize(value: f32, limit: i32) -> Result<i32, EncodeError> {
    if !value.is_finite() {
        return Err(EncodeError::NotFinite);
    }
    // f64 keeps the division and rounding exact at every code an i24 can hold.
    let code = (f64::from(value) * 100.0).round();
    if code.abs() > f64::from(limit) {
        return Err(EncodeError::OutOfRange);
    }
    Ok(code as i32)
}

/// A world position in metres, sent with 1 cm precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    /// Coordinates in centimetres, always within the `i24` range.
    cm: [i32; 3],
}

impl Position {
    /// Wire size in bytes.
    pub const SIZE: usize = 9;

    /// Quantizes a position given in metres.
    pub fn from_metres(metres: [f32; 3]) -> Result<Self, EncodeError> {
        let mut cm = [0; 3];
        for (out, value) in cm.iter_mut().zip(metres) {
            *out = quantize(value, I24_MAX)?;
        }
        Ok(Self { cm })
    }

    /// A position from coordinates in centimetres. Fails if one does not fit in 24 bits.
    pub fn from_cm(cm: [i32; 3]) -> Result<Self, EncodeError> {
        if cm.iter().any(|c| !(-(I24_MAX + 1)..=I24_MAX).contains(c)) {
            return Err(EncodeError::OutOfRange);
        }
        Ok(Self { cm })
    }

    /// The coordinates in centimetres.
    #[must_use]
    pub fn cm(self) -> [i32; 3] {
        self.cm
    }

    /// The position in metres.
    #[must_use]
    pub fn to_metres(self) -> [f32; 3] {
        self.cm.map(|c| (f64::from(c) / 100.0) as f32)
    }

    pub(crate) fn write(self, w: &mut Writer<'_>) {
        for c in self.cm {
            w.bytes(&c.to_le_bytes()[..3]);
        }
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let mut cm = [0; 3];
        for out in &mut cm {
            let b = r.array::<3>()?;
            // Sign-extend the 24-bit value.
            *out = i32::from_le_bytes([b[0], b[1], b[2], 0]) << 8 >> 8;
        }
        Ok(Self { cm })
    }
}

/// A linear velocity in metres per second, sent with 1 cm/s precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Velocity {
    /// Components in centimetres per second.
    pub cm_per_s: [i16; 3],
}

impl Velocity {
    /// Wire size in bytes.
    pub const SIZE: usize = 6;

    /// Quantizes a velocity given in metres per second.
    pub fn from_metres_per_second(value: [f32; 3]) -> Result<Self, EncodeError> {
        let mut cm_per_s = [0; 3];
        for (out, v) in cm_per_s.iter_mut().zip(value) {
            // The limit fits in i16, so the cast cannot truncate.
            *out = quantize(v, i32::from(i16::MAX))? as i16;
        }
        Ok(Self { cm_per_s })
    }

    /// The velocity in metres per second.
    #[must_use]
    pub fn to_metres_per_second(self) -> [f32; 3] {
        self.cm_per_s.map(|c| (f64::from(c) / 100.0) as f32)
    }

    pub(crate) fn write(self, w: &mut Writer<'_>) {
        for c in self.cm_per_s {
            w.bytes(&c.to_le_bytes());
        }
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let mut cm_per_s = [0; 3];
        for out in &mut cm_per_s {
            *out = i16::from_le_bytes(r.array::<2>()?);
        }
        Ok(Self { cm_per_s })
    }
}

/// A rotation as a unit quaternion `[x, y, z, w]`, sent in 32 bits with the smallest-three encoding: the index
/// of the largest component in the top 2 bits, then the other three in 10 bits each. The largest component is
/// rebuilt from the unit length, with its sign made positive (`q` and `-q` are the same rotation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rotation {
    /// The packed bits as sent.
    pub bits: u32,
}

impl Rotation {
    /// Wire size in bytes.
    pub const SIZE: usize = 4;

    /// Quantizes a quaternion `[x, y, z, w]`. It is normalized first; a zero or non-finite input fails.
    pub fn from_quaternion(q: [f32; 4]) -> Result<Self, EncodeError> {
        if q.iter().any(|c| !c.is_finite()) {
            return Err(EncodeError::NotFinite);
        }
        // f64 so that large finite inputs do not overflow to infinity and tiny ones do not underflow to zero.
        let len = q
            .iter()
            .map(|c| f64::from(*c) * f64::from(*c))
            .sum::<f64>()
            .sqrt();
        if len == 0.0 {
            return Err(EncodeError::OutOfRange);
        }
        let mut largest = 0;
        for i in 1..4 {
            if q[i].abs() > q[largest].abs() {
                largest = i;
            }
        }
        let sign = if q[largest] < 0.0 { -1.0 } else { 1.0 };
        let mut bits = (largest as u32) << (3 * ROTATION_BITS);
        let mut shift = 2 * ROTATION_BITS;
        for (i, c) in q.iter().enumerate() {
            if i == largest {
                continue;
            }
            let unit = ((f64::from(*c) * sign / len) as f32 / ROTATION_RANGE).clamp(-1.0, 1.0);
            let code = ((unit + 1.0) * 0.5 * ROTATION_MAX_CODE as f32).round() as u32;
            bits |= code << shift;
            shift = shift.saturating_sub(ROTATION_BITS);
        }
        Ok(Self { bits })
    }

    /// The rotation as a unit quaternion `[x, y, z, w]`.
    #[must_use]
    pub fn to_quaternion(self) -> [f32; 4] {
        let largest = (self.bits >> (3 * ROTATION_BITS)) as usize;
        let mut q = [0.0; 4];
        let mut shift = 2 * ROTATION_BITS;
        let mut sum = 0.0;
        for (i, out) in q.iter_mut().enumerate() {
            if i == largest {
                continue;
            }
            let code = (self.bits >> shift) & ROTATION_MAX_CODE;
            *out = (code as f32 / ROTATION_MAX_CODE as f32 * 2.0 - 1.0) * ROTATION_RANGE;
            sum += *out * *out;
            shift = shift.saturating_sub(ROTATION_BITS);
        }
        q[largest] = (1.0 - sum).max(0.0).sqrt();
        let len = q.iter().map(|c| c * c).sum::<f32>().sqrt();
        q.map(|c| c / len)
    }

    pub(crate) fn write(self, w: &mut Writer<'_>) {
        w.bytes(&self.bits.to_le_bytes());
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            bits: u32::from_le_bytes(r.array::<4>()?),
        })
    }
}
