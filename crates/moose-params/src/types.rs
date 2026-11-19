use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};

use crate::catalog::{CatalogChange, CatalogChanges, CatalogSignal};
use crate::info::ParamInfo;
use crate::range::ParamRange;
use crate::sample::Float;
use crate::smooth::{SmoothState, Smoother, SmoothingStyle};

/// Metadata accessors shared by every parameter type.
///
/// Each instance borrows its `&'static ParamInfo` from the declaring
/// struct's table (emitted once by `#[derive(Params)]`) and carries
/// only its own id, which nesting rebases per instance.
macro_rules! shared_param_meta {
    () => {
        /// Changed writes counted on this field's counter: the owning
        /// derived tree's counter once attached, else the field's own.
        #[doc(hidden)]
        pub fn parameter_edit_epoch(&self) -> u64 {
            self.edit_epoch.load(Ordering::Acquire)
        }

        #[doc(hidden)]
        pub fn set_edit_epoch(&mut self, epoch: Arc<AtomicU64>) {
            self.edit_epoch = epoch;
        }

        /// Count a changed value. The counter belongs to one instance, so
        /// instances never contend on it.
        #[inline]
        fn note_edit(&self, changed: bool) {
            if changed {
                self.edit_epoch.fetch_add(1, Ordering::Release);
            }
        }

        /// Parameter metadata carrying this instance's (rebased) id.
        #[must_use]
        pub fn info(&self) -> ParamInfo {
            ParamInfo {
                id: self.id,
                ..*self.info
            }
        }

        /// The declaring struct's static metadata. Its `id` is the
        /// struct-local id; use [`Self::id`] for the id the host sees.
        #[must_use]
        pub fn static_info(&self) -> &'static ParamInfo {
            self.info
        }

        /// The declared value range.
        #[must_use]
        #[inline]
        pub fn range(&self) -> &'static ParamRange {
            &self.info.range
        }

        /// Parameter ID.
        #[must_use]
        #[inline]
        pub fn id(&self) -> u32 {
            self.id
        }

        /// Internal: fold a nested group's base into the id. Called by
        /// the derive's `offset_ids` after construction.
        #[doc(hidden)]
        pub fn offset_id(&mut self, id_base: u32) {
            self.id = crate::rebase_nested_param_id(self.id, id_base);
        }
    };
}

/// Process-wide custom-state edit counter, kept only for the deprecated
/// [`bump_edit_epoch`] / [`custom_edit_epoch`] pair. Parameter setters never
/// touch it.
static CUSTOM_EDIT_EPOCH: AtomicU64 = AtomicU64::new(0);

/// A fresh per-field edit counter; a derived tree replaces it with its own.
fn own_edit_epoch() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(0))
}

/// Process-wide custom-state edits.
#[deprecated(
    since = "7.1.0",
    note = "process-wide and shared by every instance; parameter edits count on \
            `Params::parameter_edit_epoch`, custom state should keep its own generation"
)]
#[inline]
pub fn edit_epoch() -> u64 {
    CUSTOM_EDIT_EPOCH.load(Ordering::Acquire)
}

/// Mark a custom-state edit on the process-wide counter.
#[deprecated(
    since = "7.1.0",
    note = "process-wide and shared by every instance; keep a generation on the \
            custom state itself and call `PluginContext::mark_dirty` for the host"
)]
#[inline]
pub fn bump_edit_epoch() {
    CUSTOM_EDIT_EPOCH.fetch_add(1, Ordering::Release);
}

/// Process-wide custom-state edits only; parameter setters never move it.
#[deprecated(
    since = "7.1.0",
    note = "process-wide and shared by every instance; keep a generation on the \
            custom state itself"
)]
#[inline]
pub fn custom_edit_epoch() -> u64 {
    CUSTOM_EDIT_EPOCH.load(Ordering::Acquire)
}

/// Atomic f64 - wraps `AtomicU64` with f64 load/store.
pub struct AtomicF64 {
    bits: AtomicU64,
}

impl AtomicF64 {
    pub fn new(value: f64) -> Self {
        Self {
            bits: AtomicU64::new(value.to_bits()),
        }
    }

    #[inline]
    pub fn load(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn store(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// A continuous floating-point parameter.
///
/// Metadata is borrowed from the declaring struct's static table and the
/// smoother is only allocated when the field asks for one, so a param is
/// a pointer, an id, an atomic and an optional box. Plugins with
/// hundreds of params (or pools of nested groups) stay inside a host's
/// stack budget for the state struct.
pub struct FloatParam {
    info: &'static ParamInfo,
    id: u32,
    edit_epoch: Arc<AtomicU64>,
    value: AtomicF64,
    smoother: Option<Box<Smoother>>,
}

impl FloatParam {
    shared_param_meta!();

    #[must_use]
    pub fn new(info: &'static ParamInfo, smoothing: SmoothingStyle) -> Self {
        let default = info.default_plain;
        // Surface a mis-ordered or non-finite range (a `Linear { min: 6,
        // max: -60 }` typo) at construction, where it's obvious, rather than
        // as a `clamp` panic on the first host automation write. The derive
        // already rejects `min >= max` at compile time; this covers direct
        // `FloatParam::new` callers. `set_value` normalizes the bounds so it
        // never panics even in release, where this assert is compiled out.
        let (lo, hi) = (info.range.min(), info.range.max());
        debug_assert!(
            lo.is_finite() && hi.is_finite() && lo <= hi,
            "FloatParam range bounds must be finite and ordered (min <= max); \
             got [{lo}, {hi}] - check the `range = \"...\"` attribute"
        );
        // Contain the default as `set_value` contains writes. A NaN or
        // out-of-range default (a hand-rolled `FloatParam::new` caller -
        // the derive rejects both at compile time) would otherwise ship
        // DSP at a value the host can't display and mutate it on the first
        // save/restore round-trip. debug_assert catches it in dev; release
        // clamps for containment.
        debug_assert!(
            default.is_finite() && default >= lo.min(hi) && default <= lo.max(hi),
            "FloatParam default {default} is outside range [{lo}, {hi}] or non-finite"
        );
        let default = if default.is_finite() {
            default.clamp(lo.min(hi), lo.max(hi))
        } else {
            lo.min(hi)
        };
        let smoother = (!matches!(smoothing, SmoothingStyle::None)).then(|| {
            let smoother = Box::new(Smoother::new(smoothing));
            smoother.snap(default);
            smoother
        });
        Self {
            info,
            id: info.id,
            edit_epoch: own_edit_epoch(),
            value: AtomicF64::new(default),
            smoother,
        }
    }

    /// The smoother, when the field declared one (`smooth = "..."`).
    /// `SmoothingStyle::None` fields have none; their smoothed reads
    /// return the target directly.
    #[must_use]
    pub fn smoother(&self) -> Option<&Smoother> {
        self.smoother.as_deref()
    }

    /// Jump the smoother to the current target (state restore, or a
    /// sample-rate change that would otherwise glide from stale state).
    #[inline]
    pub fn snap_smoother(&self) {
        if let Some(smoother) = &self.smoother {
            smoother.snap(self.value.load());
        }
    }

    /// Re-derive the smoother's coefficients for `sample_rate`.
    pub fn set_sample_rate(&self, sample_rate: f64) {
        if let Some(smoother) = &self.smoother {
            smoother.set_sample_rate(sample_rate);
        }
    }

    /// Set the plain value (host automation and, crucially, state restore
    /// from a project file / preset - untrusted input `parse_state` can
    /// validate the structure of but not the values). Drop non-finite
    /// writes and clamp to the declared range, so a corrupt or hostile
    /// value can't latch a NaN into the smoother (which would then emit
    /// NaN audio forever) or drive out-of-range DSP.
    #[inline]
    pub fn set_value(&self, v: f64) {
        if !v.is_finite() {
            return;
        }
        // Normalize the bounds before clamping: `f64::clamp` panics if
        // `min > max`, and `range.min()`/`max()` return the stored fields,
        // so a mis-ordered range would otherwise panic on every write (on
        // whatever thread the host calls the setter from). `new` debug-
        // asserts the ordering; this keeps release safe regardless.
        let (lo, hi) = (self.info.range.min(), self.info.range.max());
        let clamped = v.clamp(lo.min(hi), lo.max(hi));
        let value = if matches!(
            self.info.range.base(),
            crate::range::ParamRange::Stepped { .. }
        ) {
            self.info
                .range
                .denormalize(self.info.range.normalize(clamped))
        } else {
            clamped
        };
        self.note_edit(self.value.bits.swap(value.to_bits(), Ordering::Relaxed) != value.to_bits());
    }

    /// Internal: raw target value at `f64` precision (host-side
    /// surface, before any narrowing for DSP use). Plugin authors
    /// don't call this directly - they go through the prelude's
    /// `read` / `value` / `current` instead, which have no
    /// precision-suffix decisions at the call site.
    #[doc(hidden)]
    #[inline]
    pub fn raw_target(&self) -> f64 {
        self.value.load()
    }

    /// Internal: next smoother step at `f32` (the smoother's native
    /// precision). See [`Self::raw_target`].
    #[doc(hidden)]
    #[inline]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn raw_smoothed_next(&self) -> f32 {
        let target = self.value.load();
        match &self.smoother {
            Some(smoother) => smoother.next(target),
            None => f32::from_f64(target),
        }
    }

    /// Internal: current smoother value at `f32`. See
    /// [`Self::raw_target`].
    #[doc(hidden)]
    #[inline]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn raw_smoothed_current(&self) -> f32 {
        match &self.smoother {
            Some(smoother) => smoother.current(),
            None => f32::from_f64(self.value.load()),
        }
    }

    /// Constant smoothed output once the smoother has landed on its target.
    /// Returns `None` while a ramp still needs to advance.
    #[inline]
    #[must_use]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn settled_value(&self) -> Option<f32> {
        let target = self.value.load();
        let settled = self
            .smoother
            .as_ref()
            .is_none_or(|smoother| smoother.settled_at(target));
        (settled && target.is_finite()).then(|| f32::from_f64(target))
    }

