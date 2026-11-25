#![forbid(unsafe_code)]
// Sample indices and FFT sizes stay far below 2^23.
#![allow(clippy::cast_precision_loss)]

//! Realtime-safe DSP primitives shared by Matari plugins.
//!
//! Everything allocates, if at all, only when it is built; the per-sample
//! and per-block calls never allocate, lock or panic. Filters and smoothers
//! are generic over [`Float`] (`f32` and `f64`) and do their arithmetic in
//! the order the plugins that share them always did, so moving a plugin onto
//! this crate does not move a single output bit.
//!
//! - [`time`]: one-pole time constants, `exp(-1/τ)` and `1 - exp(-1/τ)`.
//! - [`smooth`]: the one-pole smoother `y += (x - y)·c`.
//! - [`follower`]: attack/release envelope followers (meter ballistics).
//! - [`biquad`]: transposed direct form II sections, cookbook designs and
//!   BS.1770 K-weighting.
//! - [`svf`]: the trapezoidal (TPT) state-variable filter.
//! - [`window`]: Hann windows.
//! - `fft` (feature `fft`): planned complex FFTs with owned scratch.

mod float;

pub mod biquad;
#[cfg(feature = "fft")]
pub mod fft;
pub mod follower;
pub mod heap_array;
pub mod smooth;
pub mod svf;
pub mod time;
pub mod window;

pub use float::Float;
pub use heap_array::HeapArray;

/// Zeroes `x` when it has decayed below `floor`, so rung-out state never
/// feeds subnormals to what follows (the audio thread runs without FTZ).
#[inline]
pub fn flush<T: Float>(x: &mut T, floor: T) {
    if x.abs() < floor {
        *x = T::ZERO;
    }
}
