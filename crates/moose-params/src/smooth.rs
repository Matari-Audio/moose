use std::sync::atomic::{AtomicU32, Ordering};

use crate::types::AtomicF64;

/// Smoothing style for a parameter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SmoothingStyle {
    None,
    /// Straight-line ramp over the given milliseconds: a constant
    /// per-sample delta that reaches the target in exactly that time,
    /// whatever the distance. The predictable choice for click-free gain
    /// fades and crossfades where a fixed-length ramp is what you want.
    Linear(f64),
    /// One-pole exponential over the given milliseconds: fast at first,
    /// asymptotic near the target (it lands only within the snap
    /// threshold, never exactly). The natural feel for most controls.
    Exponential(f64),
    /// Multiplicative (log-domain) exponential smoothing over the given
    /// milliseconds. Ramps geometrically rather than additively, so the
    /// perceived rate of change is constant - the right choice for
    /// frequency and linear-gain params where a fixed ratio, not a fixed
    /// delta, reads as "smooth". Requires strictly positive endpoints; a
    /// non-positive `current` or `target` snaps (a log ramp can't cross
    /// or touch zero).
    Logarithmic(f64),
}

/// One parameter's smoothing ramp as plain data, stepped through `&mut self`.
///
/// This is the smoothing rule itself. A processor owns one per smoothed
/// value (usually through [`crate::ParamSmoother`]), so stepping it touches
/// no shared memory. [`Smoother`] is the same rule behind atomics, for
/// reads through a shared `&FloatParam`; both give the same values, bit for
/// bit.
///
/// **Linear ramp state.** `Linear` needs a *constant* per-sample increment
/// to trace a straight line, but the target is handed in afresh on each
/// call, so the increment (`ramp_step`) and the target it was armed for
/// (`ramp_target`) are cached. A step re-arms only when the incoming target
/// differs from `ramp_target`, so a ramp spanning several blocks stays
/// straight; [`Self::snap`] and [`Self::set_sample_rate`] set `ramp_target`
/// to `NaN` to force a re-arm.
#[derive(Clone, Copy, Debug)]
pub struct SmoothState {
    style: SmoothingStyle,
    current: f64,
    coeff: f64,
    /// Constant per-sample increment for the active `Linear` ramp.
    ramp_step: f64,
    /// Target `ramp_step` was armed against; `NaN` forces a re-arm.
    ramp_target: f64,
}

impl SmoothState {
    /// A ramp at rest on `0.0`, with coefficients for `sample_rate`.
    #[must_use]
    pub fn new(style: SmoothingStyle, sample_rate: f64) -> Self {
        Self {
            style,
            current: 0.0,
            coeff: compute_coeff(style, sample_rate),
            ramp_step: 0.0,
            // NaN so the first Linear step arms the ramp from live state.
            ramp_target: f64::NAN,
        }
    }

    /// The smoothing style this ramp follows.
    #[must_use]
    pub fn style(&self) -> SmoothingStyle {
        self.style
    }