    /// Internal: advance the smoother by `out.len()` samples,
    /// writing each step to `out`. Plugin authors reach this through
    /// [`FloatParamReadF32::read_into`] /
    /// [`FloatParamReadF64::read_into`] in the prelude.
    #[doc(hidden)]
    #[inline]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn raw_smoothed_next_into(&self, out: &mut [f32]) {
        let target = self.value.load();
        match &self.smoother {
            Some(smoother) => smoother.next_into(target, out),
            None => out.fill(f32::from_f64(target)),
        }
    }

    /// Internal: f64 twins of the smoothed reads, for
    /// [`FloatParamReadF64`]: no `f32` rounding end to end.
    #[doc(hidden)]
    #[inline]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn raw_smoothed_next_f64(&self) -> f64 {
        let target = self.value.load();
        self.smoother
            .as_ref()
            .map_or(target, |s| s.next_f64(target))
    }

    #[doc(hidden)]
    #[inline]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn raw_smoothed_next_into_f64(&self, out: &mut [f64]) {
        let target = self.value.load();
        match &self.smoother {
            Some(smoother) => smoother.next_into_f64(target, out),
            None => out.fill(target),
        }
    }

    #[doc(hidden)]
    #[inline]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn raw_smoothed_next_after_f64(&self, n_samples: usize) -> f64 {
        let target = self.value.load();
        self.smoother
            .as_ref()
            .map_or(target, |s| s.next_after_f64(target, n_samples))
    }

    #[doc(hidden)]
    #[inline]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn raw_smoothed_current_f64(&self) -> f64 {
        self.smoother
            .as_ref()
            .map_or_else(|| self.value.load(), |s| s.current_f64())
    }

    /// Internal: advance the smoother by `n_samples` and return only
    /// the final value. Plugin authors reach this through
    /// [`FloatParamReadF32::read_after`] /
    /// [`FloatParamReadF64::read_after`] in the prelude.
    #[doc(hidden)]
    #[inline]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn raw_smoothed_next_after(&self, n_samples: usize) -> f32 {
        let target = self.value.load();
        match &self.smoother {
            Some(smoother) => smoother.next_after(target, n_samples),
            None => f32::from_f64(target),
        }
    }

    /// Read the value rounded to the nearest non-negative `usize`.
    /// Use this for discrete-range params consumed as array indices.
    /// Negatives, NaN, and infinities saturate at `0` / `usize::MAX`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[inline]
    pub fn value_usize(&self) -> usize {
        let v = self.value.load().round();
        if v <= 0.0 { 0 } else { v as usize }
    }

    /// Read the value rounded to the nearest `i32`. Out-of-range
    /// values saturate at `i32::MIN` / `i32::MAX`; NaN → 0.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn value_i32(&self) -> i32 {
        self.value.load().round() as i32
    }

    /// Read the value rounded to the nearest `u8`. Negatives clamp to
    /// `0`; values above `255` saturate at `u8::MAX`; NaN → 0.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[inline]
    pub fn value_u8(&self) -> u8 {
        let v = self.value.load().round();
        if v <= 0.0 {
            0
        } else if v >= 255.0 {
            255
        } else {
            v as u8
        }
    }

    /// True when the smoother is mid-step toward a new target.
    /// Inverse of [`Smoother::is_converged`].
    ///
    /// Use to branch in `process()` between a constant-gain fast
    /// path (smoothers at target, gain identical across the whole
    /// block, one `gain_block` per channel) and the envelope slow
    /// path (`read_into` + per-sample envelope + `chunks_mut`).
    /// `SmoothingStyle::None` always reports `false` here, so the
    /// fast path is unconditional for plugins that disable
    /// smoothing.
    ///
    /// ```ignore
    /// if !self.params.gain.is_smoothing() && !self.params.pan.is_smoothing() {
    ///     // fast path: gain is constant for the whole block.
    /// } else {
    ///     // slow path: envelope precompute + chunked apply.
    /// }
    /// ```
    #[inline]
    #[must_use]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    pub fn is_smoothing(&self) -> bool {
        self.smoother
            .as_ref()
            .is_some_and(|smoother| !smoother.is_converged(self.value.load()))
    }
}

