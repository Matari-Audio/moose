//! Envelope followers: the ballistics of level meters and detectors.

use crate::Float;

/// A one-pole follower with separate attack and release steps.
///
/// Feed it a detector value (`|x|`, `x²`, a band level); it rises with
/// `attack` while the input is above it and falls with `release` otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Follower<T> {
    /// Per-sample step while rising, from [`crate::time::coeff`].
    pub attack: T,
    /// Per-sample step while falling.
    pub release: T,
    /// The current envelope.
    pub env: T,
}

impl<T: Float> Follower<T> {
    #[must_use]
    pub fn new(attack: T, release: T) -> Self {
        Self {
            attack,
            release,
            env: T::ZERO,
        }
    }

    /// Follows one detector sample and returns the envelope.
    #[inline]
    pub fn process(&mut self, level: T) -> T {
        let c = if level > self.env {
            self.attack
        } else {
            self.release
        };
        self.env += c * (level - self.env);
        self.env
    }

    pub fn reset(&mut self) {
        self.env = T::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::coeff;

    #[test]
    fn rises_with_attack_and_falls_with_release() {
        let mut f = Follower::new(coeff(10.0_f32), coeff(1_000.0));
        for _ in 0..100 {
            f.process(1.0);
        }
        assert!(f.env > 0.99, "attack: {}", f.env);
        for _ in 0..100 {
            f.process(0.0);
        }
        // 100 samples of a 1000-sample release lose about 10 %.
        assert!((0.88..0.92).contains(&f.env), "release: {}", f.env);
        f.reset();
        assert!(f.env.abs() < f32::EPSILON);
    }

    #[test]
    fn one_attack_step_is_the_coefficient() {
        let mut f = Follower::new(0.25_f64, 0.5);
        assert!((f.process(1.0) - 0.25).abs() < f64::EPSILON);
        assert!((f.process(0.0) - 0.125).abs() < f64::EPSILON);
    }
}