    /// Re-derive the per-sample coefficient for `sample_rate`. Doesn't
    /// allocate; safe on the audio thread (e.g. from `reset`).
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.coeff = compute_coeff(self.style, sample_rate);
        // The Linear step size just changed: re-arm on the next step.
        self.ramp_target = f64::NAN;
    }

    /// Jump to `value` with no ramp.
    pub fn snap(&mut self, value: f64) {
        self.current = value;
        // The in-flight Linear increment was sized for another start.
        self.ramp_target = f64::NAN;
    }

    /// Whether the ramp holds `target` exactly, with no work left.
    #[inline]
    #[must_use]
    pub(crate) fn settled_at(&self, target: f64) -> bool {
        self.current.to_bits() == target.to_bits()
    }

    /// Arm (or re-arm) the `Linear` ramp toward `target` and return its
    /// constant per-sample increment.
    ///
    /// A straight-line ramp adds a fixed amount `(target - start) / N`
    /// each sample (`coeff == 1/N`), where `start` is where the ramp
    /// began - *not* the shrinking `diff * coeff`, which decays
    /// geometrically and is indistinguishable from `Exponential`. The
    /// increment is reused until the target changes, so a ramp that spans
    /// several process blocks stays straight and still lands on `target`
    /// in `N` samples total.
    #[inline]
    fn arm_linear_ramp(&mut self, target: f64) -> f64 {
        // Exact compare on purpose: an unchanged target is bit-identical
        // across calls, and a `NaN` armed target never matches.
        #[allow(clippy::float_cmp)]
        if self.ramp_target != target {
            self.ramp_step = (target - self.current) * self.coeff;
            self.ramp_target = target;
        }
        self.ramp_step
    }

    /// Short-circuit for any advance that can't step normally. `None` means
    /// proceed. Returns `Some(v)`:
    /// - when `target` is non-finite: `v = current`, and the ramp is left
    ///   untouched - an author can call `next()` with their own NaN/Inf, and
    ///   letting it reach `current` would latch it;
    /// - else when `current` is non-finite: `v = target` after snapping to
    ///   it, self-healing a NaN/Inf that slipped in (e.g. a corrupt preset).
    ///   It would otherwise latch forever, since `NaN + coeff * (target -
    ///   NaN)` stays NaN and every comparison is false against NaN.
    #[inline]
    fn advance_guard(&mut self, target: f64) -> Option<f64> {
        if !target.is_finite() {
            return Some(self.current);
        }
        if !self.current.is_finite() {
            self.snap(target);
            return Some(target);
        }
        None
    }

    /// The one stepping rule every advance path shares, so `next`,
    /// `next_after` and `next_into` (f32 and f64) agree bit for bit:
    /// step `n` samples toward `target`, calling `emit(i, value)` for each,
    /// and land exactly on `target` once the remaining distance is below
    /// f32 resolution (after every sample, so a per-sample caller and a
    /// block caller snap on the same sample).
    #[inline]
    fn advance(&mut self, target: f64, n: usize, mut emit: impl FnMut(usize, f64)) -> f64 {
        if let Some(v) = self.advance_guard(target) {
            (0..n).for_each(|i| emit(i, v));
            return v;
        }
        if self.settled_at(target) {
            (0..n).for_each(|i| emit(i, target));
            return target;
        }
        let coeff = self.coeff;
        // Linear: scale the convergence threshold to the value magnitude
        // so very-small-range params don't snap prematurely and very
        // large ones (20 kHz cutoffs) don't step through differences
        // nobody hears. Floor at 1e-8 for targets near zero.
        let threshold = (target.abs() * 1e-6).max(1e-8);
        let step = if matches!(self.style, SmoothingStyle::Linear(_)) {
            self.arm_linear_ramp(target)
        } else {
            0.0
        };
        // One-pole ramps otherwise approach forever: once the difference
        // is below f32 resolution, land on the exact target so later
        // blocks take the settled path above.
        let land = (target.abs() * 1e-7).max(1e-12);
        let mut current = self.current;
        for i in 0..n {
            current = match self.style {
                SmoothingStyle::None => target,
                SmoothingStyle::Linear(_) => linear_advance(current, target, step, threshold),
                SmoothingStyle::Exponential(_) => current + coeff * (target - current),
                // One-pole in the log domain: `current *= (target /
                // current)^coeff`. Undefined for a non-positive endpoint,
                // so snap there.
                SmoothingStyle::Logarithmic(_) => {
                    if current <= 0.0 || target <= 0.0 {
                        target
                    } else {
                        (current.ln() + coeff * (target.ln() - current.ln())).exp()
                    }
                }
            };
            if (target - current).abs() <= land {
                current = target;
            }
            emit(i, current);
            // Landed: every later step returns `target` exactly (the land
            // rule catches the log path's `exp(ln(target))` rounding).
            if current.to_bits() == target.to_bits() {
                (i + 1..n).for_each(|j| emit(j, target));
                break;
            }
        }
        self.current = current;
        current
    }

    /// Next smoothed value, advancing one sample.
    // Smoothed param values stay in `[-1e10, 1e10]`; f32 precision
    // is enough for the per-sample DSP path. `next_f64` keeps f64.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn next(&mut self, target: f64) -> f32 {
        self.next_f64(target) as f32
    }

    /// [`Self::next`] at `f64`, with no `f32` rounding anywhere.
    #[inline]
    pub fn next_f64(&mut self, target: f64) -> f64 {
        self.advance(target, 1, |_, _| {})
    }

    /// Advance by `n_samples` in one call, returning only the final
    /// value: exactly what `n_samples` calls to [`Self::next`] return
    /// last. For block-rate consumers (gates, mode switches).
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn next_after(&mut self, target: f64, n_samples: usize) -> f32 {
        self.next_after_f64(target, n_samples) as f32
    }

    /// [`Self::next_after`] at `f64`.
    #[inline]
    pub fn next_after_f64(&mut self, target: f64, n_samples: usize) -> f64 {
        if n_samples == 0 {
            return self.current;
        }
        self.advance(target, n_samples, |_, _| {})
    }

    /// Advance by `out.len()` samples, writing each value: the values
    /// `out.len()` calls to [`Self::next`] would return.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn next_into(&mut self, target: f64, out: &mut [f32]) {
        self.advance(target, out.len(), |i, v| out[i] = v as f32);
    }

    /// [`Self::next_into`] at `f64`.
    #[inline]
    pub fn next_into_f64(&mut self, target: f64, out: &mut [f64]) {
        self.advance(target, out.len(), |i, v| out[i] = v);
    }

    /// Current smoothed value without advancing.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    #[must_use]
    pub fn current(&self) -> f32 {
        self.current as f32
    }

    /// [`Self::current`] at `f64`.
    #[inline]
    #[must_use]
    pub fn current_f64(&self) -> f64 {
        self.current
    }

    /// True when further smoothing toward `target` would be a no-op.
    ///
    /// `SmoothingStyle::None` always returns `true`. For `Linear` /
    /// `Exponential`, the comparison uses the convergence threshold
    /// `(target.abs() * 1e-6).max(1e-8)`. `Logarithmic` converges on a
    /// ratio: `|ln(current) - ln(target)| < 1e-6`, falling back to the
    /// linear threshold for a non-positive endpoint, which steps snap.
    #[inline]
    #[must_use]
    pub fn is_converged(&self, target: f64) -> bool {
        converged(self.style, self.current, target)
    }
}

