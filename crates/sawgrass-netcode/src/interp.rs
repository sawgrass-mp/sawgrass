//! Remote entities shown between the snapshots that describe them.
//!
//! Each entity keeps the states of its last few snapshots. At a render tick between two of them its position
//! follows a cubic Hermite curve through both positions with both velocities as tangents, so a turn is a curve and
//! not a corner; its rotation is the shortest-path spherical interpolation. A pair of samples too far apart for
//! the time between them (a teleport, a respawn) is not interpolated: the entity jumps to the newer one.
//!
//! When the render tick passes the newest sample (snapshots late or lost) the entity is extrapolated along its
//! last velocity for at most [`InterpConfig::max_extrapolation_secs`], then held. An entity missing from a
//! snapshot has left the visible set: it is shown until the render tick reaches its last sample, then removed.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use sawgrass_protocol::EntityState;

use crate::{Pose, length, sub};

/// How entities are interpolated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InterpConfig {
    /// Longest time an entity is moved past its newest sample, in seconds.
    pub max_extrapolation_secs: f32,
    /// A move between two samples faster than this, in metres per second, is a teleport: no interpolation.
    pub teleport_speed: f32,
    /// A move between two samples shorter than this, in metres, is never a teleport.
    pub teleport_min_distance: f32,
}

impl Default for InterpConfig {
    fn default() -> Self {
        Self {
            max_extrapolation_secs: 0.25,
            // The server's movement limit (`MovementRules::max_speed`) with room for quantization and jitter.
            teleport_speed: 200.0,
            teleport_min_distance: 5.0,
        }
    }
}

/// How a sampled pose was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Between two samples.
    Interpolated,
    /// Past the newest sample, moved along its velocity.
    Extrapolated,
    /// Past the extrapolation limit, held where it stopped.
    Held,
    /// At the newer sample of a teleport.
    Snapped,
}

/// One remote entity at the render tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// The entity id.
    pub id: u32,
    /// Where to show it.
    pub pose: Pose,
    /// How the pose was obtained.
    pub kind: Kind,
}

/// Samples kept per entity. More than enough for the longest render delay the clock allows.
const KEPT: usize = 12;

#[derive(Debug, Default)]
struct Track {
    /// (tick, pose), ascending by tick.
    samples: Vec<(u32, Pose)>,
    /// The tick of the first snapshot without this entity, once it left.
    left: Option<u32>,
}

/// The remote entities and their recent samples.
#[derive(Debug, Default)]
pub struct Interpolator {
    config: InterpConfig,
    tick_hz: f32,
    tracks: HashMap<u32, Track>,
    newest: Option<u32>,
}

impl Interpolator {
    /// An interpolator for a server running at `tick_hz`.
    #[must_use]
    pub fn new(tick_hz: u16, config: InterpConfig) -> Self {
        Self {
            config,
            tick_hz: f32::from(tick_hz.max(1)),
            tracks: HashMap::new(),
            newest: None,
        }
    }

    /// Entities tracked, including those that left but are still shown.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Whether no entity is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Takes the whole snapshot for `tick`: every visible entity's state. A snapshot older than the newest one
    /// taken is ignored. `skip` is left out (the local player's own entity).
    pub fn insert(&mut self, tick: u32, states: &[EntityState], skip: Option<u32>) {
        if self.newest.is_some_and(|n| tick <= n) {
            return;
        }
        self.newest = Some(tick);
        for state in states {
            if Some(state.id) == skip {
                continue;
            }
            let track = match self.tracks.entry(state.id) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(Track::default()),
            };
            if track.left.is_some() {
                // Left and came back: start again rather than interpolate across the gap.
                *track = Track::default();
            }
            if track.samples.len() == KEPT {
                track.samples.remove(0);
            }
            track.samples.push((tick, Pose::from_state(state)));
        }
        for track in self.tracks.values_mut() {
            if track.left.is_none() && track.samples.last().is_some_and(|&(t, _)| t < tick) {
                track.left = Some(tick);
            }
        }
    }

    /// Writes every entity shown at `render` (a fractional tick) to `out`, cleared first, ascending by id. Drops
    /// samples no longer needed and entities that have left by then.
    pub fn sample(&mut self, render: f64, out: &mut Vec<Sample>) {
        out.clear();
        let config = self.config;
        let tick_hz = self.tick_hz;
        self.tracks.retain(|&id, track| {
            // A leaver is gone once the render tick reaches the snapshot it was missing from.
            if track.left.is_some_and(|l| render >= f64::from(l)) {
                return false;
            }
            // Keep one sample at or before the render tick, and all after it.
            let before = track.samples.iter().rposition(|&(t, _)| f64::from(t) <= render);
            if let Some(i) = before {
                track.samples.drain(..i);
            }
            if let Some(sample) = pose_at(&track.samples, render, tick_hz, &config) {
                out.push(Sample { id, ..sample });
            }
            true
        });
        out.sort_unstable_by_key(|s| s.id);
    }
}