/// Precision-routed read accessors for [`FloatParam`] at `f32`.
///
/// The plugin prelude (`moose::prelude` / `moose::prelude32`) imports
/// this trait via `pub use … as _;`, so plugin code reads:
///
/// ```ignore
/// use moose::prelude::*;
/// let gain = self.params.gain.read();   // f32 - no annotation needed
/// ```
///
/// The trait's methods shadow nothing - `FloatParam` has no inherent
/// `read` / `value` / `current`, so name resolution picks the one
/// (and only one) trait that's in scope. Importing `prelude64`
/// instead brings [`FloatParamReadF64`] into scope and the same
/// source resolves to `f64`. Importing **both** preludes is a
/// compile error (`multiple applicable items in scope`) - which is
/// the right error for a file that hasn't committed to a precision.
pub trait FloatParamReadF32 {
    /// Next smoothed value. Call once per sample in `process()`.
    #[must_use]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    fn read(&self) -> f32;

    /// Fill `out` with the next `out.len()` smoothed samples; advance
    /// the smoother by `out.len()` (not by the slice's capacity).
    /// One atomic load + one atomic store amortized over the whole
    /// slice. The right primitive when chunking `process()`'s block
    /// dynamically:
    ///
    /// ```ignore
    /// let mut delay = [0.0_f32; MAX_BLOCK];
    /// while offset < total {
    ///     let n = (total - offset).min(MAX_BLOCK);
    ///     self.params.delay.read_into(&mut delay[..n]);
    ///     // ... consume delay[..n] for n samples ...
    ///     offset += n;
    /// }
    /// ```
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    fn read_into(&self, out: &mut [f32]);

    /// Advance the smoother by `n_samples` in one call, returning
    /// only the final value. Use for **block-rate** DSP - hard
    /// gates, mode switches, anything that needs one smoothed value
    /// per audio block. Pass `buffer.num_samples()` to keep the
    /// smoother's wall-clock convergence time matching the smoother
    /// declaration (`smooth = "exp(20)"` then actually settles in
    /// ~20 ms instead of ~20 blocks). One atomic load + one atomic
    /// store; the per-sample envelope is skipped.
    #[must_use]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    fn read_after(&self, n_samples: usize) -> f32;

    /// Current smoothed value without advancing.
    #[must_use]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    fn current(&self) -> f32;

    /// Raw target value (post-`set_normalized` / host automation),
    /// not the smoothed output. Use [`Self::read`] / [`Self::current`]
    /// in the DSP loop.
    #[must_use]
    fn value(&self) -> f32;
}

/// Precision-routed read accessors for [`FloatParam`] at `f64`. See
/// [`FloatParamReadF32`] for the contract.
pub trait FloatParamReadF64 {
    #[must_use]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    fn read(&self) -> f64;
    /// f64 view of [`FloatParamReadF32::read_into`]; one widen per
    /// slot on top of the same one-atomic-pair fast path.
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    fn read_into(&self, out: &mut [f64]);
    /// f64 view of [`FloatParamReadF32::read_after`]; one widen
    /// on top of the same one-atomic-pair fast path.
    #[must_use]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    fn read_after(&self, n_samples: usize) -> f64;
    #[must_use]
    #[deprecated(
        since = "7.1.0",
        note = "keeps the ramp in shared atomics; keep a `ParamSmoother` in the DSP state and step it in `process`"
    )]
    fn current(&self) -> f64;
    #[must_use]
    fn value(&self) -> f64;
}

#[allow(
    deprecated,
    reason = "the deprecated read path forwards to the deprecated raw reads"
)]
impl FloatParamReadF32 for FloatParam {
    #[inline]
    fn read(&self) -> f32 {
        self.raw_smoothed_next()
    }

    #[inline]
    fn read_into(&self, out: &mut [f32]) {
        self.raw_smoothed_next_into(out);
    }

    #[inline]
    fn read_after(&self, n_samples: usize) -> f32 {
        self.raw_smoothed_next_after(n_samples)
    }

    #[inline]
    fn current(&self) -> f32 {
        self.raw_smoothed_current()
    }

    #[inline]
    fn value(&self) -> f32 {
        f32::from_f64(self.raw_target())
    }
}

#[allow(
    deprecated,
    reason = "the deprecated read path forwards to the deprecated raw reads"
)]
impl FloatParamReadF64 for FloatParam {
    #[inline]
    fn read(&self) -> f64 {
        self.raw_smoothed_next_f64()
    }

    #[inline]
    fn read_into(&self, out: &mut [f64]) {
        self.raw_smoothed_next_into_f64(out);
    }

    #[inline]
    fn read_after(&self, n_samples: usize) -> f64 {
        self.raw_smoothed_next_after_f64(n_samples)
    }

    #[inline]
    fn current(&self) -> f64 {
        self.raw_smoothed_current_f64()
    }

    #[inline]
    fn value(&self) -> f64 {
        self.raw_target()
    }
}

/// One [`FloatParam`]'s smoothing, owned by the processor.
///
/// Keep one per smoothed parameter in your DSP state and step it in
/// `process`. The ramp is plain `f64` state in the processor; the only
/// shared memory a step reads is the parameter's atomic target and its
/// snap generation (so a host preset load still jumps the ramp instead of
/// gliding to it). The prelude's `params.x.read()` gives the same values,
/// bit for bit, but keeps the ramp in atomics inside the shared params.
///
/// ```ignore
/// #[derive(Default)]
/// struct Dsp { gain: ParamSmoother }
/// fn reset(dsp: &mut Dsp, params: &MyParams, config: &AudioConfig) {
///     dsp.gain.reset(&params.gain, config.sample_rate);
/// }
/// // process: let g = dsp.gain.next(&params.gain);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct ParamSmoother {
    state: SmoothState,
    /// The param's snap generation this ramp last followed.
    seen_snap: u32,
}

impl Default for ParamSmoother {
    /// An unsmoothed ramp at `0.0`; [`Self::reset`] gives it a param.
    fn default() -> Self {
        Self {
            state: SmoothState::new(SmoothingStyle::None, 44_100.0),
            seen_snap: 0,
        }
    }
}

impl ParamSmoother {
    /// A ramp at rest on `param`'s current target, following its declared
    /// smoothing style. Call [`Self::reset`] with the real sample rate
    /// before processing.
    #[must_use]
    pub fn new(param: &FloatParam) -> Self {
        let mut smoother = Self::default();
        smoother.reset(param, 44_100.0);
        smoother
    }

    /// Follow `param`'s smoothing style at `sample_rate` and jump to its
    /// target. Doesn't allocate; call it from the plugin's `reset`. A
    /// `Default` smoother does nothing useful until this runs.
    pub fn reset(&mut self, param: &FloatParam, sample_rate: f64) {
        let style = param
            .smoother()
            .map_or(SmoothingStyle::None, Smoother::style);
        self.state = SmoothState::new(style, sample_rate);
        self.jump_to(param);
    }

    fn jump_to(&mut self, param: &FloatParam) {
        self.seen_snap = param.smoother().map_or(0, Smoother::snap_generation);
        self.state.snap(param.raw_target());
    }

