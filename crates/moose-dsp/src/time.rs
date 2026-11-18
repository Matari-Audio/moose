//! One-pole time constants.
//!
//! A one-pole with time constant `τ` samples has pole `p = exp(-1/τ)`; its
//! step `y += (x - y)·c` uses `c = 1 - p` and covers `1 - 1/e` (63 %) of a
//! step in `τ` samples. The functions take `τ` in samples so each caller
//! keeps the exact product it always formed (`ms * 0.001 * rate`,
//! `rate * seconds`, ...); [`coeff_secs`] is the common `seconds * rate`.

use crate::Float;

/// The pole `exp(-1/τ)` of a one-pole with time constant `tau` samples.
#[inline]
#[must_use]
pub fn pole<T: Float>(tau: T) -> T {
    (-T::ONE / tau).exp()
}

/// The per-sample step `1 - exp(-1/τ)` of a one-pole with time constant
/// `tau` samples. `tau == 0` gives 1 (jump), `tau == ∞` gives 0 (hold).
#[inline]
#[must_use]
pub fn coeff<T: Float>(tau: T) -> T {
    T::ONE - pole(tau)
}

/// [`coeff`] for a time constant of `seconds` at `rate` Hz.
#[inline]
#[must_use]
pub fn coeff_secs<T: Float>(seconds: T, rate: T) -> T {
    coeff(seconds * rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_covers_one_minus_one_over_e_in_tau_samples() {
        let c = coeff_secs(0.01_f64, 48_000.0);
        let mut y = 0.0;
        for _ in 0..480 {
            y += (1.0 - y) * c;
        }
        assert!((y - (1.0 - (-1.0_f64).exp())).abs() < 1e-3, "{y}");
    }

    #[test]
    fn matches_the_hand_written_form_bit_for_bit() {
        for (ms, rate) in [(3.0_f32, 44_100.0), (20.0, 48_000.0), (250.0, 96_000.0)] {
            let by_hand = 1.0 - (-1.0 / (ms * 0.001 * rate)).exp();
            assert_eq!(coeff(ms * 0.001 * rate).to_bits(), by_hand.to_bits());
            assert_eq!(coeff_secs(ms * 0.001, rate).to_bits(), by_hand.to_bits());
        }
    }

    #[test]
    fn degenerate_times_jump_or_hold() {
        assert!((coeff(0.0_f32) - 1.0).abs() < f32::EPSILON);
        assert!(coeff(f32::INFINITY).abs() < f32::EPSILON);
    }
}
