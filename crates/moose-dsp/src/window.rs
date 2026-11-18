//! Analysis windows. These allocate: build them when the plan is made.

use core::f32::consts::TAU;

/// The periodic Hann window, `0.5 - 0.5·cos(2πi/n)`: the STFT window whose
/// hop-`n/2` copies sum to one.
#[must_use]
pub fn hann(n: usize) -> Vec<f32> {
    let nf = n as f32;
    (0..n)
        .map(|i| 0.5 - 0.5 * (TAU * i as f32 / nf).cos())
        .collect()
}

/// The symmetric Hann window, `0.5 - 0.5·cos(2πi/(n-1))`: zero at both ends.
/// A window of one sample is `[1]`.
#[must_use]
pub fn hann_symmetric(n: usize) -> Vec<f32> {
    if n <= 1 {
        return vec![1.0; n];
    }
    let denom = (n - 1) as f32;
    (0..n)
        .map(|i| 0.5 - 0.5 * (TAU * i as f32 / denom).cos())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_hann_overlap_adds_to_one_at_half_hop() {
        let w = hann(64);
        assert!(w[0].abs() < 1e-7);
        assert!((w[32] - 1.0).abs() < 1e-6);
        for i in 0..32 {
            assert!((w[i] + w[i + 32] - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn symmetric_hann_is_zero_at_both_ends() {
        let w = hann_symmetric(9);
        assert!(w[0].abs() < 1e-7 && w[8].abs() < 1e-7);
        assert!((w[4] - 1.0).abs() < 1e-6);
        assert_eq!(hann_symmetric(1), [1.0]);
        assert!(hann(0).is_empty() && hann_symmetric(0).is_empty());
    }
}