/// [`SmoothState::is_converged`] on loose values, shared with [`Smoother`].
#[inline]
fn converged(style: SmoothingStyle, current: f64, target: f64) -> bool {
    let linear_converged = || {
        let threshold = (target.abs() * 1e-6).max(1e-8);
        (target - current).abs() < threshold
    };
    match style {
        SmoothingStyle::None => true,
        SmoothingStyle::Linear(_) | SmoothingStyle::Exponential(_) => linear_converged(),
        SmoothingStyle::Logarithmic(_) => {
            if current > 0.0 && target > 0.0 {
                (current.ln() - target.ln()).abs() < 1e-6
            } else {
                linear_converged()
            }
        }
    }
}

/// [`SmoothState`] behind atomics, for smoothed reads through a shared
/// `&FloatParam` (the prelude's `read` / `read_into` / `read_after`).
///
/// Prefer [`crate::ParamSmoother`] in new code: it keeps this state in the
/// processor as plain data. Here every advance loads the state from
/// atomics, steps a [`SmoothState`] copy, and stores it back, so the
/// values match `ParamSmoother`'s bit for bit; the cost is one atomic
/// load/store set per call, which the block paths amortize.
///
/// **Threading.** The audio thread advances it; [`Self::snap`] jumps it
/// from whichever thread applies a value (audio thread on reset / state
/// restore, main thread on activate and host state load). A main-thread
/// `snap` racing an audio-thread advance can be lost, so a preset load may
/// ramp toward the restored target over the next block instead of
/// jumping. That's benign: the target is already published.
pub struct Smoother {
    style: SmoothingStyle,
    current: AtomicF64,
    coeff: AtomicF64,
    sample_rate: AtomicF64,
    ramp_step: AtomicF64,
    ramp_target: AtomicF64,
    /// Bumped by every [`Self::snap`], so a [`crate::ParamSmoother`]
    /// reading the same param knows to jump too.
    snap_generation: AtomicU32,
}

impl Smoother {
    #[must_use]
    pub fn new(style: SmoothingStyle) -> Self {
        // A placeholder sample rate keeps tests that never call
        // `set_sample_rate` producing output; the host sets the real one
        // at activate.
        let state = SmoothState::new(style, 44_100.0);
        Self {
            style,
            current: AtomicF64::new(state.current),
            coeff: AtomicF64::new(state.coeff),
            sample_rate: AtomicF64::new(44_100.0),
            ramp_step: AtomicF64::new(state.ramp_step),
            ramp_target: AtomicF64::new(state.ramp_target),
            snap_generation: AtomicU32::new(0),
        }
    }

    pub fn set_sample_rate(&self, sr: f64) {
        // Compute from the local `sr` and publish `coeff` once, so a
        // concurrent block sees the old coefficient or the new one.
        self.sample_rate.store(sr);
        self.coeff.store(compute_coeff(self.style, sr));
        self.ramp_target.store(f64::NAN);
    }

    /// Snap to a value immediately (used on reset/init and state loads).
    pub fn snap(&self, value: f64) {
        self.current.store(value);
        self.ramp_target.store(f64::NAN);
        self.snap_generation.fetch_add(1, Ordering::Release);
    }

