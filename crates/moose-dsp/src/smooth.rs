//! The one-pole smoother.

use crate::Float;

/// A value gliding toward its target: `y += (x - y)·c` per sample, `c` from
/// [`crate::time::coeff`]. The coefficient is passed per step so one
/// smoother can follow a time constant the caller changes on the fly.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OnePole<T> {
    pub value: T,
}

impl<T: Float> OnePole<T> {
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self { value }
    }

    /// Steps once toward `target` and returns the new value.
    #[inline]
    pub fn step(&mut self, target: T, coeff: T) -> T {
        self.value += (target - self.value) * coeff;
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::coeff_secs;

    #[test]
    fn converges_on_the_target_and_never_overshoots() {
        let c = coeff_secs(0.005_f32, 48_000.0);
        let mut s = OnePole::new(0.0_f32);
        let mut last = 0.0;
        for _ in 0..48_000 / 10 {
            let y = s.step(1.0, c);
            assert!(y >= last && y <= 1.0);
            last = y;
        }
        // In f32 the step stalls once `(1 - y)·c` is under half an ulp of 1:
        // a one-pole lands near its target, never on it.
        assert!((1.0 - last) < 1e-4, "{last}");
    }

    #[test]
    fn a_unit_coefficient_jumps() {
        let mut s = OnePole::new(0.25_f64);
        assert!((s.step(-3.0, 1.0) + 3.0).abs() < f64::EPSILON);
    }
}