    /// The param's target, after jumping to it if the host snapped the
    /// param (a state load) since the last step.
    #[inline]
    fn target(&mut self, param: &FloatParam) -> f64 {
        if let Some(shared) = param.smoother() {
            let generation = shared.snap_generation();
            if generation != self.seen_snap {
                self.seen_snap = generation;
                self.state.snap(param.raw_target());
            }
        }
        param.raw_target()
    }

    /// Next smoothed value, advancing one sample.
    #[inline]
    pub fn next(&mut self, param: &FloatParam) -> f32 {
        let target = self.target(param);
        self.state.next(target)
    }

    /// [`Self::next`] at `f64`.
    #[inline]
    pub fn next_f64(&mut self, param: &FloatParam) -> f64 {
        let target = self.target(param);
        self.state.next_f64(target)
    }

    /// Advance `n_samples`, returning only the last value.
    #[inline]
    pub fn next_after(&mut self, param: &FloatParam, n_samples: usize) -> f32 {
        let target = self.target(param);
        self.state.next_after(target, n_samples)
    }

    /// [`Self::next_after`] at `f64`.
    #[inline]
    pub fn next_after_f64(&mut self, param: &FloatParam, n_samples: usize) -> f64 {
        let target = self.target(param);
        self.state.next_after_f64(target, n_samples)
    }

    /// Fill `out` with the next `out.len()` smoothed values.
    #[inline]
    pub fn next_into(&mut self, param: &FloatParam, out: &mut [f32]) {
        let target = self.target(param);
        self.state.next_into(target, out);
    }

    /// [`Self::next_into`] at `f64`.
    #[inline]
    pub fn next_into_f64(&mut self, param: &FloatParam, out: &mut [f64]) {
        let target = self.target(param);
        self.state.next_into_f64(target, out);
    }

    /// Current smoothed value without advancing.
    #[inline]
    #[must_use]
    pub fn current(&self) -> f32 {
        self.state.current()
    }

    /// [`Self::current`] at `f64`.
    #[inline]
    #[must_use]
    pub fn current_f64(&self) -> f64 {
        self.state.current_f64()
    }

    /// True while the ramp is still moving toward `param`'s target.
    #[inline]
    #[must_use]
    pub fn is_smoothing(&self, param: &FloatParam) -> bool {
        !self.state.is_converged(param.raw_target())
    }
}

/// A boolean parameter.
pub struct BoolParam {
    info: &'static ParamInfo,
    id: u32,
    edit_epoch: Arc<AtomicU64>,
    value: AtomicBool,
}

impl BoolParam {
    shared_param_meta!();

    /// # Panics
    ///
    /// Panics if `info.default_plain` isn't exactly `0.0` or `1.0`.
    /// Bool params have no halfway value; the derive emits `0.0` /
    /// `1.0` only, so this fires only when a user constructs a
    /// `BoolParam` from hand-rolled `ParamInfo`.
    #[must_use]
    pub fn new(info: &'static ParamInfo) -> Self {
        let default = match info.default_plain {
            0.0 => false,
            1.0 => true,
            other => panic!(
                "BoolParam '{}' default {} must be exactly 0.0 (false) \
                 or 1.0 (true) - bool params have no halfway value",
                info.name, other,
            ),
        };
        Self {
            info,
            id: info.id,
            edit_epoch: own_edit_epoch(),
            value: AtomicBool::new(default),
        }
    }

    pub fn value(&self) -> bool {
        self.value.load(Ordering::Relaxed)
    }

    pub fn set_value(&self, v: bool) {
        self.note_edit(self.value.swap(v, Ordering::Relaxed) != v);
    }
}

/// An integer parameter.
pub struct IntParam {
    info: &'static ParamInfo,
    id: u32,
    edit_epoch: Arc<AtomicU64>,
    value: AtomicI64,
}

impl IntParam {
    shared_param_meta!();

    /// # Panics
    ///
    /// Panics if `info.default_plain` is non-finite or doesn't
    /// round-trip through `i64`. The cast `f64 as i64` saturates
    /// silently - `default_plain = -1.0` lands on `-1` (fine), but
    /// `default_plain = 1e30` saturates to `i64::MAX` and `f64::NAN`
    /// becomes `0`. The derive populates `default_plain` from
    /// `#[param(default = ...)]`; a user-supplied float there is a
    /// programmer error, not a runtime condition we should
    /// silently absorb.
    // `truncated as f64 == default` is the integer round-trip
    // exactness check - epsilon would defeat its purpose. The
    // `as i64` truncation is the round-trip's whole point.
    #[allow(
        clippy::float_cmp,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss
    )]
    #[must_use]
    pub fn new(info: &'static ParamInfo) -> Self {
        let default = info.default_plain;
        assert!(
            default.is_finite(),
            "IntParam '{}' default {} is not finite",
            info.name,
            default,
        );
        let truncated = default as i64;
        assert!(
            truncated as f64 == default,
            "IntParam '{}' default {} doesn't round-trip through i64 \
             - supply an integer-valued default in the derive attribute",
            info.name,
            default,
        );
        let (lo, hi) = (info.range.min() as i64, info.range.max() as i64);
        assert!(
            truncated >= lo && truncated <= hi,
            "IntParam '{}' default {} is outside range [{}, {}]",
            info.name,
            truncated,
            lo,
            hi,
        );
        Self {
            info,
            id: info.id,
            edit_epoch: own_edit_epoch(),
            value: AtomicI64::new(truncated),
        }
    }

    pub fn value(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }

    /// Read the value widened to `f32`. Useful when an int param feeds
    /// a per-sample DSP loop that runs in `f32`.
    #[allow(clippy::cast_precision_loss)]
    #[inline]
    pub fn value_f32(&self) -> f32 {
        self.value.load(Ordering::Relaxed) as f32
    }

    /// Read the value widened to `f64`.
    #[allow(clippy::cast_precision_loss)]
    #[inline]
    pub fn value_f64(&self) -> f64 {
        self.value.load(Ordering::Relaxed) as f64
    }

    /// Read the value as a non-negative `usize`. Negatives clamp to 0;
    /// values above `usize::MAX` saturate.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    #[inline]
    pub fn value_usize(&self) -> usize {
        let v = self.value.load(Ordering::Relaxed);
        if v <= 0 { 0 } else { v as usize }
    }

    /// Read the value clamped to `i32` range.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn value_i32(&self) -> i32 {
        self.value
            .load(Ordering::Relaxed)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }

    /// Read the value clamped to `u8` range (`0..=255`).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    #[inline]
    pub fn value_u8(&self) -> u8 {
        self.value.load(Ordering::Relaxed).clamp(0, 255) as u8
    }

    /// Set the value, clamped to the declared range - symmetric with
    /// `FloatParam::set_value`. A corrupt preset or hostile automation
    /// value (`i64::MAX` into a `[0, 8]` param) must not reach the plugin,
    /// where it could index out of range and panic the audio thread. The
    /// bounds are normalized so a mis-ordered range can't panic `clamp`.
    #[allow(clippy::cast_possible_truncation)]
    pub fn set_value(&self, v: i64) {
        let (lo, hi) = (self.info.range.min() as i64, self.info.range.max() as i64);
        let v = v.clamp(lo.min(hi), lo.max(hi));
        self.note_edit(self.value.swap(v, Ordering::Relaxed) != v);
    }
}