    /// How many times [`Self::snap`] has run. A processor-owned smoother
    /// compares it with the last value it saw to follow host jumps.
    #[inline]
    #[must_use]
    pub fn snap_generation(&self) -> u32 {
        self.snap_generation.load(Ordering::Acquire)
    }

    /// The smoothing style.
    #[must_use]
    pub fn style(&self) -> SmoothingStyle {
        self.style
    }

    /// Whether this smoother holds the target exactly, with no work left.
    #[inline]
    pub(crate) fn settled_at(&self, target: f64) -> bool {
        self.current.load().to_bits() == target.to_bits()
    }

    #[inline]
    fn load(&self) -> SmoothState {
        SmoothState {
            style: self.style,
            current: self.current.load(),
            coeff: self.coeff.load(),
            ramp_step: self.ramp_step.load(),
            ramp_target: self.ramp_target.load(),
        }
    }

    #[inline]
    fn store(&self, state: &SmoothState) {
        self.current.store(state.current);
        self.ramp_step.store(state.ramp_step);
        self.ramp_target.store(state.ramp_target);
    }

    /// Step a plain copy and publish the result.
    #[inline]
    fn step<R>(&self, f: impl FnOnce(&mut SmoothState) -> R) -> R {
        let mut state = self.load();
        let r = f(&mut state);
        self.store(&state);
        r
    }

    /// See [`SmoothState::next`].
    #[inline]
    pub fn next(&self, target: f64) -> f32 {
        self.step(|s| s.next(target))
    }

    /// See [`SmoothState::next_f64`].
    #[inline]
    pub fn next_f64(&self, target: f64) -> f64 {
        self.step(|s| s.next_f64(target))
    }

    /// See [`SmoothState::current`].
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn current(&self) -> f32 {
        self.current.load() as f32
    }

    /// See [`SmoothState::is_converged`]. Costs one atomic load.
    #[inline]
    #[must_use]
    pub fn is_converged(&self, target: f64) -> bool {
        converged(self.style, self.current.load(), target)
    }

    /// See [`SmoothState::current_f64`].
    #[inline]
    #[must_use]
    pub fn current_f64(&self) -> f64 {
        self.current.load()
    }

    /// See [`SmoothState::next_after`].
    #[inline]
    pub fn next_after(&self, target: f64, n_samples: usize) -> f32 {
        self.step(|s| s.next_after(target, n_samples))
    }

    /// See [`SmoothState::next_after_f64`].
    #[inline]
    pub fn next_after_f64(&self, target: f64, n_samples: usize) -> f64 {
        self.step(|s| s.next_after_f64(target, n_samples))
    }

    /// Advance by `N` samples, returning each value in a stack array.
    #[inline]
    pub fn next_block<const N: usize>(&self, target: f64) -> [f32; N] {
        let mut out = [0.0_f32; N];
        self.next_into(target, &mut out);
        out
    }

    /// See [`SmoothState::next_into`].
    #[inline]
    pub fn next_into(&self, target: f64, out: &mut [f32]) {
        self.step(|s| s.next_into(target, out));
    }

    /// See [`SmoothState::next_into_f64`].
    #[inline]
    pub fn next_into_f64(&self, target: f64, out: &mut [f64]) {
        self.step(|s| s.next_into_f64(target, out));
    }
}

/// One `Linear` step: add the constant `step` toward `target`, landing
/// exactly on `target` on the final step - once the remaining distance no
/// longer exceeds one step - or once within the convergence `threshold`.
/// The overshoot clamp is what terminates the ramp on `target` instead of
/// stepping past it, and (with a constant `step`) is the guard the geometric
/// version could never trip.
#[inline]
fn linear_advance(current: f64, target: f64, step: f64, threshold: f64) -> f64 {
    let diff = target - current;
    if diff.abs() < threshold || step.abs() >= diff.abs() {
        target
    } else {
        current + step
    }
}

