//! Second-order sections: coefficients, the transposed direct form II
//! filter, the Audio EQ Cookbook designs and BS.1770 K-weighting.

use crate::Float;

/// Normalized biquad coefficients (`a0 = 1`):
/// `H(z) = (b0 + b1 z⁻¹ + b2 z⁻²) / (1 + a1 z⁻¹ + a2 z⁻²)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coeffs<T> {
    pub b0: T,
    pub b1: T,
    pub b2: T,
    pub a1: T,
    pub a2: T,
}

impl<T: Float> Default for Coeffs<T> {
    /// The identity section.
    fn default() -> Self {
        Self {
            b0: T::ONE,
            b1: T::ZERO,
            b2: T::ZERO,
            a1: T::ZERO,
            a2: T::ZERO,
        }
    }
}

impl<T: Float> Coeffs<T> {
    /// Divides `[b0, b1, b2, a0, a1, a2]` through by `a0`, which must not be 0.
    #[must_use]
    pub fn normalize([b0, b1, b2, a0, a1, a2]: [T; 6]) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// `[b0, b1, b2, a1, a2]`.
    #[must_use]
    pub const fn to_array(self) -> [T; 5] {
        [self.b0, self.b1, self.b2, self.a1, self.a2]
    }
}

impl Coeffs<f64> {
    /// Rounds each coefficient to `f32` (design in `f64`, run in `f32`).
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn to_f32(self) -> Coeffs<f32> {
        Coeffs {
            b0: self.b0 as f32,
            b1: self.b1 as f32,
            b2: self.b2 as f32,
            a1: self.a1 as f32,
            a2: self.a2 as f32,
        }
    }

    /// Linear gain at the normalized angular frequency `w = 2π·hz/rate`.
    #[must_use]
    pub fn magnitude(self, w: f64) -> f64 {
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let num = (self.b0 + self.b1 * c1 + self.b2 * c2).hypot(self.b1 * s1 + self.b2 * s2);
        let den = (1.0 + self.a1 * c1 + self.a2 * c2).hypot(self.a1 * s1 + self.a2 * s2);
        num / den
    }
}

impl From<Coeffs<f32>> for Coeffs<f64> {
    fn from(c: Coeffs<f32>) -> Self {
        Self {
            b0: c.b0.into(),
            b1: c.b1.into(),
            b2: c.b2.into(),
            a1: c.a1.into(),
            a2: c.a2.into(),
        }
    }
}

/// Which side of its corner a shelf moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Low,
    High,
}

/// The Audio EQ Cookbook (R. Bristow-Johnson) designs.
///
/// Each takes `cos = cos(w)` and `alpha` for the section's corner
/// `w = 2π·hz/rate`, so the caller keeps its own clamping and its own form
/// of `alpha` (`sin(w)/(2Q)`, the shelf-slope form, ...); `a` is
/// `10^(dB/40)`.
#[allow(clippy::manual_midpoint)] // `(1 ± cos)/2` as the cookbook (and every caller) writes it.
pub mod cookbook {
    use super::{Coeffs, Side};