/// Trait for enums used as parameters.
pub trait ParamEnum: crate::__private::Sealed + Clone + Copy + Send + Sync + 'static {
    fn from_index(index: usize) -> Self;
    fn to_index(&self) -> usize;
    fn name(&self) -> &'static str;
    fn variant_count() -> usize;
    fn variant_names() -> &'static [&'static str];
}

/// An enum parameter.
pub struct EnumParam<E: ParamEnum> {
    info: &'static ParamInfo,
    id: u32,
    edit_epoch: Arc<AtomicU64>,
    value: AtomicU32,
    _phantom: std::marker::PhantomData<E>,
}

impl<E: ParamEnum> EnumParam<E> {
    shared_param_meta!();

    /// # Panics
    ///
    /// Panics if `info.default_plain` is non-finite, negative, or
    /// `>= E::variant_count()`. The cast `f64 as u32` saturates
    /// silently - a user-supplied `#[param(default = -1)]` would
    /// land on variant 0 without any signal that the default was
    /// invalid. Validate up front so the bug surfaces at plugin
    /// construction time.
    // `f64::from(idx) == default` is the integer round-trip
    // exactness check - epsilon would defeat its purpose. The
    // `as u32` truncation is the round-trip's whole point.
    #[allow(
        clippy::float_cmp,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    #[must_use]
    pub fn new(info: &'static ParamInfo) -> Self {
        let default = info.default_plain;
        let count = E::variant_count();
        assert!(
            default.is_finite(),
            "EnumParam '{}' default {} is not finite",
            info.name,
            default,
        );
        assert!(
            default >= 0.0,
            "EnumParam '{}' default {} is negative; enum variants are \
             0-indexed",
            info.name,
            default,
        );
        let idx = default as u32;
        assert!(
            f64::from(idx) == default,
            "EnumParam '{}' default {} is non-integer; supply a 0-indexed \
             variant index",
            info.name,
            default,
        );
        assert!(
            (idx as usize) < count,
            "EnumParam '{}' default {} is out of range; only {} variant(s) \
             defined",
            info.name,
            idx,
            count,
        );
        Self {
            info,
            id: info.id,
            edit_epoch: own_edit_epoch(),
            value: AtomicU32::new(idx),
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn value(&self) -> E {
        // u32 → usize widens on 64-bit, narrows nowhere we ship to;
        // the lint trips because `usize` is target-dependent.
        #[allow(clippy::cast_possible_truncation)]
        let idx = self.value.load(Ordering::Relaxed) as usize;
        E::from_index(idx)
    }

    pub fn set_value(&self, v: E) {
        // Enum variant indices come from `ParamEnum::to_index`, whose
        // valid range is `0..variant_count()`; truncation past `u32::MAX`
        // would mean a > 4-billion-variant enum.
        #[allow(clippy::cast_possible_truncation)]
        let idx = v.to_index() as u32;
        self.note_edit(self.value.swap(idx, Ordering::Relaxed) != idx);
    }

    pub fn set_index(&self, idx: u32) {
        // Clamp to the enum's valid range. A preset saved with a wider
        // enum (a since-shrunk v1) restores an out-of-range index through
        // here; stored verbatim, `value()` / `from_index` read it as the
        // first variant while `get_normalized` clamps to the last, so audio
        // and display disagree. Clamp to the last variant - matching
        // `ParamRange::Enum::normalize`'s clamp - so they stay consistent.
        // `variant_count()` is >= 1 for any `ParamEnum`; `saturating_sub`
        // guards the underflow regardless.
        #[allow(clippy::cast_possible_truncation)]
        let max = (E::variant_count() as u32).saturating_sub(1);
        let idx = idx.min(max);
        self.note_edit(self.value.swap(idx, Ordering::Relaxed) != idx);
    }

    pub fn index(&self) -> u32 {
        self.value.load(Ordering::Relaxed)
    }

    /// Format a plain value (index as f64) to the variant name string.
    ///
    /// Associated function - the dispatch is purely on `E`, no instance
    /// state is read. The `#[derive(Params)]` macro calls it as
    /// `<EnumParam<E>>::format_by_index(value)` so the field type
    /// supplies `E`.
    #[must_use]
    pub fn format_by_index(value: f64) -> String {
        // `value` is a normalized f64 in `[0, count - 1]`; the round
        // → usize cast is bounded by the variant count.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = value.round() as usize;
        E::from_index(idx).name().to_string()
    }
}

// ---------------------------------------------------------------------------
// ValueListParam
// ---------------------------------------------------------------------------

/// A discrete parameter whose labelled value list can be populated or updated
/// at runtime (e.g. a sample-bank selector, impulse-response picker, or
/// dynamic routing target list).
///
/// Audio-thread reads (`index`, `len`, `get_plain`, `get_normalized`) and
/// writes (`set_index`, `set_plain`, `set_normalized`) are 100% lock-free.
/// Label updates (`set_labels`) run off the audio thread and notify the
/// wrapper via [`CatalogSignal`] (`CatalogChange::INFO` when only labels
/// changed; `CatalogChange::STRUCTURE | INFO` when the item count changed).
pub struct ValueListParam {
    info: &'static ParamInfo,
    id: u32,
    edit_epoch: Arc<AtomicU64>,
    value: AtomicU32,
    count: AtomicU32,
    labels: RwLock<Vec<String>>,
    catalog_signal: Mutex<Weak<CatalogSignal>>,
    pending_changes: AtomicU8,
}

impl ValueListParam {
    shared_param_meta!();

    /// Construct a `ValueListParam` seeded with default numeric labels sized
    /// from `info.range` (or 1 entry if the range has fewer than 1 items).
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn new(info: &'static ParamInfo) -> Self {
        let initial_count = match info.range {
            ParamRange::Enum { count } => count.max(1),
            ParamRange::Discrete { min, max } if max >= min => {
                usize::try_from(max - min + 1).unwrap_or(1).max(1)
            }
            _ => 1,
        };
        let labels: Vec<String> = (0..initial_count).map(|i| i.to_string()).collect();
        Self::with_labels(info, labels)
    }

    /// Construct a `ValueListParam` with explicit initial labels. If `labels`
    /// is empty, a single `"Default"` entry is inserted so the range is always
    /// non-degenerate.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn with_labels<I, S>(info: &'static ParamInfo, labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut items: Vec<String> = labels.into_iter().map(Into::into).collect();
        if items.is_empty() {
            items.push("Default".to_owned());
        }
        let count_u32 = u32::try_from(items.len()).unwrap_or(u32::MAX);
        let default = if info.default_plain.is_finite() && info.default_plain >= 0.0 {
            (info.default_plain.round() as u32).min(count_u32.saturating_sub(1))
        } else {
            0
        };
        Self {
            info,
            id: info.id,
            edit_epoch: own_edit_epoch(),
            value: AtomicU32::new(default),
            count: AtomicU32::new(count_u32),
            labels: RwLock::new(items),
            catalog_signal: Mutex::new(Weak::new()),
            pending_changes: AtomicU8::new(0),
        }
    }

    /// Replace the value list on the main thread. Returns `false` if `labels`
    /// is empty (leaving the existing list untouched). Not real-time safe.
    pub fn set_labels<I, S>(&self, labels: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let items: Vec<String> = labels.into_iter().map(Into::into).collect();
        if items.is_empty() {
            return false;
        }
        let new_count = u32::try_from(items.len()).unwrap_or(u32::MAX);
        let mut guard = self.labels.write().unwrap_or_else(PoisonError::into_inner);
        if *guard == items {
            return true;
        }
        let old_count = self.count.swap(new_count, Ordering::Release);
        *guard = items;
        drop(guard);

        let mut flags = CatalogChange::INFO;
        if old_count != new_count {
            flags |= CatalogChange::STRUCTURE;
            let max_idx = new_count.saturating_sub(1);
            let mut cur = self.value.load(Ordering::Relaxed);
            while cur > max_idx {
                match self.value.compare_exchange_weak(
                    cur,
                    max_idx,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        self.note_edit(true);
                        flags |= CatalogChange::VALUES;
                        break;
                    }
                    Err(observed) => cur = observed,
                }
            }
        }
        self.pending_changes
            .fetch_or(flags.bits(), Ordering::Release);
        if let Some(signal) = self
            .catalog_signal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade()
        {
            signal.notify();
        }
        true
    }

