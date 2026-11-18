//! Planned complex FFTs over `rustfft` that own their scratch, so the
//! transform itself never allocates.

use std::sync::Arc;

pub use rustfft::FftNum;
pub use rustfft::num_complex::Complex;

/// A complex FFT of one size, forward and inverse.
///
/// Forward uses `exp(-iωt)`; [`Fft::inverse`] uses `exp(+iωt)` and scales
/// by `1/N`, so `inverse(forward(x)) == x`.
pub struct Fft<T: FftNum> {
    forward: Arc<dyn rustfft::Fft<T>>,
    inverse: Arc<dyn rustfft::Fft<T>>,
    scratch: Vec<Complex<T>>,
    scale: T,
}

impl<T: FftNum> Fft<T> {
    /// Plans both directions for `len` points. Allocates.
    #[must_use]
    pub fn new(len: usize) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        let forward = planner.plan_fft_forward(len);
        let inverse = planner.plan_fft_inverse(len);
        let scratch = forward
            .get_inplace_scratch_len()
            .max(inverse.get_inplace_scratch_len());
        Self {
            forward,
            inverse,
            scratch: vec![Complex::new(T::zero(), T::zero()); scratch],
            scale: T::one() / T::from_usize(len.max(1)).unwrap_or_else(T::one),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forward transform of `values` in place.
    ///
    /// # Panics
    /// If `values.len()` is not the planned length.
    pub fn forward(&mut self, values: &mut [Complex<T>]) {
        assert_eq!(
            values.len(),
            self.len(),
            "FFT buffer does not match its plan"
        );
        self.forward.process_with_scratch(values, &mut self.scratch);
    }

    /// Inverse transform of `values` in place, scaled by `1/N`.
    ///
    /// # Panics
    /// If `values.len()` is not the planned length.
    pub fn inverse(&mut self, values: &mut [Complex<T>]) {
        assert_eq!(
            values.len(),
            self.len(),
            "FFT buffer does not match its plan"
        );
        self.inverse.process_with_scratch(values, &mut self.scratch);
        for v in values {
            *v = *v * self.scale;
        }
    }

    /// [`Fft::forward`] or [`Fft::inverse`].
    pub fn transform(&mut self, values: &mut [Complex<T>], inverse: bool) {
        if inverse {
            self.inverse(values);
        } else {
            self.forward(values);
        }
    }
}

impl<T: FftNum> core::fmt::Debug for Fft<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fft")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f64::consts::TAU;

    #[test]
    fn a_bin_centred_sine_lands_in_its_bin() {
        let n = 64;
        let mut fft = Fft::<f64>::new(n);
        let mut x: Vec<Complex<f64>> = (0..n)
            .map(|i| Complex::new((TAU * 5.0 * i as f64 / n as f64).cos(), 0.0))
            .collect();
        fft.forward(&mut x);
        for (bin, v) in x.iter().enumerate() {
            let want = if bin == 5 || bin == n - 5 {
                n as f64 / 2.0
            } else {
                0.0
            };
            assert!((v.norm() - want).abs() < 1e-9, "bin {bin}: {}", v.norm());
        }
    }

    #[test]
    fn inverse_undoes_forward() {
        let mut fft = Fft::<f32>::new(32);
        let src: Vec<Complex<f32>> = (0..32)
            .map(|i| Complex::new((i as f32 * 0.37).sin(), (i as f32 * 0.11).cos()))
            .collect();
        let mut x = src.clone();
        fft.forward(&mut x);
        fft.inverse(&mut x);
        for (a, b) in x.iter().zip(&src) {
            assert!((a - b).norm() < 1e-5);
        }
    }

    #[test]
    fn an_impulse_is_flat() {
        let mut fft = Fft::<f64>::new(16);
        let mut x = vec![Complex::new(0.0, 0.0); 16];
        x[0] = Complex::new(1.0, 0.0);
        fft.forward(&mut x);
        assert!(
            x.iter()
                .all(|v| (v - Complex::new(1.0, 0.0)).norm() < 1e-12)
        );
    }
}
