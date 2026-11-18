//! The trapezoidal (TPT) state-variable filter, after Zavalishin and
//! Simper. Its states are integrator outputs, so retuning it every block
//! while audio runs neither clicks nor destabilizes it.

use crate::Float;

/// SVF coefficients for a corner `g = tan(π·hz/rate)` and damping `k = 1/Q`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SvfCoeffs<T> {
    pub g: T,
    pub k: T,
    pub a1: T,
    pub a2: T,
    pub a3: T,
}

impl<T: Float> SvfCoeffs<T> {
    /// Coefficients at `hz` (already clamped below Nyquist by the caller)
    /// and damping `k`.
    #[must_use]
    pub fn new(hz: T, k: T, rate: T) -> Self {
        Self::from_g((T::PI * hz / rate).tan(), k)
    }

    /// Coefficients for a pre-warped corner `g`.
    #[must_use]
    pub fn from_g(g: T, k: T) -> Self {
        let a1 = T::ONE / (T::ONE + g * (g + k));
        let a2 = g * a1;
        Self {
            g,
            k,
            a1,
            a2,
            a3: g * a2,
        }
    }
}

/// One tick's outputs. `low` and `band` are the filter's own; the rest
/// are mixes of them with the input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfOut<T> {
    pub input: T,
    pub band: T,
    pub low: T,
    pub k: T,
}

impl<T: Float> SvfOut<T> {
    #[must_use]
    pub fn high(self) -> T {
        self.input - self.k * self.band - self.low
    }

    #[must_use]
    pub fn notch(self) -> T {
        self.input - self.k * self.band
    }

    /// The band-pass scaled to unity gain at the corner whatever the Q.
    #[must_use]
    pub fn band_norm(self) -> T {
        self.k * self.band
    }
}

/// The SVF's two integrator states.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Svf<T> {
    pub ic1: T,
    pub ic2: T,
}

impl<T: Float> Svf<T> {
    /// Filters one sample with `c`.
    #[inline]
    pub fn tick(&mut self, x: T, c: &SvfCoeffs<T>) -> SvfOut<T> {
        let v3 = x - self.ic2;
        let v1 = c.a1 * self.ic1 + c.a2 * v3;
        let v2 = self.ic2 + c.a2 * self.ic1 + c.a3 * v3;
        self.ic1 = T::TWO * v1 - self.ic1;
        self.ic2 = T::TWO * v2 - self.ic2;
        SvfOut {
            input: x,
            band: v1,
            low: v2,
            k: c.k,
        }
    }

    /// Zeroes states that have decayed below `floor`.
    pub fn flush(&mut self, floor: T) {
        crate::flush(&mut self.ic1, floor);
        crate::flush(&mut self.ic2, floor);
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::biquad::{Biquad, cookbook};
    use core::f64::consts::{PI, TAU};

    /// The TPT SVF low-pass and the cookbook low-pass are the same bilinear
    /// transform of one analog prototype, so their impulse responses agree.
    #[test]
    fn low_pass_impulse_matches_the_cookbook_biquad() {
        let (hz, quality, rate) = (2_000.0, 0.9, 48_000.0);
        let c = SvfCoeffs::new(hz, 1.0 / quality, rate);
        let w = TAU * hz / rate;
        let mut bq = Biquad::new(cookbook::low_pass(w.cos(), w.sin() / (2.0 * quality)));
        let mut svf = Svf::default();
        for i in 0..256 {
            let x = if i == 0 { 1.0 } else { 0.0 };
            let (tpt, tdf) = (svf.tick(x, &c).low, bq.process(x));
            assert!((tpt - tdf).abs() < 1e-12, "sample {i}: {tpt} vs {tdf}");
        }
    }

    #[test]
    fn step_settles_low_at_one_and_high_band_at_zero() {
        let c = SvfCoeffs::new(500.0_f32, 1.0, 48_000.0);
        let mut svf = Svf::default();
        let mut out = svf.tick(1.0, &c);
        for _ in 0..48_000 {
            out = svf.tick(1.0, &c);
        }
        assert!((out.low - 1.0).abs() < 1e-5);
        assert!(out.band.abs() < 1e-5 && out.high().abs() < 1e-5);
        assert!((out.notch() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn normalized_band_has_unity_gain_at_the_corner() {
        let (hz, rate) = (1_000.0, 48_000.0);
        let c = SvfCoeffs::new(hz, 1.0 / 8.0, rate);
        let mut svf = Svf::default();
        let mut peak = 0.0_f64;
        for i in 0..48_000 {
            let y = svf
                .tick((TAU * hz * f64::from(i) / rate).sin(), &c)
                .band_norm();
            if i > 24_000 {
                peak = peak.max(y.abs());
            }
        }
        assert!((peak - 1.0).abs() < 1e-3, "{peak}");
        assert!(c.g > 0.0 && c.g < (PI / 2.0).tan());
    }
}