    /// Current selected 0-based index. Lock-free on any thread.
    #[inline]
    #[must_use]
    pub fn index(&self) -> u32 {
        let max = self.count.load(Ordering::Acquire).saturating_sub(1);
        self.value.load(Ordering::Relaxed).min(max)
    }

    /// Select a 0-based index, clamping to the current list length. Lock-free.
    #[inline]
    pub fn set_index(&self, idx: u32) {
        let max = self.count.load(Ordering::Acquire).saturating_sub(1);
        let clamped = idx.min(max);
        self.note_edit(self.value.swap(clamped, Ordering::Relaxed) != clamped);
    }

    /// Number of entries currently in the list (always >= 1). Lock-free.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Acquire) as usize
    }

    /// Always `false` since a `ValueListParam` holds at least one entry.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot of all current labels (main thread).
    #[must_use]
    pub fn labels(&self) -> Vec<String> {
        self.labels
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Current label at `index` (main thread).
    #[must_use]
    pub fn label_at(&self, index: usize) -> Option<String> {
        self.labels
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(index)
            .cloned()
    }

    /// Currently selected label (main thread).
    #[must_use]
    pub fn current_label(&self) -> String {
        let idx = self.index() as usize;
        self.label_at(idx).unwrap_or_else(|| idx.to_string())
    }

    /// Live [`ParamRange::Enum`] reflecting the current entry count. Lock-free.
    #[inline]
    #[must_use]
    pub fn current_range(&self) -> ParamRange {
        ParamRange::Enum {
            count: self.len().max(1),
        }
    }

    /// Live [`ParamInfo`] with this instance's rebased id and current list count.
    #[must_use]
    pub fn current_info(&self) -> ParamInfo {
        let mut info = self.info();
        info.range = self.current_range();
        info
    }

    /// Current plain value (`0.0..=(len - 1)`). Lock-free.
    #[inline]
    #[must_use]
    pub fn get_plain(&self) -> f64 {
        f64::from(self.index())
    }

    /// Set plain value (rounded and clamped to `0..len`). Lock-free.
    #[inline]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn set_plain(&self, value: f64) {
        if !value.is_finite() {
            return;
        }
        let max = self.count.load(Ordering::Acquire).saturating_sub(1);
        let idx = if value <= 0.0 {
            0
        } else {
            (value.round() as u32).min(max)
        };
        self.note_edit(self.value.swap(idx, Ordering::Relaxed) != idx);
    }

    /// Current normalized value in `0.0..=1.0`. Lock-free.
    #[inline]
    #[must_use]
    pub fn get_normalized(&self) -> f64 {
        self.current_range().normalize(self.get_plain())
    }

    /// Set from normalized value in `0.0..=1.0`. Lock-free.
    #[inline]
    pub fn set_normalized(&self, norm: f64) {
        if !norm.is_finite() {
            return;
        }
        let plain = self.current_range().denormalize(norm);
        self.set_plain(plain);
    }

    /// Format a plain index into its label (main thread).
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn format_value(&self, plain: f64) -> String {
        let guard = self.labels.read().unwrap_or_else(PoisonError::into_inner);
        let max = guard.len().saturating_sub(1);
        let idx = if !plain.is_finite() || plain <= 0.0 {
            0
        } else {
            (plain.round() as usize).min(max)
        };
        guard.get(idx).cloned().unwrap_or_else(|| idx.to_string())
    }

    /// Parse a label (case-insensitive) or 0-based index into a plain value (main thread).
    #[must_use]
    pub fn parse_value(&self, text: &str) -> Option<f64> {
        let needle = text.trim();
        let guard = self.labels.read().unwrap_or_else(PoisonError::into_inner);
        if let Some(pos) = guard
            .iter()
            .position(|label| label.trim().eq_ignore_ascii_case(needle))
        {
            #[allow(clippy::cast_precision_loss)]
            return Some(pos as f64);
        }
        let idx: usize = needle.parse().ok()?;
        if idx < guard.len() {
            #[allow(clippy::cast_precision_loss)]
            Some(idx as f64)
        } else {
            None
        }
    }

    /// Bind the wrapper's [`CatalogSignal`] so `set_labels` wakes the host.
    #[doc(hidden)]
    pub fn set_catalog_signal(&self, signal: Weak<CatalogSignal>) {
        *self
            .catalog_signal
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = signal;
    }

    /// Take pending catalog changes produced by [`Self::set_labels`].
    #[doc(hidden)]
    pub fn take_catalog_changes(&self) -> CatalogChanges {
        let bits = self.pending_changes.swap(0, Ordering::AcqRel);
        let flags = CatalogChange::from_bits_truncate(bits);
        CatalogChanges {
            flags,
            removed: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// MeterSlot
// ---------------------------------------------------------------------------

/// A meter slot with an auto-assigned ID.
///
/// Declare in your params struct with `#[meter]`:
/// ```ignore
/// #[derive(Params)]
/// pub struct MyParams {
///     #[meter]
///     pub meter_left: MeterSlot,
/// }
/// ```
///
/// `id` is `pub` so the `#[derive(Params)]` macro can construct a
/// `MeterSlot { id: <auto-assigned> }` directly without going through
/// a `pub fn new(id)` constructor that would let user code mint
/// arbitrary slots and break the auto-assignment contract.
pub struct MeterSlot {
    #[doc(hidden)]
    pub id: u32,
}

impl MeterSlot {
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }
}

impl From<MeterSlot> for u32 {
    fn from(m: MeterSlot) -> u32 {
        m.id
    }
}

impl From<&MeterSlot> for u32 {
    fn from(m: &MeterSlot) -> u32 {
        m.id
    }
}

#[cfg(test)]
#[expect(
    deprecated,
    reason = "the bit-identity tests compare the owned smoother against the deprecated shared path"
)]
mod tests {
    use super::*;
    use crate::info::{ParamFlags, ParamUnit, ParamValueKind};
    use crate::range::ParamRange;