    /// The second-order low-pass.
    #[must_use]
    pub fn low_pass(cos: f64, alpha: f64) -> Coeffs<f64> {
        Coeffs::normalize([
            (1.0 - cos) / 2.0,
            1.0 - cos,
            (1.0 - cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ])
    }

    /// The second-order high-pass.
    #[must_use]
    pub fn high_pass(cos: f64, alpha: f64) -> Coeffs<f64> {
        Coeffs::normalize([
            (1.0 + cos) / 2.0,
            -(1.0 + cos),
            (1.0 + cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ])
    }

    /// The peaking (bell) section.
    #[must_use]
    pub fn bell(cos: f64, alpha: f64, a: f64) -> Coeffs<f64> {
        Coeffs::normalize([
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        ])
    }

    /// A shelf on `side`; the high shelf is the low one with `cos` negated
    /// and `b1`, `a1` flipped.
    #[must_use]
    pub fn shelf(cos: f64, alpha: f64, a: f64, side: Side) -> Coeffs<f64> {
        let (c, sign) = match side {
            Side::Low => (cos, 1.0),
            Side::High => (-cos, -1.0),
        };
        let root = 2.0 * a.sqrt() * alpha;
        let (p, m) = (a + 1.0, a - 1.0);
        Coeffs::normalize([
            a * (p - m * c + root),
            sign * 2.0 * a * (m - p * c),
            a * (p - m * c - root),
            p + m * c + root,
            sign * -2.0 * (m + p * c),
            p + m * c - root,
        ])
    }
}

/// ITU-R BS.1770 K-weighting at `rate` Hz: the head shelf, then the RLB
/// high-pass. The standard's 48 kHz tables are this pre-warped derivation
/// (Brecht De Man's prototype form) evaluated at 48 kHz.
#[must_use]
pub fn k_weighting(rate: f64) -> [Coeffs<f64>; 2] {
    use core::f64::consts::PI;
    let (f0, gain_db, q) = (
        1_681.974_450_955_533,
        3.999_843_853_973_347,
        0.707_175_236_955_419_6,
    );
    let k = (PI * f0 / rate).tan();
    let vh = 10f64.powf(gain_db / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Coeffs {
        b0: (vh + vb * k / q + k * k) / a0,
        b1: 2.0 * (k * k - vh) / a0,
        b2: (vh - vb * k / q + k * k) / a0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / q + k * k) / a0,
    };
    let (f0, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
    let k = (PI * f0 / rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Coeffs {
        b0: 1.0,
        b1: -2.0,
        b2: 1.0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / q + k * k) / a0,
    };
    [shelf, high_pass]
}

/// A biquad in transposed direct form II. The default is the identity
/// section at rest.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Biquad<T> {
    pub coeffs: Coeffs<T>,
    pub z1: T,
    pub z2: T,
}

impl<T: Float> Default for Biquad<T> {
    fn default() -> Self {
        Self::new(Coeffs::default())
    }
}

impl<T: Float> Biquad<T> {
    #[must_use]
    pub fn new(coeffs: Coeffs<T>) -> Self {
        Self {
            coeffs,
            z1: T::ZERO,
            z2: T::ZERO,
        }
    }

    /// Filters one sample.
    #[inline]
    pub fn process(&mut self, x: T) -> T {
        let Coeffs { b0, b1, b2, a1, a2 } = self.coeffs;
        let y = b0 * x + self.z1;
        self.z1 = b1 * x - a1 * y + self.z2;
        self.z2 = b2 * x - a2 * y;
        y
    }

    /// Zeroes state words that have decayed below `floor`.
    pub fn flush(&mut self, floor: T) {
        crate::flush(&mut self.z1, floor);
        crate::flush(&mut self.z2, floor);
    }

    pub fn reset(&mut self) {
        self.z1 = T::ZERO;
        self.z2 = T::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f64::consts::TAU;

    fn impulse_response(c: Coeffs<f64>, n: usize) -> Vec<f64> {
        let mut f = Biquad::new(c);
        (0..n)
            .map(|i| f.process(if i == 0 { 1.0 } else { 0.0 }))
            .collect()
    }

    /// The impulse response of a TDF-II section is its difference equation's.
    #[test]
    fn impulse_response_follows_the_difference_equation() {
        let c = Coeffs::normalize([0.3, 0.2, 0.1, 1.0, -0.5, 0.25]);
        let h = impulse_response(c, 6);
        let mut expect = vec![0.0; 6];
        for n in 0..6 {
            let x = |k: usize| if n == k { 1.0 } else { 0.0 };
            let y = |k: usize| if n >= k { expect[n - k] } else { 0.0 };
            expect[n] = c.b0 * x(0) + c.b1 * x(1) + c.b2 * x(2) - c.a1 * y(1) - c.a2 * y(2);
        }
        for (a, b) in h.iter().zip(&expect) {
            assert!((a - b).abs() < 1e-15, "{h:?} vs {expect:?}");
        }
    }

    #[test]
    fn low_pass_passes_dc_and_settles_a_step_at_unity() {
        let w = TAU * 1_000.0 / 48_000.0;
        let c = cookbook::low_pass(w.cos(), w.sin() / (2.0 * core::f64::consts::FRAC_1_SQRT_2));
        assert!((c.magnitude(1e-9) - 1.0).abs() < 1e-9);
        assert!((c.magnitude(w) - core::f64::consts::FRAC_1_SQRT_2).abs() < 1e-9);
        let mut f = Biquad::new(c);
        let y = (0..4_800)
            .map(|_| f.process(1.0))
            .last()
            .unwrap_or_default();
        assert!((y - 1.0).abs() < 1e-9, "{y}");
    }

    #[test]
    fn high_pass_blocks_dc() {
        let w = TAU * 100.0 / 48_000.0;
        let c = cookbook::high_pass(w.cos(), w.sin() / 2.0);
        assert!(c.magnitude(1e-6) < 1e-8);
        assert!((c.magnitude(core::f64::consts::PI) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn bell_and_shelves_reach_their_gain() {
        let a = 10f64.powf(6.0 / 40.0);
        let gain = 10f64.powf(6.0 / 20.0);
        let w = TAU * 1_000.0 / 48_000.0;
        let alpha = w.sin() / 2.0;
        assert!((cookbook::bell(w.cos(), alpha, a).magnitude(w) - gain).abs() < 1e-9);
        let low = cookbook::shelf(w.cos(), alpha, a, Side::Low);
        let high = cookbook::shelf(w.cos(), alpha, a, Side::High);
        assert!((low.magnitude(1e-9) - gain).abs() < 1e-6);
        assert!((low.magnitude(core::f64::consts::PI) - 1.0).abs() < 1e-6);
        assert!((high.magnitude(core::f64::consts::PI) - gain).abs() < 1e-6);
        assert!((high.magnitude(1e-9) - 1.0).abs() < 1e-6);
    }

    /// ITU-R BS.1770-4 Tables 1 and 2 at 48 kHz.
    #[test]
    fn k_weighting_matches_the_standard_at_48k() {
        let [shelf, hp] = k_weighting(48_000.0);
        let table = [
            [
                1.535_124_859_586_97,
                -2.691_696_189_406_38,
                1.198_392_810_852_85,
            ],
            [-1.690_659_293_182_41, 0.732_480_774_215_85, 0.0],
        ];
        for (got, want) in [shelf.b0, shelf.b1, shelf.b2].iter().zip(table[0]) {
            assert!((got - want).abs() < 1e-12, "{got} vs {want}");
        }
        assert!((shelf.a1 - table[1][0]).abs() < 1e-12);
        assert!((shelf.a2 - table[1][1]).abs() < 1e-12);
        assert!((hp.a1 + 1.990_047_454_833_98).abs() < 1e-12);
        assert!((hp.a2 - 0.990_072_250_366_21).abs() < 1e-12);
    }

    #[test]
    fn flush_zeroes_only_subnormal_state() {
        let mut f = Biquad::new(Coeffs::<f32>::default());
        f.z1 = 1e-25;
        f.z2 = 1e-3;
        f.flush(1e-20);
        assert!(f.z1 == 0.0 && (f.z2 - 1e-3).abs() < f32::EPSILON);
    }
}