/// Pure coefficient calculation: smoothing style + sample rate →
/// per-sample step coefficient. Lifted out of `Smoother` so
/// `set_sample_rate` can compute the new coefficient against its
/// local `sr` argument without re-loading any shared state - the
/// audio thread then sees a single atomic publish of `coeff`
/// instead of a two-step (`sample_rate`, `coeff`) write.
fn compute_coeff(style: SmoothingStyle, sr: f64) -> f64 {
    match style {
        SmoothingStyle::None => 1.0,
        SmoothingStyle::Linear(ms) => {
            let samples = (ms / 1000.0) * sr;
            if samples > 1.0 { 1.0 / samples } else { 1.0 }
        }
        // Same one-pole coefficient as `Exponential`; `Logarithmic`
        // applies it in the log domain (see `next`).
        SmoothingStyle::Exponential(ms) | SmoothingStyle::Logarithmic(ms) => {
            let samples = (ms / 1000.0) * sr;
            if samples > 0.0 {
                1.0 - (-1.0 / samples).exp()
            } else {
                1.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_converged_none_always_true() {
        let s = Smoother::new(SmoothingStyle::None);
        assert!(s.is_converged(0.0));
        assert!(s.is_converged(42.0));
        assert!(s.is_converged(-1e6));
    }

    #[test]
    fn is_converged_linear_after_snap() {
        let s = Smoother::new(SmoothingStyle::Linear(5.0));
        s.snap(2.5);
        assert!(s.is_converged(2.5));
        assert!(!s.is_converged(2.6));
    }

    #[test]
    fn is_converged_exponential_at_target() {
        let s = Smoother::new(SmoothingStyle::Exponential(5.0));
        s.snap(1.0);
        assert!(s.is_converged(1.0));
        // Step partway toward 2.0: still smoothing.
        let _ = s.next(2.0);
        assert!(!s.is_converged(2.0));
    }

    #[test]
    fn is_converged_threshold_scales_with_magnitude() {
        // Target near zero: floor at 1e-8.
        let s = Smoother::new(SmoothingStyle::Linear(5.0));
        s.snap(0.0);
        assert!(s.is_converged(1e-9));
        assert!(!s.is_converged(1e-7));

        // Large target: threshold scales by 1e-6.
        s.snap(20_000.0);
        assert!(s.is_converged(20_000.01));
        assert!(!s.is_converged(20_001.0));
    }

    #[test]
    fn next_after_matches_next_block_exponential() {
        // The closed-form path for Exponential should land on the
        // same value the step-by-step `next_block` produces (within
        // f32 rounding).
        const N: usize = 512;
        let stepwise = Smoother::new(SmoothingStyle::Exponential(20.0));
        stepwise.set_sample_rate(48_000.0);
        stepwise.snap(0.0);
        let block = stepwise.next_block::<N>(1.0);

        let closed = Smoother::new(SmoothingStyle::Exponential(20.0));
        closed.set_sample_rate(48_000.0);
        closed.snap(0.0);
        let after = closed.next_after(1.0, N);

        let diff = (block[N - 1] - after).abs();
        assert!(
            diff < 1e-6,
            "block last = {}, after = {}",
            block[N - 1],
            after
        );
    }

    #[test]
    fn next_into_matches_next_block_prefix() {
        // `next_into(&mut [_; n])` must produce the same per-sample
        // sequence as `next_block::<N>` for `i < n`, and must advance
        // the smoother by exactly `n` steps. Regression guard for the
        // bug that motivated `next_into`: callers chunking the host
        // buffer into a `MAX_BLOCK`-sized ladder were calling
        // `next_block::<MAX_BLOCK>` and consuming only `n` samples,
        // which silently advanced the smoother by `MAX_BLOCK` and
        // stepped the value at the next block boundary.
        const FULL: usize = 64;
        const PARTIAL: usize = 17;

        let reference = Smoother::new(SmoothingStyle::Exponential(20.0));
        reference.set_sample_rate(48_000.0);
        reference.snap(0.0);
        let block = reference.next_block::<FULL>(1.0);

        let mut buf = [0.0_f32; FULL];
        let partial = Smoother::new(SmoothingStyle::Exponential(20.0));
        partial.set_sample_rate(48_000.0);
        partial.snap(0.0);
        partial.next_into(1.0, &mut buf[..PARTIAL]);

        for i in 0..PARTIAL {
            let diff = (buf[i] - block[i]).abs();
            assert!(diff < 1e-6, "i={i}, into={}, block={}", buf[i], block[i]);
        }

        // Next sample from `partial` must equal `block[PARTIAL]` —
        // i.e. the smoother is positioned at sample PARTIAL, not at
        // sample FULL.
        let next = partial.next(1.0);
        let diff = (next - block[PARTIAL]).abs();
        assert!(diff < 1e-6, "next={next}, expected={}", block[PARTIAL]);
    }

    #[test]
    fn next_after_matches_next_block_linear() {
        const N: usize = 64;
        let stepwise = Smoother::new(SmoothingStyle::Linear(5.0));
        stepwise.set_sample_rate(48_000.0);
        stepwise.snap(0.0);
        let mut last = 0.0_f32;
        for _ in 0..N {
            last = stepwise.next(1.0);
        }

        let chunked = Smoother::new(SmoothingStyle::Linear(5.0));
        chunked.set_sample_rate(48_000.0);
        chunked.snap(0.0);
        let after = chunked.next_after(1.0, N);

        assert!(
            (last - after).abs() < 1e-6,
            "stepwise = {last}, after = {after}"
        );
    }

    #[test]
    fn linear_ramp_is_straight_and_settles_on_time() {
        // 10 ms at 48 kHz = 480 samples. A true linear ramp 0 -> 1 traces
        // a straight line (constant per-sample delta), passes 0.5 at the
        // half-way sample, and lands on the target at ~480 samples - not
        // the geometric one-pole the shrinking `diff * coeff` used to
        // produce (midpoint ~0.63, and settling ~14x later).
        let s = Smoother::new(SmoothingStyle::Linear(10.0));
        s.set_sample_rate(48_000.0);
        s.snap(0.0);

        let vals: Vec<f64> = (0..480).map(|_| f64::from(s.next(1.0))).collect();

        // Midpoint is linear (~0.5), decisively not the exponential ~0.63.
        let mid = vals[239];
        assert!(
            (mid - 0.5).abs() < 0.01,
            "midpoint {mid} should be ~0.5 (linear), not ~0.63 (exponential)"
        );

        // Every consecutive delta is the same constant ~1/480.
        let expected = 1.0 / 480.0;
        for w in vals.windows(2) {
            let d = w[1] - w[0];
            assert!(
                (d - expected).abs() < 1e-4,
                "step {d} not constant ~{expected}"
            );
        }

        // Reaches the target by the declared time, and stays.
        assert!(
            (vals[479] - 1.0).abs() < 1e-3,
            "should reach target by ~480 samples, got {}",
            vals[479]
        );
    }

    #[test]
    fn linear_ramp_stays_straight_across_blocks() {
        // Regression guard for the constant-step cache: two 100-sample
        // blocks of a 480-sample ramp must continue the same straight
        // line, not re-arm a fresh 480-sample ramp from each block's
        // start value (which would bend the slope at the boundary and
        // stretch the total time).
        let s = Smoother::new(SmoothingStyle::Linear(10.0));
        s.set_sample_rate(48_000.0);
        s.snap(0.0);

        let mut b1 = [0.0_f32; 100];
        let mut b2 = [0.0_f32; 100];
        s.next_into(1.0, &mut b1);
        s.next_into(1.0, &mut b2);

        // After 200 samples the value is ~200/480, i.e. the ramp kept
        // going rather than restarting.
        let after_200 = f64::from(b2[99]);
        assert!(
            (after_200 - 200.0 / 480.0).abs() < 1e-3,
            "after 200 samples got {after_200}, expected {}",
            200.0 / 480.0
        );

        // Slope is unchanged across the block boundary.
        let last_b1 = f64::from(b1[99] - b1[98]);
        let first_b2 = f64::from(b2[0] - b1[99]);
        assert!(
            (last_b1 - first_b2).abs() < 1e-4,
            "slope changed at block boundary: {last_b1} vs {first_b2}"
        );
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn next_after_zero_samples_is_no_op() {
        // n=0 must return current value and leave state untouched.
        // Float equality is the right check here: we want bit-exact
        // identity, not "close enough".
        let s = Smoother::new(SmoothingStyle::Exponential(5.0));
        s.set_sample_rate(48_000.0);
        s.snap(0.25);
        let before = s.current();
        let v = s.next_after(0.99, 0);
        assert_eq!(v, before);
        assert_eq!(s.current(), before);
    }

    #[test]
    fn logarithmic_converges_multiplicatively() {
        let s = Smoother::new(SmoothingStyle::Logarithmic(5.0));
        s.set_sample_rate(48_000.0);
        s.snap(100.0);
        // Ramp toward 1 kHz; the value stays positive the whole way and
        // converges to the target.
        let mut last = 0.0_f32;
        for _ in 0..4096 {
            last = s.next(1000.0);
            assert!(last > 0.0, "log smoothing must stay positive, got {last}");
        }
        assert!((last - 1000.0).abs() < 1.0, "did not converge: {last}");
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn logarithmic_snaps_on_nonpositive_endpoint() {
        // A log ramp can't touch or cross zero, so a non-positive current
        // or target snaps straight to the target.
        let s = Smoother::new(SmoothingStyle::Logarithmic(5.0));
        s.snap(-1.0);
        assert_eq!(s.next(2.0), 2.0);
        s.snap(1.0);
        assert_eq!(s.next(0.0), 0.0);
    }

    #[test]
    fn next_after_matches_next_block_logarithmic() {
        const N: usize = 512;
        let stepwise = Smoother::new(SmoothingStyle::Logarithmic(20.0));
        stepwise.set_sample_rate(48_000.0);
        stepwise.snap(100.0);
        let block = stepwise.next_block::<N>(2000.0);

        let closed = Smoother::new(SmoothingStyle::Logarithmic(20.0));
        closed.set_sample_rate(48_000.0);
        closed.snap(100.0);
        let after = closed.next_after(2000.0, N);

        assert!(
            (block[N - 1] - after).abs() < 1.0,
            "block last = {}, after = {after}",
            block[N - 1]
        );
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn next_after_none_snaps_immediately() {
        let s = Smoother::new(SmoothingStyle::None);
        s.snap(0.0);
        let v = s.next_after(0.7, 1024);
        assert_eq!(v, 0.7);
        assert_eq!(s.current(), 0.7);
    }

    #[test]
    fn next_self_heals_from_non_finite_current() {
        // A NaN accumulator would latch forever without the recovery guard.
        let s = Smoother::new(SmoothingStyle::Exponential(5.0));
        s.snap(f64::NAN);
        let first = s.next(0.5);
        assert!(first.is_finite(), "recovers to a finite value");
        // And keeps converging normally afterward.
        for _ in 0..64 {
            assert!(s.next(0.5).is_finite());
        }
        assert!((s.current() - 0.5).abs() < 1e-3);
    }

    /// The block-rate paths (`next_into`, `next_after`, and `next_block`
    /// via `next_into`) share the same self-heal as `next` - a NaN in the
    /// accumulator can't fill a whole block with NaN.
    #[test]
    fn block_paths_self_heal_from_non_finite_current() {
        for style in [
            SmoothingStyle::Exponential(5.0),
            SmoothingStyle::Linear(5.0),
            SmoothingStyle::Logarithmic(5.0),
        ] {
            let s = Smoother::new(style);
            s.snap(f64::NAN);
            let mut out = [0.0f32; 16];
            s.next_into(0.5, &mut out);
            assert!(out.iter().all(|v| v.is_finite()), "next_into: {style:?}");

            let s = Smoother::new(style);
            s.snap(f64::INFINITY);
            assert!(s.next_after(0.5, 32).is_finite(), "next_after: {style:?}");
        }
    }

    /// A non-finite *target* (an author calling the prelude-exported
    /// `Smoother` with their own NaN) must not poison a healthy
    /// accumulator: bail to the last good value, leave `current` finite.
    #[test]
    #[allow(clippy::float_cmp)]
    fn non_finite_target_bails_without_poisoning() {
        let s = Smoother::new(SmoothingStyle::Exponential(5.0));
        s.snap(0.5);

        assert_eq!(s.next(f64::NAN), 0.5, "next keeps the last value");
        assert!(s.current().is_finite());

        let mut out = [1.0f32; 8];
        s.next_into(f64::NAN, &mut out);
        assert!(out.iter().all(|&v| v == 0.5), "next_into fills last value");
        assert!(s.current().is_finite());

        assert_eq!(s.next_after(f64::INFINITY, 64), 0.5);
        assert!(s.current().is_finite(), "accumulator stays finite");
    }

    /// `next`, `next_into` and `next_after` (f32 and f64) step the same
    /// rule: driven through the same targets in different block shapes,
    /// they produce identical bits on every sample (finding 7).
    /// The processor-owned `SmoothState` and the shared atomic `Smoother`
    /// step the same rule: every path agrees bit for bit across styles,
    /// block shapes, changing targets and a mid-run snap.
    #[test]
    fn owned_state_matches_the_atomic_smoother_bit_for_bit() {
        let styles = [
            SmoothingStyle::None,
            SmoothingStyle::Linear(20.0),
            SmoothingStyle::Exponential(20.0),
            SmoothingStyle::Logarithmic(20.0),
        ];
        #[cfg(not(miri))]
        let blocks = [1_usize, 7, 64, 3, 512, 1, 128, 1000, 33, 2048];
        #[cfg(miri)]
        let blocks = [1_usize, 7, 16, 3, 32, 1, 16, 24, 9, 64];
        for style in styles {
            let shared = Smoother::new(style);
            shared.set_sample_rate(48_000.0);
            shared.snap(100.0);
            let mut owned = SmoothState::new(style, 48_000.0);
            owned.snap(100.0);
            for (k, &n) in blocks.iter().enumerate() {
                let target = [2_000.0, 0.5, 18_000.0, 440.0][k % 4];
                if k == 5 {
                    shared.snap(7.0);
                    owned.snap(7.0);
                }
                let (mut a, mut b) = (vec![0.0_f32; n], vec![0.0_f32; n]);
                match k % 3 {
                    0 => {
                        shared.next_into(target, &mut a);
                        owned.next_into(target, &mut b);
                    }
                    1 => {
                        for i in 0..n {
                            a[i] = shared.next(target);
                            b[i] = owned.next(target);
                        }
                    }
                    _ => {
                        a[n - 1] = shared.next_after(target, n);
                        b[n - 1] = owned.next_after(target, n);
                    }
                }
                assert_smooth_slice_f32(&a, &b, &format!("{style:?} block {k}"));
                assert_smooth_f64(
                    shared.current_f64(),
                    owned.current_f64(),
                    &format!("{style:?} block {k} current"),
                );
            }
        }
    }

    #[cfg(not(miri))]
    fn assert_smooth_f32(left: f32, right: f32, ctx: &str) {
        assert_eq!(left.to_bits(), right.to_bits(), "{ctx}");
    }

    #[cfg(miri)]
    fn assert_smooth_f32(left: f32, right: f32, ctx: &str) {
        // Miri adds small non-deterministic ULP perturbations to transcendental
        // ops (`exp`/`ln`) across calls; allow a tight relative tolerance there
        // while keeping bit-exact checks on native targets.
        let tol = 1e-5 * left.abs().max(right.abs()).max(1.0);
        assert!(
            (left - right).abs() <= tol,
            "{ctx}: left={left} right={right} diff={}",
            (left - right).abs()
        );
    }

    #[cfg(not(miri))]
    fn assert_smooth_f64(left: f64, right: f64, ctx: &str) {
        assert_eq!(left.to_bits(), right.to_bits(), "{ctx}");
    }

    #[cfg(miri)]
    fn assert_smooth_f64(left: f64, right: f64, ctx: &str) {
        let tol = 1e-9 * left.abs().max(right.abs()).max(1.0);
        assert!(
            (left - right).abs() <= tol,
            "{ctx}: left={left} right={right} diff={}",
            (left - right).abs()
        );
    }

    fn assert_smooth_slice_f32(left: &[f32], right: &[f32], ctx: &str) {
        assert_eq!(left.len(), right.len(), "{ctx}");
        for (i, (&a, &b)) in left.iter().zip(right.iter()).enumerate() {
            assert_smooth_f32(a, b, &format!("{ctx} sample {i}"));
        }
    }

    #[test]
    fn every_advance_path_matches_next_bit_for_bit() {
        let styles = [
            SmoothingStyle::None,
            SmoothingStyle::Linear(20.0),
            SmoothingStyle::Exponential(20.0),
            SmoothingStyle::Logarithmic(20.0),
        ];
        #[cfg(not(miri))]
        let blocks = [1_usize, 7, 64, 3, 512, 1, 128, 1000, 33, 2048];
        #[cfg(miri)]
        let blocks = [1_usize, 7, 16, 3, 32, 1, 16, 24, 9, 64];
        for style in styles {
            let mk = || {
                let s = Smoother::new(style);
                s.set_sample_rate(48_000.0);
                s.snap(100.0);
                s
            };
            let (per_sample, into, into64, after, after64) = (mk(), mk(), mk(), mk(), mk());
            for (k, &n) in blocks.iter().enumerate() {
                let target = [2_000.0, 0.5, 18_000.0, 440.0][k % 4];
                let expect: Vec<f64> = (0..n).map(|_| per_sample.next_f64(target)).collect();
                let mut got = vec![0.0_f32; n];
                into.next_into(target, &mut got);
                let mut got64 = vec![0.0_f64; n];
                into64.next_into_f64(target, &mut got64);
                for i in 0..n {
                    #[allow(clippy::cast_possible_truncation)]
                    let e32 = expect[i] as f32;
                    assert_smooth_f32(got[i], e32, &format!("{style:?} block {k} sample {i}"));
                    assert_smooth_f64(
                        got64[i],
                        expect[i],
                        &format!("{style:?} block {k} sample {i}"),
                    );
                }
                let last = expect[n - 1];
                #[allow(clippy::cast_possible_truncation)]
                let last32 = last as f32;
                assert_smooth_f32(
                    after.next_after(target, n),
                    last32,
                    &format!("{style:?} block {k}"),
                );
                assert_smooth_f64(
                    after64.next_after_f64(target, n),
                    last,
                    &format!("{style:?} block {k}"),
                );
            }
        }
    }
}