    /// Keeps every leaked test `ParamInfo` reachable from a static, so
    /// Miri's leak check doesn't flag the intentional `Box::leak`.
    static INFOS: std::sync::Mutex<Vec<&'static ParamInfo>> = std::sync::Mutex::new(Vec::new());

    fn info(name: &'static str, range: ParamRange, default_plain: f64) -> &'static ParamInfo {
        let info = Box::leak(Box::new(ParamInfo {
            id: 0,
            name,
            short_name: name,
            group: "",
            range,
            default_plain,
            flags: ParamFlags::AUTOMATABLE,
            unit: ParamUnit::None,
            kind: ParamValueKind::Float,
            midi_map: None,
            midi_channel: None,
        }));
        INFOS.lock().unwrap().push(info);
        info
    }

    #[test]
    fn float_param_is_pointer_sized_metadata() {
        // The borrowed-info layout: a param must not carry `ParamInfo`
        // (or an unused smoother) inline.
        // One edit-counter pointer (the field's own, or its tree's).
        assert!(std::mem::size_of::<FloatParam>() <= 40);
        let p = float(0.0, 1.0);
        assert!(p.smoother().is_none(), "no smoother without a style");
        p.set_value(0.5);
        assert!(!p.is_smoothing());
        assert!((p.raw_smoothed_next() - 0.5).abs() < 1e-6);
        let mut out = [0.0_f32; 4];
        p.raw_smoothed_next_into(&mut out);
        assert!(out.iter().all(|v| (v - 0.5).abs() < 1e-6));
    }

    /// A field in a tree counts on the tree's counter (finding 13).
    #[test]
    fn tree_field_edits_skip_the_process_wide_epoch() {
        let mut p = float(0.0, 1.0);
        let tree = Arc::new(AtomicU64::new(0));
        p.set_edit_epoch(Arc::clone(&tree));
        p.set_value(0.5);
        assert_eq!(tree.load(Ordering::Acquire), 1);
    }

    /// A processor-owned smoother gives the shared read path's values,
    /// and follows a host snap (state load) through the snap generation.
    #[test]
    fn param_smoother_matches_shared_reads_and_follows_snaps() {
        let info = info(
            "Gain",
            ParamRange::Linear {
                min: 0.0,
                max: 10.0,
            },
            1.0,
        );
        let shared = FloatParam::new(info, SmoothingStyle::Exponential(5.0));
        shared.set_sample_rate(48_000.0);
        let owned_param = FloatParam::new(info, SmoothingStyle::Exponential(5.0));
        let mut owned = ParamSmoother::new(&owned_param);
        owned.reset(&owned_param, 48_000.0);
        for (k, target) in [8.0, 2.0, 9.5].into_iter().enumerate() {
            shared.set_value(target);
            owned_param.set_value(target);
            if k == 2 {
                shared.snap_smoother();
                owned_param.snap_smoother();
            }
            for _ in 0..300 {
                assert_eq!(
                    FloatParamReadF32::read(&shared).to_bits(),
                    owned.next(&owned_param).to_bits(),
                    "target {target}"
                );
            }
        }
        assert!(!owned.is_smoothing(&owned_param));
    }

    #[test]
    fn changed_value_advances_only_its_own_edit_epoch() {
        let p = float(0.0, 1.0);
        let other = float(0.0, 1.0);
        p.set_value(0.5);
        assert_eq!(p.parameter_edit_epoch(), 1);
        p.set_value(0.5);
        assert_eq!(
            p.parameter_edit_epoch(),
            1,
            "an unchanged write doesn't count"
        );
        assert_eq!(
            other.parameter_edit_epoch(),
            0,
            "standalone fields don't share a counter"
        );
    }

    /// The f64 prelude reads without an f32 round trip (finding 6).
    #[test]
    fn f64_reads_keep_f64_precision() {
        let p = FloatParam::new(
            info("precise", ParamRange::Linear { min: 0.0, max: 1.0 }, 0.0),
            SmoothingStyle::Linear(1.0),
        );
        p.set_sample_rate(48_000.0);
        p.set_value(0.1);
        let mut out = vec![0.0_f64; 4096];
        FloatParamReadF64::read_into(&p, &mut out);
        assert_eq!(out[4095].to_bits(), 0.1_f64.to_bits());
        assert_eq!(FloatParamReadF64::read(&p).to_bits(), 0.1_f64.to_bits());
        assert_eq!(
            FloatParamReadF64::read_after(&p, 8).to_bits(),
            0.1_f64.to_bits()
        );
        assert_eq!(FloatParamReadF64::current(&p).to_bits(), 0.1_f64.to_bits());
        // Mid-ramp values aren't f32-quantised either.
        p.set_value(0.3);
        let v = FloatParamReadF64::read(&p);
        #[allow(clippy::cast_possible_truncation)]
        let narrowed = f64::from(v as f32);
        assert_ne!(v.to_bits(), narrowed.to_bits());
    }

    #[test]
    fn settled_value_requires_a_finished_ramp() {
        let p = FloatParam::new(
            info("smoothed", ParamRange::Linear { min: 0.0, max: 1.0 }, 0.0),
            SmoothingStyle::Exponential(1.0),
        );
        p.set_sample_rate(48_000.0);
        assert_eq!(p.settled_value(), Some(0.0));
        p.set_value(0.8);
        assert_eq!(p.settled_value(), None);
        let mut out = [0.0; 4096];
        p.raw_smoothed_next_into(&mut out);
        assert_eq!(p.settled_value(), Some(0.8_f32));
        p.raw_smoothed_next_into(&mut out);
        assert!(
            out.iter()
                .all(|&sample| sample.to_bits() == 0.8_f32.to_bits())
        );
    }

    #[test]
    fn offset_id_rebases_only_the_instance() {
        let mut p = float(0.0, 1.0);
        p.offset_id(512);
        assert_eq!(p.id(), 512);
        assert_eq!(p.info().id, 512, "info() carries the rebased id");
        assert_eq!(p.static_info().id, 0, "the shared table is untouched");
    }

    #[derive(Clone, Copy)]
    enum E4 {
        A,
        B,
        C,
        D,
    }
    impl crate::__private::Sealed for E4 {}
    impl ParamEnum for E4 {
        fn from_index(i: usize) -> Self {
            match i {
                0 => Self::A,
                1 => Self::B,
                2 => Self::C,
                _ => Self::D,
            }
        }
        fn to_index(&self) -> usize {
            *self as usize
        }
        fn name(&self) -> &'static str {
            match self {
                Self::A => "A",
                Self::B => "B",
                Self::C => "C",
                Self::D => "D",
            }
        }
        fn variant_count() -> usize {
            4
        }
        fn variant_names() -> &'static [&'static str] {
            &["A", "B", "C", "D"]
        }
    }

    #[test]
    fn enum_param_accepts_in_range_default() {
        let p: EnumParam<E4> = EnumParam::new(info("Mode", ParamRange::Enum { count: 4 }, 2.0));
        assert_eq!(p.index(), 2);
    }

    #[test]
    #[should_panic(expected = "negative")]
    fn enum_param_rejects_negative_default() {
        let _: EnumParam<E4> = EnumParam::new(info("Mode", ParamRange::Enum { count: 4 }, -1.0));
    }

    #[test]
    fn enum_param_set_index_clamps_out_of_range() {
        // A preset saved with a wider (5-variant) enum restores index 4
        // into this 4-variant enum. It must clamp to the last variant so
        // `value()` (audio) and the normalized read (display) agree - not
        // play the first variant while `normalize` clamps to the last.
        let p: EnumParam<E4> = EnumParam::new(info("Mode", ParamRange::Enum { count: 4 }, 0.0));
        p.set_index(4);
        assert_eq!(p.index(), 3, "out-of-range index clamps to last variant");
        assert!(matches!(p.value(), E4::D));
        p.set_index(1000);
        assert_eq!(p.index(), 3);
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn enum_param_rejects_overflow_default() {
        let _: EnumParam<E4> = EnumParam::new(info("Mode", ParamRange::Enum { count: 4 }, 99.0));
    }

    #[test]
    #[should_panic(expected = "non-integer")]
    fn enum_param_rejects_fractional_default() {
        let _: EnumParam<E4> = EnumParam::new(info("Mode", ParamRange::Enum { count: 4 }, 1.5));
    }

    #[test]
    fn int_param_accepts_negative_default() {
        let p = IntParam::new(info("N", ParamRange::Discrete { min: -10, max: 10 }, -3.0));
        assert_eq!(p.value(), -3);
    }

    #[test]
    #[should_panic(expected = "round-trip")]
    fn int_param_rejects_fractional_default() {
        let _ = IntParam::new(info("N", ParamRange::Discrete { min: 0, max: 10 }, 1.5));
    }

    #[test]
    #[should_panic(expected = "outside range")]
    fn int_param_rejects_out_of_range_default() {
        let _ = IntParam::new(info("N", ParamRange::Discrete { min: 0, max: 5 }, 10.0));
    }

    #[test]
    fn int_param_set_value_clamps_to_range() {
        // A corrupt preset restoring a wild value must not land out of range
        // (symmetric with FloatParam::set_value); the derive stores
        // `value.round() as i64`, so i64::MAX is a realistic input.
        let p = IntParam::new(info("N", ParamRange::Discrete { min: 0, max: 8 }, 0.0));
        p.set_value(i64::MAX);
        assert_eq!(p.value(), 8, "clamps above max");
        p.set_value(-1000);
        assert_eq!(p.value(), 0, "clamps below min");
        p.set_value(5);
        assert_eq!(p.value(), 5, "in-range value stored as-is");
    }

    fn float(min: f64, max: f64) -> FloatParam {
        FloatParam::new(
            info("Gain", ParamRange::Linear { min, max }, 0.0),
            SmoothingStyle::None,
        )
    }

    #[test]
    #[allow(clippy::float_cmp)] // clamp / dropped-write yields the exact stored value
    fn float_set_value_drops_non_finite() {
        let p = float(-60.0, 6.0);
        p.set_value(-12.0);
        p.set_value(f64::NAN);
        assert_eq!(p.raw_target(), -12.0, "NaN write is dropped");
        p.set_value(f64::INFINITY);
        assert_eq!(p.raw_target(), -12.0, "infinite write is dropped");
    }

    #[test]
    #[allow(clippy::float_cmp)] // clamp yields the exact range bound
    fn float_set_value_clamps_to_range() {
        let p = float(-60.0, 6.0);
        p.set_value(1e308);
        assert_eq!(p.raw_target(), 6.0, "clamps above max");
        p.set_value(-1e308);
        assert_eq!(p.raw_target(), -60.0, "clamps below min");
    }

    /// A mis-ordered range (`min > max`) is a bug caught at construction in
    /// debug builds - loud and early, not a `clamp` panic buried in a
    /// host-automation callback.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ordered")]
    fn float_new_debug_asserts_misordered_range() {
        let _ = FloatParam::new(
            info(
                "Bad",
                ParamRange::Linear {
                    min: 6.0,
                    max: -60.0,
                },
                0.0,
            ),
            SmoothingStyle::None,
        );
    }

    /// In release the construction assert is compiled out, so `set_value`
    /// must still not panic on a mis-ordered range: it normalizes the clamp
    /// bounds. (`f64::clamp` would panic on `min > max`.)
    #[cfg(not(debug_assertions))]
    #[test]
    fn float_set_value_survives_misordered_range() {
        let p = FloatParam::new(
            info(
                "Bad",
                ParamRange::Linear {
                    min: 6.0,
                    max: -60.0,
                },
                0.0,
            ),
            SmoothingStyle::None,
        );
        p.set_value(1000.0); // must not panic
        let v = p.raw_target();
        assert!(
            (-60.0..=6.0).contains(&v),
            "clamped to the normalized interval"
        );
    }

    #[test]
    fn value_list_param_updates_labels_notifies_catalog_and_clamps() {
        use std::sync::atomic::AtomicUsize;

        let woken = Arc::new(AtomicUsize::new(0));
        let woken_cb = Arc::clone(&woken);
        let signal = Arc::new(CatalogSignal::new(move || {
            woken_cb.fetch_add(1, Ordering::Relaxed);
        }));

        let p = ValueListParam::with_labels(
            info("Sample", ParamRange::Enum { count: 3 }, 0.0),
            ["Kick", "Snare", "Hat"],
        );
        p.set_catalog_signal(Arc::downgrade(&signal));
        assert_eq!(p.len(), 3);
        assert_eq!(p.current_label(), "Kick");
        p.set_index(2);
        assert_eq!(p.current_label(), "Hat");
        assert_eq!(p.parse_value("snare"), Some(1.0));
        assert_eq!(p.parse_value("2"), Some(2.0));
        assert_eq!(p.parse_value("99"), None);

        // Empty list is rejected without mutating state or waking host.
        assert!(!p.set_labels::<[&str; 0], _>([]));
        assert_eq!(p.len(), 3);
        assert_eq!(woken.load(Ordering::Relaxed), 0);

        // Renaming with same count fires CatalogChange::INFO only.
        assert!(p.set_labels(["808", "909", "CR78"]));
        assert_eq!(woken.load(Ordering::Relaxed), 1);
        assert_eq!(p.take_catalog_changes().flags, CatalogChange::INFO);
        assert_eq!(p.format_value(2.0), "CR78");

        // Shrinking count clamps index and fires STRUCTURE | INFO | VALUES.
        assert!(p.set_labels(["Solo"]));
        assert_eq!(woken.load(Ordering::Relaxed), 2);
        assert_eq!(p.index(), 0);
        assert_eq!(p.current_label(), "Solo");
        assert_eq!(
            p.take_catalog_changes().flags,
            CatalogChange::STRUCTURE | CatalogChange::INFO | CatalogChange::VALUES
        );
    }

    #[test]
    fn hash_param_id_is_deterministic_and_host_safe() {
        let a = crate::hash_param_id("osc1.cutoff");
        let b = crate::hash_param_id("osc1.cutoff");
        let c = crate::hash_param_id("osc2.cutoff");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a <= crate::PARAM_ID_MAX);
        assert!(c <= crate::PARAM_ID_MAX);
    }
}