/// The pose at `render` from samples whose first is at or before it. `None` if the entity is not visible yet.
fn pose_at(samples: &[(u32, Pose)], render: f64, tick_hz: f32, config: &InterpConfig) -> Option<Sample> {
    let &(t0, p0) = samples.first()?;
    if f64::from(t0) > render {
        return None;
    }
    let sample = |pose, kind| Some(Sample { id: 0, pose, kind });
    let Some(&(t1, p1)) = samples.get(1) else {
        let secs = ((render - f64::from(t0)) as f32 / tick_hz).max(0.0);
        let limit = config.max_extrapolation_secs;
        let kind = if secs > limit {
            Kind::Held
        } else {
            Kind::Extrapolated
        };
        let secs = secs.min(limit);
        let mut pose = p0;
        for (p, v) in pose.position.iter_mut().zip(p0.velocity) {
            *p += v * secs;
        }
        return sample(pose, kind);
    };
    let dt = (t1 - t0) as f32 / tick_hz;
    let distance = length(sub(p1.position, p0.position));
    if distance > config.teleport_min_distance && distance > config.teleport_speed * dt {
        return sample(p1, Kind::Snapped);
    }
    let s = ((render - f64::from(t0)) / f64::from(t1 - t0)) as f32;
    let (position, velocity) = hermite(&p0, &p1, dt, distance, s);
    sample(
        Pose {
            position,
            velocity,
            rotation: slerp(p0.rotation, p1.rotation, s),
        },
        Kind::Interpolated,
    )
}

/// Position and velocity at `s` in `[0, 1]` on the Hermite curve from `a` to `b`, `dt` seconds apart. Falls back to
/// a straight line when the velocities do not fit the move (a reported velocity far longer than the distance
/// covered would make the curve overshoot).
fn hermite(a: &Pose, b: &Pose, dt: f32, distance: f32, s: f32) -> ([f32; 3], [f32; 3]) {
    let m0 = a.velocity.map(|v| v * dt);
    let m1 = b.velocity.map(|v| v * dt);
    let bound = 3.0 * distance + 0.1;
    if length(m0) > bound || length(m1) > bound || dt <= 0.0 {
        let mut position = [0.0; 3];
        let mut velocity = [0.0; 3];
        for i in 0..3 {
            position[i] = a.position[i] + (b.position[i] - a.position[i]) * s;
            velocity[i] = if dt > 0.0 {
                (b.position[i] - a.position[i]) / dt
            } else {
                b.velocity[i]
            };
        }
        return (position, velocity);
    }
    let (s2, s3) = (s * s, s * s * s);
    let (h00, h10, h01, h11) = (
        2.0 * s3 - 3.0 * s2 + 1.0,
        s3 - 2.0 * s2 + s,
        -2.0 * s3 + 3.0 * s2,
        s3 - s2,
    );
    // Derivatives with respect to s; divided by dt for metres per second.
    let (d00, d10, d01, d11) = (
        6.0 * s2 - 6.0 * s,
        3.0 * s2 - 4.0 * s + 1.0,
        -6.0 * s2 + 6.0 * s,
        3.0 * s2 - 2.0 * s,
    );
    let mut position = [0.0; 3];
    let mut velocity = [0.0; 3];
    for i in 0..3 {
        position[i] = h00 * a.position[i] + h10 * m0[i] + h01 * b.position[i] + h11 * m1[i];
        velocity[i] = (d00 * a.position[i] + d10 * m0[i] + d01 * b.position[i] + d11 * m1[i]) / dt;
    }
    (position, velocity)
}

/// Spherical interpolation from `a` to `b` along the shorter arc.
pub(crate) fn slerp(a: [f32; 4], b: [f32; 4], s: f32) -> [f32; 4] {
    let mut dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let b = if dot < 0.0 {
        dot = -dot;
        b.map(|c| -c)
    } else {
        b
    };
    let (wa, wb) = if dot > 0.9995 {
        // Nearly the same rotation: a straight line, normalized below, is exact enough.
        (1.0 - s, s)
    } else {
        let theta = dot.min(1.0).acos();
        let sin = theta.sin();
        (((1.0 - s) * theta).sin() / sin, (s * theta).sin() / sin)
    };
    let q = [0, 1, 2, 3].map(|i| wa * a[i] + wb * b[i]);
    let len = q.iter().map(|c| c * c).sum::<f32>().sqrt();
    q.map(|c| c / len)
}

#[cfg(test)]
mod tests {
    use sawgrass_protocol::{Position, Rotation, Velocity};

    use super::*;

    fn state(id: u32, position: [f32; 3], velocity: [f32; 3]) -> EntityState {
        EntityState {
            id,
            position: Position::from_metres(position).expect("position"),
            velocity: Velocity::from_metres_per_second(velocity).expect("velocity"),
            rotation: Rotation::from_quaternion([0.0, 0.0, 0.0, 1.0]).expect("rotation"),
        }
    }

    fn one(interp: &mut Interpolator, render: f64) -> Option<Sample> {
        let mut out = Vec::new();
        interp.sample(render, &mut out);
        assert!(out.len() <= 1);
        out.pop()
    }

