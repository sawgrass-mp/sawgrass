//! The local player: its states to the server, and corrections from it.
//!
//! The game simulates the local player itself, so its movement is already predicted; the server checks each
//! state and refuses a move that is too fast, too long or off the map, leaving the player where it was last
//! accepted (`sawgrass-server`'s `movement` module). [`LocalPlayer::correct`] takes where the server holds the
//! player: an error larger than [`LocalConfig::snap_distance`] is a snap (the game module teleports the player);
//! a smaller one is spread over the next frames by [`LocalPlayer::frame`], so a small correction looks like a
//! slight drift rather than a jump.

use crate::{Pose, length, sub};

/// How states are paced and corrections applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalConfig {
    /// States sent per second.
    pub send_hz: u32,
    /// An error at least this long, in metres, is corrected at once.
    pub snap_distance: f32,
    /// An error at most this long, in metres, is ignored: quantization and the server's slack cover it.
    pub ignore_distance: f32,
    /// Time constant of a smoothed correction, in seconds: about 63% of the error is gone after this long.
    pub smoothing_secs: f32,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            send_hz: 20,
            snap_distance: 4.0,
            ignore_distance: 0.05,
            smoothing_secs: 0.1,
        }
    }
}

/// What a correction asks of the game module.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Correction {
    /// Close enough: nothing to do.
    None,
    /// Put the local player at this pose now.
    Snap(Pose),
    /// Move the local player by the offsets [`LocalPlayer::frame`] returns over the next frames.
    Smooth,
}

/// The local player's sending and corrections.
#[derive(Debug, Clone)]
pub struct LocalPlayer {
    config: LocalConfig,
    next_send: Option<f64>,
    /// What remains of a smoothed correction, in metres.
    remaining: [f32; 3],
}

impl LocalPlayer {
    /// A local player that has sent nothing.
    #[must_use]
    pub fn new(config: LocalConfig) -> Self {
        Self {
            config,
            next_send: None,
            remaining: [0.0; 3],
        }
    }

    /// Whether a state is due at local time `now`, in seconds. When it is, the next one is scheduled one send
    /// period later (or one period after `now` if the caller fell more than a period behind).
    pub fn due(&mut self, now: f64) -> bool {
        let period = 1.0 / f64::from(self.config.send_hz.max(1));
        match self.next_send {
            Some(t) if now < t => false,
            Some(t) => {
                self.next_send = Some(if now - t > period {
                    now + period
                } else {
                    t + period
                });
                true
            }
            None => {
                self.next_send = Some(now + period);
                true
            }
        }
    }

    /// Takes where the server holds the player, `server`, while the game has it at `current`.
    pub fn correct(&mut self, current: &Pose, server: &Pose) -> Correction {
        let error = sub(server.position, current.position);
        let distance = length(error);
        if distance >= self.config.snap_distance {
            self.remaining = [0.0; 3];
            Correction::Snap(*server)
        } else if distance <= self.config.ignore_distance {
            Correction::None
        } else {
            // A newer correction replaces what is left of an older one: both measure from the same game position.
            self.remaining = error;
            Correction::Smooth
        }
    }

    /// The offset to move the local player by this frame, `dt` seconds after the last one, while a smoothed
    /// correction is in progress.
    pub fn frame(&mut self, dt: f32) -> Option<[f32; 3]> {
        if self.remaining == [0.0; 3] {
            return None;
        }
        let keep = (-dt.max(0.0) / self.config.smoothing_secs.max(1e-3)).exp();
        let mut step = self.remaining.map(|r| r * (1.0 - keep));
        if length(self.remaining) * keep < 0.005 {
            // Within half a centimetre: finish.
            step = self.remaining;
            self.remaining = [0.0; 3];
        } else {
            self.remaining = self.remaining.map(|r| r * keep);
        }
        Some(step)
    }

    /// What remains of a smoothed correction, in metres.
    #[must_use]
    pub fn pending(&self) -> [f32; 3] {
        self.remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paces_states_at_the_send_rate() {
        let mut local = LocalPlayer::new(LocalConfig::default());
        // A 144 Hz frame loop for one second.
        let sent = (0..144).filter(|f| local.due(f64::from(*f) / 144.0)).count();
        assert!((19..=21).contains(&sent), "{sent}");
        // A stall does not cause a burst afterwards.
        assert!(local.due(5.0));
        assert!(!local.due(5.01));
    }

    #[test]
    fn large_errors_snap_small_ones_are_ignored() {
        let mut local = LocalPlayer::new(LocalConfig::default());
        let here = Pose::at([0.0; 3]);
        assert_eq!(
            local.correct(&here, &Pose::at([0.01, 0.0, 0.0])),
            Correction::None
        );
        let far = Pose::at([10.0, 0.0, 0.0]);
        assert_eq!(local.correct(&here, &far), Correction::Snap(far));
        assert_eq!(local.frame(0.016), None);
    }

    #[test]
    fn a_small_error_is_spread_over_frames_and_finishes() {
        let mut local = LocalPlayer::new(LocalConfig::default());
        let here = Pose::at([0.0; 3]);
        assert_eq!(
            local.correct(&here, &Pose::at([1.0, -0.5, 0.0])),
            Correction::Smooth
        );
        let mut moved = [0.0f32; 3];
        let mut frames = 0;
        let mut largest = 0.0f32;
        while let Some(step) = local.frame(1.0 / 60.0) {
            largest = largest.max(length(step));
            for i in 0..3 {
                moved[i] += step[i];
            }
            frames += 1;
            assert!(frames < 120, "never finished");
        }
        assert!(
            (moved[0] - 1.0).abs() < 1e-4 && (moved[1] + 0.5).abs() < 1e-4,
            "{moved:?}"
        );
        // No single frame moves more than a fifth of the error.
        assert!(largest < 0.2 * length([1.0, -0.5, 0.0]), "{largest}");
        assert!(frames > 10, "{frames}");
    }
}