    #[test]
    fn straight_motion_is_exact_between_samples() {
        let mut interp = Interpolator::new(20, InterpConfig::default());
        for tick in 0..4u32 {
            let x = tick as f32 * 0.25;
            interp.insert(tick, &[state(7, [x, 0.0, 0.0], [5.0, 0.0, 0.0])], None);
        }
        let s = one(&mut interp, 1.5).expect("shown");
        assert_eq!(s.id, 7);
        assert_eq!(s.kind, Kind::Interpolated);
        assert!((s.pose.position[0] - 0.375).abs() < 1e-3, "{:?}", s.pose);
        assert!((s.pose.velocity[0] - 5.0).abs() < 1e-2, "{:?}", s.pose);
    }

    #[test]
    fn not_shown_before_its_first_sample() {
        let mut interp = Interpolator::new(20, InterpConfig::default());
        interp.insert(10, &[state(1, [0.0; 3], [0.0; 3])], None);
        assert_eq!(one(&mut interp, 9.5), None);
        assert!(one(&mut interp, 10.0).is_some());
    }

    #[test]
    fn extrapolates_for_a_while_then_holds() {
        let mut interp = Interpolator::new(20, InterpConfig::default());
        interp.insert(0, &[state(1, [0.0; 3], [4.0, 0.0, 0.0])], None);
        let s = one(&mut interp, 2.0).expect("shown");
        assert_eq!(s.kind, Kind::Extrapolated);
        assert!((s.pose.position[0] - 0.4).abs() < 1e-3);
        // 0.25 s at most: 1 m.
        let s = one(&mut interp, 20.0).expect("shown");
        assert_eq!(s.kind, Kind::Held);
        assert!((s.pose.position[0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn a_teleport_jumps() {
        let mut interp = Interpolator::new(20, InterpConfig::default());
        interp.insert(0, &[state(1, [0.0; 3], [0.0; 3])], None);
        interp.insert(1, &[state(1, [500.0, 0.0, 0.0], [0.0; 3])], None);
        let s = one(&mut interp, 0.1).expect("shown");
        assert_eq!(s.kind, Kind::Snapped);
        assert_eq!(s.pose.position[0], 500.0);
    }

    #[test]
    fn a_leaver_is_removed_once_the_render_tick_reaches_its_absence() {
        let mut interp = Interpolator::new(20, InterpConfig::default());
        interp.insert(
            0,
            &[state(1, [0.0; 3], [0.0; 3]), state(2, [1.0; 3], [0.0; 3])],
            None,
        );
        interp.insert(1, &[state(1, [0.0; 3], [0.0; 3])], None);
        let mut out = Vec::new();
        interp.sample(0.5, &mut out);
        assert_eq!(out.iter().map(|s| s.id).collect::<Vec<_>>(), [1, 2]);
        interp.sample(1.0, &mut out);
        assert_eq!(out.iter().map(|s| s.id).collect::<Vec<_>>(), [1]);
        assert_eq!(interp.len(), 1);
    }

    #[test]
    fn skips_the_local_player_and_old_snapshots() {
        let mut interp = Interpolator::new(20, InterpConfig::default());
        interp.insert(
            5,
            &[state(1, [0.0; 3], [0.0; 3]), state(9, [0.0; 3], [0.0; 3])],
            Some(9),
        );
        interp.insert(4, &[state(3, [0.0; 3], [0.0; 3])], None);
        let mut out = Vec::new();
        interp.sample(5.0, &mut out);
        assert_eq!(out.iter().map(|s| s.id).collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn a_turn_is_a_curve_through_both_samples() {
        let mut interp = Interpolator::new(20, InterpConfig::default());
        // Moving +x at 10 m/s, then +y: the curve bulges past the chord, towards the corner.
        interp.insert(0, &[state(1, [0.0; 3], [10.0, 0.0, 0.0])], None);
        interp.insert(1, &[state(1, [0.5, 0.5, 0.0], [0.0, 10.0, 0.0])], None);
        let mid = one(&mut interp, 0.5).expect("shown").pose.position;
        assert!(mid[0] > 0.25 + 0.05 && mid[1] < 0.25 - 0.05, "{mid:?}");
        let end = one(&mut interp, 1.0).expect("shown").pose.position;
        assert!((end[0] - 0.5).abs() < 1e-3 && (end[1] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn slerp_takes_the_short_way_and_stays_unit() {
        let half = std::f32::consts::FRAC_1_SQRT_2;
        let a = [0.0, 0.0, 0.0, 1.0];
        // 90 degrees about z, given with the opposite sign.
        let b = [0.0, 0.0, -half, -half];
        let q = slerp(a, b, 0.5);
        let len: f32 = q.iter().map(|c| c * c).sum();
        assert!((len - 1.0).abs() < 1e-5);
        let angle = 2.0 * q[3].abs().acos();
        assert!((angle - std::f32::consts::FRAC_PI_4).abs() < 1e-3, "{q:?}");
    }
}
