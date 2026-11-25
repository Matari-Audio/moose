//! The [`Plugin`] trait: shared, main-thread and audio parts as three types.
//!
//! A plugin is three values with three owners:
//!
//! - [`Plugin::Shared`] - params and atomics, read from any thread.
//! - [`Plugin::MainThread`] - host-thread hooks, built once per instance.
//! - [`Plugin::Processor`] - the DSP state. It exists only after a successful
//!   [`Plugin::activate`] and only the audio side holds it.
//!
//! Background tasks declared with `tasks: [..]` on `moose::plugin!` are
//! reached through the [`InitContext`] handed to [`Plugin::activate`] (stash
//! a spawner or the snapshot publisher in the processor) and through
//! `ProcessContext::tasks` in [`Plugin::process`].
//!
//! [`Lifecycle`] adapts a [`Plugin`] to [`PluginLogicCore`] and
//! [`PluginEditor`], so every format wrapper runs it unchanged through
//! `moose::plugin!`:
//!
//! ```ignore
//! moose::plugin! { plugin: MySynth, params: MySynthParams }
//! ```

use std::marker::PhantomData;
use std::sync::Arc;

use moose_core::buffer::AudioBuffer;
use moose_core::bus::BusLayout;
use moose_core::config::AudioConfig;
use moose_core::denormal::DenormalGuard;
use moose_core::editor::Editor;
use moose_core::events::EventList;
use moose_core::export::CustomClapExtension;
use moose_core::main_thread::{BusDirection, MainThreadHooks, Midi2ControllerAssignment};
use moose_core::plugin::{PreparationConfig, PreparationError};
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::state::{DecodedState, ForeignState, MigratedState, StateContext, StateLoadError};
use moose_core::tasks::InitContext;
use moose_params::Params;
use moose_params::sample::Sample;

use crate::{PluginEditor, PluginLogicCore};

/// A plugin split by thread: `Shared` params, a `MainThread` part and a `Processor`.
///
/// `S` is the audio precision of the processor: `impl Plugin for X` is `f32`,
/// `impl Plugin<f64> for X` runs `f64` buffers (or `impl Plugin<Sample>` to
/// follow the prelude). Export it with `moose::plugin!` through [`Lifecycle`].
pub trait Plugin<S: Sample = f32>: 'static {
    /// Params and atomics, shared lock-free with the host, editor and audio thread.
    type Shared: Params;
    /// The host-main-thread part (track info, note names, MIDI learn, ...).
    /// Use `()` for none.
    type MainThread: MainThreadHooks;
    /// The audio processor: filter memory, voices, buffers sized in
    /// [`Self::activate`]. Owned by the audio side between activate and
    /// deactivate.
    type Processor: Send + 'static;

    /// Resolve a custom static CLAP extension table by id.
    #[must_use]
    fn clap_extension(id: &std::ffi::CStr) -> Option<CustomClapExtension> {
        let _ = id;
        None
    }

    /// Opt into zero-copy in-place I/O: the wrapper hands host-aliased
    /// buffers through unchanged instead of copying them. Only return `true`
    /// when [`Self::process`] reads each input sample before it writes the
    /// matching output sample. Default `false`.
    #[must_use]
    fn supports_in_place() -> bool {
        false
    }

    /// Maximum serialized host state in bytes; larger loads are refused
    /// before they are admitted. Default 32 MiB.
    #[must_use]
    fn max_host_state_bytes() -> usize {
        32 * 1024 * 1024
    }

    /// Supported audio bus configurations. Default: stereo and mono effect.
    #[must_use]
    fn bus_layouts() -> Vec<BusLayout> {
        BusLayout::stereo_and_mono()
    }

    /// Worker/fallible-preparation logic must declare this requirement:
    /// return `true` when [`Self::activate`] can fail or the processor owns
    /// worker threads. Hot loaders refuse `true` until executable worker
    /// lifetime and fallible cross-generation preparation are qualified.
    /// Default `false`.
    #[must_use]
    fn requires_fallible_preparation() -> bool {
        false
    }

    /// Build the main-thread part. Called once per instance, on the host
    /// main thread.
    fn main_thread(shared: &Arc<Self::Shared>) -> Self::MainThread;

    /// Static MIDI 2.0 controller-to-parameter assignments advertised via
    /// VST3 SDK 3.8 `IMidiMapping2`. Combined with any dynamic assignments
    /// returned by [`MainThreadHooks::midi2_controller_assignments`].
    /// Default: empty.
    #[must_use]
    fn midi2_controller_assignments(direction: BusDirection) -> Vec<Midi2ControllerAssignment> {
        let _ = direction;
        Vec::new()
    }

    /// Build a processor for `config`, on the host main thread. May allocate
    /// and block.
    ///
    /// Implement this for small processors (`<= 64 KiB`), or override
    /// [`Self::activate_boxed`] for large processors (`> 64 KiB`) so
    /// `Self::Processor` is constructed directly on the heap (e.g. via
    /// `#[derive(HeapInit)]` and [`Self::create_boxed`]) without ever
    /// materializing on the host thread's stack.
    ///
    /// A host re-activation the current processor still fits (same sample
    /// rate and process mode, a max block no larger than the one it was
    /// built for) keeps it and calls [`Self::reactivate`] instead. Otherwise the
    /// previous processor is deactivated first (unless overlapping activation is enabled); the new one reads everything
    /// it needs (including `#[persist]` state) from `shared`.
    ///
    /// `cx` holds the instance's background-task spawners
    /// ([`InitContext::tasks`]) and the off-thread snapshot publisher
    /// ([`InitContext::snapshot_publisher`]); keep what the processor needs.
    ///
    /// # Errors
    ///
    /// Return a [`PreparationError`] when the configuration can't be served;
    /// the wrapper then fails the host's activation and the plugin keeps
    /// producing silence.
    fn activate(
        _shared: &Self::Shared,
        _config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<Self::Processor, PreparationError> {
        Err(PreparationError::NotReady)
    }

    /// Construct a fresh default processor directly on the heap via
    /// [`moose_core::heap_init::HeapInit`] without ever placing
    /// `Self::Processor` on the caller's stack.
    ///
    /// Intended for use inside [`Self::activate_boxed`]:
    /// ```ignore
    /// fn activate_boxed(shared: &Self::Shared, config: &AudioConfig, _cx: &InitContext)
    ///     -> Result<Box<Self::Processor>, PreparationError>
    /// {
    ///     let mut proc = Self::create_boxed();
    ///     proc.configure(shared, config);
    ///     Ok(proc)
    /// }
    /// ```
    #[must_use]
    fn create_boxed() -> Box<Self::Processor>
    where
        Self::Processor: moose_core::heap_init::HeapInit,
    {
        <Self::Processor as moose_core::heap_init::HeapInit>::heap_init()
    }

    /// Build a boxed processor for `config` directly on the heap on the host
    /// main thread, without returning `Self::Processor` by value on the stack.
    ///
    /// Large processors (`> 64 KiB`) should override this method (and
    /// [`Self::deactivate_boxed`] if needed) instead of [`Self::activate`],
    /// constructing the processor via [`Self::create_boxed`] /
    /// `#[derive(HeapInit)]`.
    ///
    /// Default: delegates to [`Self::activate`] and boxes the result.
    ///
    /// # Errors
    ///
    /// Return a [`PreparationError`] when the configuration can't be served.
    fn activate_boxed(
        shared: &Self::Shared,
        config: &AudioConfig,
        cx: &InitContext,
    ) -> Result<Box<Self::Processor>, PreparationError> {
        Self::activate(shared, config, cx).map(Box::new)
    }

    /// A host re-activation the processor still fits (see
    /// [`Self::activate`]), on the host main thread: may allocate and block.
    /// Redo what `activate` reads from `shared` at activation time (a mode
    /// that latency depends on, for instance), then clear to silence.
    /// Default: [`Self::reset`].
    fn reactivate(processor: &mut Self::Processor, shared: &Self::Shared, config: &AudioConfig) {
        let _ = config;
        Self::reset(processor, shared);
    }

    /// Clear the processor back to silence. Runs on the audio thread (CLAP
    /// `reset`, VST3 `setProcessing(false)`): no allocation, locks or
    /// blocking. Default: no-op.
    fn reset(processor: &mut Self::Processor, shared: &Self::Shared) {
        let _ = (processor, shared);
    }

    /// Process one block. Real-time: no allocation, locks or I/O.
    fn process(
        processor: &mut Self::Processor,
        shared: &Self::Shared,
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    /// Take the processor back, on the host main thread, before a new
    /// activation replaces it. Default: drop it.
    ///
    /// For large processors (`> 64 KiB`), override [`Self::deactivate_boxed`]
    /// so teardown never unboxes `Self::Processor` onto the host thread's stack.
    fn deactivate(processor: Self::Processor) {
        drop(processor);
    }

    /// Take the boxed processor back on the host main thread before replacement
    /// or teardown, without unboxing `Self::Processor` onto the caller's stack.
    ///
    /// Large processors (`> 64 KiB`) should override this (even with a simple
    /// `drop(processor)`) when overriding [`Self::activate_boxed`].
    /// Default: delegates to [`Self::deactivate`] (`*processor`).
    #[allow(clippy::boxed_local)]
    fn deactivate_boxed(processor: Box<Self::Processor>) {
        Self::deactivate(*processor);
    }

    /// Latency in samples for delay compensation.
    fn latency(processor: &Self::Processor) -> u32 {
        let _ = processor;
        0
    }

    /// Tail in samples after input stops (`u32::MAX` for infinite).
    fn tail(processor: &Self::Processor) -> u32 {
        let _ = processor;
        0
    }

    /// Called after a host state load lands on the processor, on the thread
    /// that applied it, with the loaded params already in `shared`. Recompute
    /// caches the next `process` reads (snap smoothers, for instance).
    /// Default: no-op.
    ///
    /// **RT contract:** plugins that opt into off-thread decoding
    /// ([`Self::decode_state`] returning `Ok(Some(_))`) invoke `state_changed`
    /// inside an `RtSection` on the audio thread right after
    /// [`Self::apply_decoded_state`], so `state_changed` must not allocate,
    /// lock, or block on that path. Plugins returning `Ok(None)` keep the
    /// legacy non-RT `load_state` path.
    ///
    /// Custom state lives in `#[persist]` fields on [`Self::Shared`].
    fn state_changed(processor: &mut Self::Processor, shared: &Self::Shared) {
        let _ = (processor, shared);
    }

    /// Context-aware variant of [`Self::state_changed`], receiving the
    /// host-reported [`StateContext`]. Default delegates to [`Self::state_changed`].
    fn state_changed_with_context(
        processor: &mut Self::Processor,
        shared: &Self::Shared,
        _context: StateContext,
    ) {
        Self::state_changed(processor, shared);
    }

    /// Save optional processor-owned extra state bytes with host-reported
    /// [`StateContext`], called strictly on the host/main thread (never on the
    /// audio thread). Return `Some(bytes)` to supply a context-specific payload
    /// (including `Some(Vec::new())` to intentionally omit extra state for a
    /// context such as `StateContext::Preset`). For context-specific saves while
    /// audio is actively processing, publish off-thread via
    /// `SnapshotPublisher::publish_for_context`. Default returns `None`,
    /// preserving the snapshot / `save_state` fallback.
    fn save_state_with_context(
        processor: &Self::Processor,
        _context: StateContext,
    ) -> Option<Vec<u8>> {
        let _ = processor;
        None
    }

    /// Load optional processor-owned extra state bytes with host-reported
    /// [`StateContext`]. Default returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// Return [`StateLoadError`] when the payload cannot be loaded.
    fn load_state_with_context(
        processor: &mut Self::Processor,
        data: &[u8],
        _context: StateContext,
    ) -> Result<(), StateLoadError> {
        let _ = (processor, data);
        Ok(())
    }

    /// Allow a replacement to activate while its predecessor is still alive.
    /// Opt in only when both processors can hold resources concurrently. By default
    /// teardown runs first: retain settings needed by the replacement in Shared.
    const OVERLAPPING_ACTIVATION: bool = false;

    /// Transfer processor-owned state (such as state previously swapped in via
    /// [`Self::apply_decoded_state`]) from an outgoing processor to its
    /// replacement when [`Self::activate`] rebuilds the processor for a new
    /// [`AudioConfig`] (e.g. a sample-rate or max-block-size change).
    ///
    /// Only called with `OVERLAPPING_ACTIVATION` enabled, on the host main thread.
    /// Plugins whose custom state lives solely
    /// in `#[persist]` fields on [`Self::Shared`] do not need to override this.
    /// Default: no-op.
    fn transfer_state(old: &mut Self::Processor, new: &mut Self::Processor, shared: &Self::Shared) {
        let _ = (old, new, shared);
    }

    /// Check custom state bytes on the host thread before they are applied.
    ///
    /// # Errors
    ///
    /// The bytes are not a state this plugin can load.
    fn validate_state(data: &[u8]) -> Result<(), StateLoadError> {
        let _ = data;
        Ok(())
    }

    /// Context-aware variant of [`Self::validate_state`]. Default delegates
    /// to [`Self::validate_state`].
    ///
    /// # Errors
    ///
    /// The bytes are not a state this plugin can load in `context`.
    fn validate_state_with_context(
        data: &[u8],
        _context: StateContext,
    ) -> Result<(), StateLoadError> {
        Self::validate_state(data)
    }

    /// Decode custom state bytes off the audio thread into a ready-to-swap
    /// [`DecodedState`].
    ///
    /// Opt-in zero-allocation RT state swap: return `Ok(Some(DecodedState::new(val)))`
    /// here and swap `val` into your processor inside [`Self::apply_decoded_state`]
    /// via [`DecodedState::swap_with`]. The displaced old state is retired and
    /// dropped off the audio thread. Default returns `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns `Err(StateLoadError)` when the bytes cannot be decoded.
    fn decode_state(data: &[u8]) -> Result<Option<DecodedState>, StateLoadError> {
        let _ = data;
        Ok(None)
    }

    /// Context-aware variant of [`Self::decode_state`]. Default delegates
    /// to [`Self::decode_state`].
    ///
    /// # Errors
    ///
    /// Returns `Err(StateLoadError)` when the bytes cannot be decoded in `context`.
    fn decode_state_with_context(
        data: &[u8],
        _context: StateContext,
    ) -> Result<Option<DecodedState>, StateLoadError> {
        Self::decode_state(data)
    }

    /// Swap a state decoded by [`Self::decode_state`] into the active processor.
    ///
    /// Runs inside an `RtSection` on the audio thread: must not allocate,
    /// deallocate, lock, or block. Use [`DecodedState::swap_with`] to swap
    /// in-place so the previous state drops off the audio thread.
    fn apply_decoded_state(
        processor: &mut Self::Processor,
        shared: &Self::Shared,
        decoded: &mut DecodedState,
    ) {
        let _ = (processor, shared, decoded);
    }

    /// Context-aware variant of [`Self::apply_decoded_state`]. Default
    /// delegates to [`Self::apply_decoded_state`].
    fn apply_decoded_state_with_context(
        processor: &mut Self::Processor,
        shared: &Self::Shared,
        decoded: &mut DecodedState,
        _context: StateContext,
    ) {
        Self::apply_decoded_state(processor, shared, decoded);
    }

    /// Decode persisted (`#[persist]`) state off the audio thread into a
    /// heap-allocated [`DecodedState`] for RT-safe swapping in
    /// [`Self::apply_decoded_persist`].
    ///
    /// Because `Lifecycle<P>` keeps plugin state in `#[persist]` fields on
    /// `Self::Shared` (leaving the custom `extra` blob empty), `decode_state`
    /// never runs for `Plugin` implementations. Override this hook to build
    /// large derived DSP tables heap-in-place off-thread (e.g. via
    /// `#[derive(HeapInit)]`) from `shared` / `persist_bytes` without ever
    /// going through the host thread's stack or allocating on the audio thread.
    ///
    /// # Errors
    ///
    /// Return [`StateLoadError`] when persisted state cannot be decoded;
    /// parameters and persisted fields will be rolled back.
    fn decode_persist_state(
        shared: &Self::Shared,
        persist_bytes: &[u8],
        context: StateContext,
    ) -> Result<Option<DecodedState>, StateLoadError> {
        let _ = (shared, persist_bytes, context);
        Ok(None)
    }

    /// Swap a payload decoded by [`Self::decode_persist_state`] into the active
    /// processor inside an `RtSection` on the audio thread (zero allocations,
    /// zero deallocations, zero locks).
    ///
    /// Use [`DecodedState::swap_with`] so the previous value is retired back to
    /// the host thread to drop off the audio thread.
    fn apply_decoded_persist(
        processor: &mut Self::Processor,
        shared: &Self::Shared,
        decoded: &mut DecodedState,
        context: StateContext,
    ) {
        let _ = (processor, shared, decoded, context);
    }

    /// Translate foreign state (a previous framework's blob or a renamed
    /// plugin's envelope) into moose params, extra, and persist bytes.
    #[must_use]
    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState> {
        let _ = foreign;
        None
    }

    /// Plugin-defined state schema version (`0` preserves legacy OAST v1;
    /// `>= 1` writes OAST v2 carrying stable string parameter IDs).
    #[must_use]
    fn state_schema_version() -> u32 {
        0
    }

    /// Composable `v -> v + 1` migration chain upgrading older state envelopes
    /// up to [`Self::state_schema_version`].
    #[must_use]
    fn state_migrations() -> moose_core::state::StateMigrationChain {
        moose_core::state::StateMigrationChain::new(Self::state_schema_version())
    }

    /// Build the editor from the shared part.
    ///
    /// Defaults to [`moose_core::editor::NoEditor`] for headless plugins.
    fn editor(_shared: Arc<Self::Shared>) -> Box<dyn Editor> {
        Box::new(moose_core::editor::NoEditor)
    }
}

/// Runs a [`Plugin`] through the format wrappers (`moose::plugin!`).
///
/// A type-level marker, never constructed: it implements [`PluginLogicCore`]
/// and [`PluginEditor`] for any `P: Plugin<S>`, with [`ProcessorSlot`] as
/// the shell-owned DSP state.
pub struct Lifecycle<P>(PhantomData<fn() -> P>);

impl<P> std::fmt::Debug for Lifecycle<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Lifecycle")
    }
}

/// The DSP state [`Lifecycle`] hands the shell: a processor once
/// activated, any pre-activation decoded or raw state waiting for the first
/// activation, and the instance's [`InitContext`] for each activation.
pub struct ProcessorSlot<T> {
    slot: Slot<T>,
    pending_decoded: Option<(DecodedState, StateContext)>,
    pending_decoded_persist: Option<(DecodedState, StateContext)>,
    pending_raw: Option<(Vec<u8>, StateContext)>,
    cx: InitContext,
}

impl<T> Default for ProcessorSlot<T> {
    fn default() -> Self {
        Self {
            slot: Slot::default(),
            pending_decoded: None,
            pending_decoded_persist: None,
            pending_raw: None,
            cx: InitContext::default(),
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ProcessorSlot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessorSlot")
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
enum Slot<T> {
    /// Not activated.
    #[default]
    Inactive,
    /// Previous processor parked after a failed replacement `activate` so its
    /// state can still transfer on a subsequent retry; produces silence in
    /// `process` until a new `activate` succeeds.
    ///
    /// Boxed so `ProcessorSlot<T>` and `activate`'s stack frame stay small even
    /// for large DSP structs on constrained host/test thread stacks.
    Parked(Box<T>),
    /// Built by `activate` for `config`.
    Active {
        processor: Box<T>,
        config: AudioConfig,
    },
}

impl<T> ProcessorSlot<T> {
    /// The processor, once [`Plugin::activate`] has succeeded.
    #[must_use]
    pub fn processor(&self) -> Option<&T> {
        match &self.slot {
            Slot::Active { processor, .. } => Some(processor.as_ref()),
            Slot::Inactive | Slot::Parked(_) => None,
        }
    }

    /// Mutable access to the processor, once activated.
    pub fn processor_mut(&mut self) -> Option<&mut T> {
        match &mut self.slot {
            Slot::Active { processor, .. } => Some(processor.as_mut()),
            Slot::Inactive | Slot::Parked(_) => None,
        }
    }
}

#[inline(never)]
fn activate_boxed<S: Sample, P: Plugin<S>>(
    shared: &P::Shared,
    audio: &AudioConfig,
    cx: &InitContext,
) -> Result<Box<P::Processor>, PreparationError> {
    P::activate_boxed(shared, audio, cx)
}

#[inline(never)]
#[allow(clippy::boxed_local)]
fn deactivate_boxed<S: Sample, P: Plugin<S>>(processor: Box<P::Processor>) {
    P::deactivate_boxed(processor);
}

fn activate<S: Sample, P: Plugin<S>>(
    slot: &mut ProcessorSlot<P::Processor>,
    shared: &P::Shared,
    config: &PreparationConfig,
) -> Result<(), PreparationError> {
    config.validate()?;
    // The activation's host identity is the freshest (a VST3 host may name
    // itself only after instantiation); `activate` reads it from `cx`.
    if let Some(host) = &config.host {
        slot.cx.set_host(host.clone());
    }
    if let Slot::Active {
        processor,
        config: built,
    } = &mut slot.slot
        && fits(built, &config.audio)
    {
        P::reactivate(processor, shared, &config.audio);
        return Ok(());
    }
    if !P::OVERLAPPING_ACTIVATION
        && let Slot::Active { processor, .. } = std::mem::take(&mut slot.slot)
    {
        deactivate_boxed::<S, P>(processor);
    }
    let mut processor = match activate_boxed::<S, P>(shared, &config.audio, &slot.cx) {
        Ok(processor) => processor,
        Err(err) => {
            // Replacement failed: park any previously active processor so
            // `process` outputs silence (and `processor()` is `None`), while
            // preserving its state for a subsequent retry.
            if matches!(slot.slot, Slot::Active { .. })
                && let Slot::Active { processor: old, .. } = std::mem::take(&mut slot.slot)
            {
                slot.slot = Slot::Parked(old);
            }
            return Err(err);
        }
    };
    if let Slot::Active {
        processor: mut old, ..
    }
    | Slot::Parked(mut old) = std::mem::take(&mut slot.slot)
    {
        P::transfer_state(&mut old, &mut processor, shared);
        deactivate_boxed::<S, P>(old);
    }
    if let Some((mut pending_persist, context)) = slot.pending_decoded_persist.take() {
        P::apply_decoded_persist(&mut processor, shared, &mut pending_persist, context);
        P::state_changed_with_context(&mut processor, shared, context);
    }
    if let Some((mut pending, context)) = slot.pending_decoded.take() {
        P::apply_decoded_state_with_context(&mut processor, shared, &mut pending, context);
        P::state_changed_with_context(&mut processor, shared, context);
    } else if let Some((pending_bytes, context)) = slot.pending_raw.take() {
        if P::load_state_with_context(&mut processor, &pending_bytes, context).is_err() {
            slot.pending_raw = Some((pending_bytes, context));
            deactivate_boxed::<S, P>(processor);
            return Err(PreparationError::NotReady);
        }
        P::state_changed_with_context(&mut processor, shared, context);
    }
    slot.slot = Slot::Active {
        processor,
        config: config.audio,
    };
    Ok(())
}

/// A processor built for `built` can serve `wanted` unchanged.
fn fits(built: &AudioConfig, wanted: &AudioConfig) -> bool {
    built.sample_rate.to_bits() == wanted.sample_rate.to_bits()
        && built.process_mode == wanted.process_mode
        && wanted.max_block_size <= built.max_block_size
}

fn process<S: Sample, P: Plugin<S>>(
    slot: &mut ProcessorSlot<P::Processor>,
    shared: &P::Shared,
    buffer: &mut AudioBuffer<S>,
    events: &EventList,
    context: &mut ProcessContext,
) -> ProcessStatus {
    if let Slot::Active { processor, .. } = &mut slot.slot {
        return P::process(processor, shared, buffer, events, context);
    }
    for ch in 0..buffer.num_output_channels() {
        buffer.output(ch).fill(S::default());
    }
    ProcessStatus::Normal
}

impl<S: Sample, P: Plugin<S>> PluginLogicCore<S> for Lifecycle<P> {
    type Params = P::Shared;
    type DspState = ProcessorSlot<P::Processor>;

    fn clap_extension(id: &std::ffi::CStr) -> Option<CustomClapExtension> {
        P::clap_extension(id)
    }

    fn supports_in_place() -> bool {
        P::supports_in_place()
    }

    fn max_host_state_bytes() -> usize {
        P::max_host_state_bytes()
    }

    fn bus_layouts() -> Vec<BusLayout> {
        P::bus_layouts()
    }

    fn requires_fallible_preparation() -> bool {
        P::requires_fallible_preparation()
    }

    fn init(_params: &P::Shared, cx: &InitContext) -> Self::DspState {
        ProcessorSlot {
            slot: Slot::default(),
            pending_decoded: None,
            pending_decoded_persist: None,
            pending_raw: None,
            cx: cx.clone(),
        }
    }

    fn main_thread(params: &Arc<P::Shared>) -> Box<dyn MainThreadHooks> {
        Box::new(P::main_thread(params))
    }

    fn midi2_controller_assignments(direction: BusDirection) -> Vec<Midi2ControllerAssignment> {
        P::midi2_controller_assignments(direction)
    }

    fn reset(state: &mut Self::DspState, params: &P::Shared, _config: &AudioConfig) {
        if let Some(processor) = state.processor_mut() {
            P::reset(processor, params);
        }
    }

    fn prepare(
        state: &mut Self::DspState,
        params: &P::Shared,
        config: &PreparationConfig,
    ) -> Result<(), PreparationError> {
        activate::<S, P>(state, params, config)
    }

    fn process(
        state: &mut Self::DspState,
        params: &P::Shared,
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // FTZ/DAZ (or FZ on AArch64) for the duration of the plugin's
        // process body so denormals on feedback paths never stall the core.
        let _denormal_guard = DenormalGuard::new();
        process::<S, P>(state, params, buffer, events, context)
    }

    fn save_state_with_context(state: &Self::DspState, context: StateContext) -> Option<Vec<u8>> {
        state
            .processor()
            .and_then(|p| P::save_state_with_context(p, context))
    }

    fn load_state_with_context(
        state: &mut Self::DspState,
        data: &[u8],
        context: StateContext,
    ) -> Result<(), StateLoadError> {
        if let Some(processor) = state.processor_mut() {
            P::load_state_with_context(processor, data, context)
        } else if data.is_empty()
            && (state.pending_decoded.is_some() || state.pending_raw.is_some())
        {
            // Empty bytes mean "no custom state" (parameter-only load): keep
            // the custom state still waiting for activation.
            Ok(())
        } else {
            state.pending_decoded = None;
            state.pending_raw = Some((data.to_vec(), context));
            Ok(())
        }
    }

    fn state_changed(state: &mut Self::DspState, params: &P::Shared) {
        if let Some(processor) = state.processor_mut() {
            P::state_changed(processor, params);
        }
    }

    fn state_changed_with_context(
        state: &mut Self::DspState,
        params: &P::Shared,
        context: StateContext,
    ) {
        if let Some(processor) = state.processor_mut() {
            P::state_changed_with_context(processor, params, context);
        }
    }

    fn validate_state(data: &[u8]) -> Result<(), StateLoadError> {
        P::validate_state(data)
    }

    fn validate_state_with_context(
        data: &[u8],
        context: StateContext,
    ) -> Result<(), StateLoadError> {
        P::validate_state_with_context(data, context)
    }

    fn decode_state(data: &[u8]) -> Result<Option<DecodedState>, StateLoadError> {
        P::decode_state(data)
    }

    fn decode_state_with_context(
        data: &[u8],
        context: StateContext,
    ) -> Result<Option<DecodedState>, StateLoadError> {
        P::decode_state_with_context(data, context)
    }

    fn apply_decoded_state(
        state: &mut Self::DspState,
        params: &P::Shared,
        decoded: &mut DecodedState,
    ) {
        Self::apply_decoded_state_with_context(state, params, decoded, StateContext::Unknown);
    }

    fn apply_decoded_state_with_context(
        state: &mut Self::DspState,
        params: &P::Shared,
        decoded: &mut DecodedState,
        context: StateContext,
    ) {
        if let Some(processor) = state.processor_mut() {
            P::apply_decoded_state_with_context(processor, params, decoded, context);
        } else {
            state.pending_raw = None;
            if let Some((prev, prev_ctx)) = &mut state.pending_decoded {
                std::mem::swap(prev, decoded);
                *prev_ctx = context;
            } else {
                state.pending_decoded =
                    Some((std::mem::replace(decoded, DecodedState::empty()), context));
            }
        }
    }

    fn decode_persist_state(
        params: &P::Shared,
        persist: &[u8],
        context: StateContext,
    ) -> Result<Option<DecodedState>, StateLoadError> {
        P::decode_persist_state(params, persist, context)
    }

    fn apply_decoded_persist(
        state: &mut Self::DspState,
        params: &P::Shared,
        decoded: &mut DecodedState,
        context: StateContext,
    ) {
        if let Some(processor) = state.processor_mut() {
            P::apply_decoded_persist(processor, params, decoded, context);
        } else if let Some((prev, prev_ctx)) = &mut state.pending_decoded_persist {
            std::mem::swap(prev, decoded);
            *prev_ctx = context;
        } else {
            state.pending_decoded_persist =
                Some((std::mem::replace(decoded, DecodedState::empty()), context));
        }
    }

    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState> {
        P::migrate_state(foreign)
    }

    fn state_schema_version() -> u32 {
        P::state_schema_version()
    }

    fn state_migrations() -> moose_core::state::StateMigrationChain {
        P::state_migrations()
    }

    fn latency(state: &Self::DspState) -> u32 {
        state.processor().map_or(0, P::latency)
    }

    fn tail(state: &Self::DspState) -> u32 {
        state.processor().map_or(0, P::tail)
    }
}

impl<S: Sample, P: Plugin<S>> PluginEditor<S> for Lifecycle<P> {
    type Params = P::Shared;

    fn editor(params: Arc<P::Shared>) -> Box<dyn Editor> {
        P::editor(params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moose_core::plugin::PreparationFrameBound;
    use moose_params::DynamicParams;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    static REFUSE_ACTIVATE: AtomicBool = AtomicBool::new(false);
    static REACTIVATE_CALLS: AtomicU32 = AtomicU32::new(0);
    static TRANSFER_CALLS: AtomicU32 = AtomicU32::new(0);

    struct PreActivatePlugin;

    #[derive(Default)]
    struct PreActivateProcessor {
        loaded_tag: u32,
        state_changed_calls: u32,
    }

    impl Plugin for PreActivatePlugin {
        const OVERLAPPING_ACTIVATION: bool = true;
        type Shared = DynamicParams<0>;
        type MainThread = ();
        type Processor = PreActivateProcessor;

        fn main_thread(_shared: &Arc<Self::Shared>) -> Self::MainThread {}

        fn activate(
            _shared: &Self::Shared,
            _config: &AudioConfig,
            _cx: &InitContext,
        ) -> Result<Self::Processor, PreparationError> {
            if REFUSE_ACTIVATE.load(Ordering::Acquire) {
                return Err(PreparationError::ResourceBudget);
            }
            Ok(PreActivateProcessor::default())
        }

        fn reactivate(
            processor: &mut Self::Processor,
            shared: &Self::Shared,
            config: &AudioConfig,
        ) {
            let _ = config;
            REACTIVATE_CALLS.fetch_add(1, Ordering::AcqRel);
            Self::reset(processor, shared);
        }

        fn transfer_state(
            old: &mut Self::Processor,
            new: &mut Self::Processor,
            _shared: &Self::Shared,
        ) {
            TRANSFER_CALLS.fetch_add(1, Ordering::AcqRel);
            new.loaded_tag = old.loaded_tag;
        }

        fn process(
            _processor: &mut Self::Processor,
            _shared: &Self::Shared,
            _buffer: &mut AudioBuffer<f32>,
            _events: &EventList,
            _context: &mut ProcessContext,
        ) -> ProcessStatus {
            ProcessStatus::Normal
        }

        fn decode_state(data: &[u8]) -> Result<Option<DecodedState>, StateLoadError> {
            if data.len() == 4 {
                let tag = u32::from_le_bytes(data.try_into().unwrap());
                return Ok(Some(DecodedState::new(tag)));
            }
            Ok(None)
        }

        fn apply_decoded_state(
            processor: &mut Self::Processor,
            _shared: &Self::Shared,
            decoded: &mut DecodedState,
        ) {
            decoded.swap_with(&mut processor.loaded_tag);
        }

        fn state_changed(processor: &mut Self::Processor, _shared: &Self::Shared) {
            processor.state_changed_calls += 1;
        }
    }

    #[test]
    fn default_replacement_releases_exclusive_resource_before_activate() {
        struct Exclusive;
        static RESERVED: AtomicBool = AtomicBool::new(false);
        impl Plugin for Exclusive {
            type Processor = ();
            type Shared = DynamicParams<0>;
            type MainThread = ();

            fn main_thread(_: &Arc<Self::Shared>) {}
            fn activate(
                _: &Self::Shared,
                _: &AudioConfig,
                _: &InitContext,
            ) -> Result<(), PreparationError> {
                if RESERVED.swap(true, Ordering::AcqRel) {
                    return Err(PreparationError::ResourceBudget);
                }
                Ok(())
            }
            fn deactivate((): ()) {
                assert!(RESERVED.swap(false, Ordering::AcqRel));
            }
            fn process(
                (): &mut (),
                _: &Self::Shared,
                _: &mut AudioBuffer<f32>,
                _: &EventList,
                _: &mut ProcessContext,
            ) -> ProcessStatus {
                ProcessStatus::Normal
            }
        }
        let shared = DynamicParams::<0>::new();
        let mut slot =
            <Lifecycle<Exclusive> as PluginLogicCore>::init(&shared, &InitContext::default());
        for rate in [48_000.0, 96_000.0] {
            let prep = PreparationConfig::new(
                AudioConfig::new(rate, 512),
                PreparationFrameBound::NegotiatedHost,
            );
            activate::<f32, Exclusive>(&mut slot, &shared, &prep).unwrap();
        }
    }

    #[test]
    fn pre_activation_decoded_state_survives_activate_reactivate_replacement_and_failed_retry() {
        REFUSE_ACTIVATE.store(false, Ordering::Release);
        REACTIVATE_CALLS.store(0, Ordering::Release);
        TRANSFER_CALLS.store(0, Ordering::Release);

        let shared = DynamicParams::<0>::new();
        let mut slot = <Lifecycle<PreActivatePlugin> as PluginLogicCore>::init(
            &shared,
            &InitContext::default(),
        );
        assert!(slot.processor().is_none());

        let mut first =
            <Lifecycle<PreActivatePlugin> as PluginLogicCore>::decode_state(&7u32.to_le_bytes())
                .unwrap()
                .unwrap();
        <Lifecycle<PreActivatePlugin> as PluginLogicCore>::apply_decoded_state(
            &mut slot, &shared, &mut first,
        );

        let mut second =
            <Lifecycle<PreActivatePlugin> as PluginLogicCore>::decode_state(&42u32.to_le_bytes())
                .unwrap()
                .unwrap();
        <Lifecycle<PreActivatePlugin> as PluginLogicCore>::apply_decoded_state(
            &mut slot,
            &shared,
            &mut second,
        );
        // The older pre-activation decoded state is swapped out into `second`
        // so callers can retire it off the audio thread.
        assert_eq!(second.downcast_ref::<u32>(), Some(&7));

        // 1. First activation at 48 kHz / 512 receives 42.
        let prep_48k = PreparationConfig::new(
            AudioConfig::new(48_000.0, 512),
            PreparationFrameBound::NegotiatedHost,
        );
        <Lifecycle<PreActivatePlugin> as PluginLogicCore>::prepare(&mut slot, &shared, &prep_48k)
            .expect("first activation succeeds");
        let processor = slot.processor().expect("processor active");
        assert_eq!(processor.loaded_tag, 42);
        assert_eq!(processor.state_changed_calls, 1);

        // 2. Fitting reactivation (48 kHz / 256) keeps processor & tag without rebuilding.
        let prep_fit = PreparationConfig::new(
            AudioConfig::new(48_000.0, 256),
            PreparationFrameBound::NegotiatedHost,
        );
        <Lifecycle<PreActivatePlugin> as PluginLogicCore>::prepare(&mut slot, &shared, &prep_fit)
            .expect("fitting reactivation succeeds");
        assert_eq!(REACTIVATE_CALLS.load(Ordering::Acquire), 1);
        assert_eq!(TRANSFER_CALLS.load(Ordering::Acquire), 0);
        assert_eq!(slot.processor().expect("active").loaded_tag, 42);

        // 3. Config-change replacement (96 kHz / 512) transfers tag 42 to the new processor.
        let prep_96k = PreparationConfig::new(
            AudioConfig::new(96_000.0, 512),
            PreparationFrameBound::NegotiatedHost,
        );
        <Lifecycle<PreActivatePlugin> as PluginLogicCore>::prepare(&mut slot, &shared, &prep_96k)
            .expect("96 kHz replacement succeeds");
        assert_eq!(TRANSFER_CALLS.load(Ordering::Acquire), 1);
        assert_eq!(slot.processor().expect("active").loaded_tag, 42);

        // 4. Failed replacement (192 kHz) parks the old processor (processor() == None)
        //    and preserves tag 42 across the subsequent retry.
        REFUSE_ACTIVATE.store(true, Ordering::Release);
        let prep_192k = PreparationConfig::new(
            AudioConfig::new(192_000.0, 512),
            PreparationFrameBound::NegotiatedHost,
        );
        assert_eq!(
            <Lifecycle<PreActivatePlugin> as PluginLogicCore>::prepare(
                &mut slot, &shared, &prep_192k,
            ),
            Err(PreparationError::ResourceBudget)
        );
        assert_eq!(
            <Lifecycle<PreActivatePlugin> as PluginLogicCore>::prepare(
                &mut slot, &shared, &prep_192k
            ),
            Err(PreparationError::ResourceBudget)
        );
        assert!(
            slot.processor().is_none(),
            "failed replacement must not expose an active processor"
        );

        REFUSE_ACTIVATE.store(false, Ordering::Release);
        <Lifecycle<PreActivatePlugin> as PluginLogicCore>::prepare(&mut slot, &shared, &prep_192k)
            .expect("retry after failed replacement succeeds");
        assert_eq!(TRANSFER_CALLS.load(Ordering::Acquire), 2);
        assert_eq!(slot.processor().expect("active").loaded_tag, 42);
    }

    #[test]
    fn processor_slot_stays_small_for_large_dsp_structs() {
        assert!(
            std::mem::size_of::<ProcessorSlot<[u8; 65_536]>>() < 1024,
            "ProcessorSlot must box P::Processor so activate() and host/test stacks stay small"
        );
    }

    struct ContextPlugin;

    #[derive(Default)]
    struct ContextProcessor {
        loaded_context: Option<StateContext>,
        applied_context: Option<StateContext>,
        changed_context: Option<StateContext>,
        tag: u32,
    }

    impl Plugin for ContextPlugin {
        type Shared = DynamicParams<0>;
        type MainThread = ();
        type Processor = ContextProcessor;

        fn main_thread(_shared: &Arc<Self::Shared>) -> Self::MainThread {}

        fn midi2_controller_assignments(direction: BusDirection) -> Vec<Midi2ControllerAssignment> {
            use moose_core::main_thread::{Midi2Controller, Midi2ControllerKind};
            if direction == BusDirection::Input {
                vec![Midi2ControllerAssignment {
                    param_id: 7,
                    bus_index: 0,
                    channel: 2,
                    controller: Midi2Controller {
                        kind: Midi2ControllerKind::Registered,
                        bank: 1,
                        index: 5,
                    },
                }]
            } else {
                Vec::new()
            }
        }

        fn activate(
            _shared: &Self::Shared,
            _config: &AudioConfig,
            _cx: &InitContext,
        ) -> Result<Self::Processor, PreparationError> {
            Ok(ContextProcessor::default())
        }

        fn process(
            _processor: &mut Self::Processor,
            _shared: &Self::Shared,
            _buffer: &mut AudioBuffer<f32>,
            _events: &EventList,
            _context: &mut ProcessContext,
        ) -> ProcessStatus {
            ProcessStatus::Normal
        }

        fn save_state_with_context(
            _processor: &Self::Processor,
            context: StateContext,
        ) -> Option<Vec<u8>> {
            Some(match context {
                StateContext::Project => b"proj".to_vec(),
                StateContext::Preset => b"pres".to_vec(),
                _ => b"unk".to_vec(),
            })
        }

        fn load_state_with_context(
            processor: &mut Self::Processor,
            _data: &[u8],
            context: StateContext,
        ) -> Result<(), StateLoadError> {
            processor.loaded_context = Some(context);
            Ok(())
        }

        fn validate_state_with_context(
            data: &[u8],
            context: StateContext,
        ) -> Result<(), StateLoadError> {
            if context == StateContext::Preset && data == b"bad-preset" {
                return Err(StateLoadError::Malformed("bad preset"));
            }
            Ok(())
        }

        fn decode_state_with_context(
            data: &[u8],
            context: StateContext,
        ) -> Result<Option<DecodedState>, StateLoadError> {
            if data.len() == 4 {
                let base = u32::from_le_bytes(data.try_into().unwrap());
                let tag = if context == StateContext::Duplicate {
                    base + 100
                } else {
                    base
                };
                return Ok(Some(DecodedState::new(tag)));
            }
            Ok(None)
        }

        fn apply_decoded_state_with_context(
            processor: &mut Self::Processor,
            _shared: &Self::Shared,
            decoded: &mut DecodedState,
            context: StateContext,
        ) {
            decoded.swap_with(&mut processor.tag);
            processor.applied_context = Some(context);
        }

        fn state_changed_with_context(
            processor: &mut Self::Processor,
            _shared: &Self::Shared,
            context: StateContext,
        ) {
            processor.changed_context = Some(context);
        }
    }

    #[test]
    fn lifecycle_forwards_state_context_and_midi2_assignments() {
        assert_eq!(
            <Lifecycle<ContextPlugin> as PluginLogicCore>::midi2_controller_assignments(
                BusDirection::Input
            )
            .len(),
            1
        );
        assert_eq!(
            <Lifecycle<ContextPlugin> as PluginLogicCore>::midi2_controller_assignments(
                BusDirection::Output
            ),
            Vec::new()
        );

        assert!(
            <Lifecycle<ContextPlugin> as PluginLogicCore>::validate_state_with_context(
                b"bad-preset",
                StateContext::Preset
            )
            .is_err()
        );
        assert!(
            <Lifecycle<ContextPlugin> as PluginLogicCore>::validate_state_with_context(
                b"bad-preset",
                StateContext::Project
            )
            .is_ok()
        );

        let shared = DynamicParams::<0>::new();
        let mut slot =
            <Lifecycle<ContextPlugin> as PluginLogicCore>::init(&shared, &InitContext::default());

        let mut decoded = <Lifecycle<ContextPlugin> as PluginLogicCore>::decode_state_with_context(
            &5u32.to_le_bytes(),
            StateContext::Duplicate,
        )
        .unwrap()
        .unwrap();
        <Lifecycle<ContextPlugin> as PluginLogicCore>::apply_decoded_state_with_context(
            &mut slot,
            &shared,
            &mut decoded,
            StateContext::Duplicate,
        );

        let prep = PreparationConfig::new(
            AudioConfig::new(48_000.0, 256),
            PreparationFrameBound::NegotiatedHost,
        );
        <Lifecycle<ContextPlugin> as PluginLogicCore>::prepare(&mut slot, &shared, &prep)
            .expect("activate");
        let p = slot.processor().expect("active");
        assert_eq!(p.tag, 105);
        assert_eq!(p.applied_context, Some(StateContext::Duplicate));
        assert_eq!(p.changed_context, Some(StateContext::Duplicate));

        assert_eq!(
            <Lifecycle<ContextPlugin> as PluginLogicCore>::save_state_with_context(
                &slot,
                StateContext::Project
            ),
            Some(b"proj".to_vec())
        );
        <Lifecycle<ContextPlugin> as PluginLogicCore>::load_state_with_context(
            &mut slot,
            b"pres",
            StateContext::Preset,
        )
        .unwrap();
        assert_eq!(
            slot.processor().expect("active").loaded_context,
            Some(StateContext::Preset)
        );
    }

    #[test]
    fn lifecycle_buffers_pre_activation_raw_load_state_with_context() {
        let shared = DynamicParams::<0>::new();
        let mut slot =
            <Lifecycle<ContextPlugin> as PluginLogicCore>::init(&shared, &InitContext::default());
        assert!(slot.processor().is_none());

        <Lifecycle<ContextPlugin> as PluginLogicCore>::load_state_with_context(
            &mut slot,
            b"raw-preset-payload",
            StateContext::Preset,
        )
        .unwrap();

        let prep = PreparationConfig::new(
            AudioConfig::new(48_000.0, 256),
            PreparationFrameBound::NegotiatedHost,
        );
        <Lifecycle<ContextPlugin> as PluginLogicCore>::prepare(&mut slot, &shared, &prep)
            .expect("activate");
        let p = slot.processor().expect("active");
        assert_eq!(p.loaded_context, Some(StateContext::Preset));
        assert_eq!(p.changed_context, Some(StateContext::Preset));
    }

    #[test]
    fn lifecycle_parameter_only_load_keeps_pending_custom_state() {
        let shared = DynamicParams::<0>::new();
        let mut slot =
            <Lifecycle<ContextPlugin> as PluginLogicCore>::init(&shared, &InitContext::default());
        <Lifecycle<ContextPlugin> as PluginLogicCore>::load_state_with_context(
            &mut slot,
            b"custom",
            StateContext::Project,
        )
        .unwrap();
        // A later parameter-only load carries no custom bytes.
        <Lifecycle<ContextPlugin> as PluginLogicCore>::load_state_with_context(
            &mut slot,
            &[],
            StateContext::Preset,
        )
        .unwrap();

        let prep = PreparationConfig::new(
            AudioConfig::new(48_000.0, 256),
            PreparationFrameBound::NegotiatedHost,
        );
        <Lifecycle<ContextPlugin> as PluginLogicCore>::prepare(&mut slot, &shared, &prep)
            .expect("activate");
        assert_eq!(
            slot.processor().expect("active").loaded_context,
            Some(StateContext::Project)
        );
    }

    #[test]
    fn lifecycle_aborts_activation_and_restores_pending_raw_on_deferred_load_error() {
        static DEACTIVATES: AtomicU32 = AtomicU32::new(0);
        static FAIL_LOAD: AtomicBool = AtomicBool::new(true);

        struct RejectRawPlugin;
        #[derive(Default)]
        struct RejectRawProc {
            loaded: Option<Vec<u8>>,
        }

        impl Plugin for RejectRawPlugin {
            type Shared = DynamicParams<0>;
            type MainThread = ();
            type Processor = RejectRawProc;
            fn main_thread(_shared: &Arc<Self::Shared>) -> Self::MainThread {}
            fn activate(
                _shared: &Self::Shared,
                _config: &AudioConfig,
                _cx: &InitContext,
            ) -> Result<Self::Processor, PreparationError> {
                Ok(RejectRawProc::default())
            }
            fn deactivate(_processor: Self::Processor) {
                DEACTIVATES.fetch_add(1, Ordering::AcqRel);
            }
            fn process(
                _processor: &mut Self::Processor,
                _shared: &Self::Shared,
                _buffer: &mut AudioBuffer<f32>,
                _events: &EventList,
                _context: &mut ProcessContext,
            ) -> ProcessStatus {
                ProcessStatus::Normal
            }
            fn load_state_with_context(
                processor: &mut Self::Processor,
                data: &[u8],
                _context: StateContext,
            ) -> Result<(), StateLoadError> {
                if FAIL_LOAD.load(Ordering::Acquire) {
                    return Err(StateLoadError::Malformed("deferred failure"));
                }
                processor.loaded = Some(data.to_vec());
                Ok(())
            }
        }

        let shared = DynamicParams::<0>::new();
        let mut slot =
            <Lifecycle<RejectRawPlugin> as PluginLogicCore>::init(&shared, &InitContext::default());
        <Lifecycle<RejectRawPlugin> as PluginLogicCore>::load_state_with_context(
            &mut slot,
            b"deferred-bytes",
            StateContext::Project,
        )
        .unwrap();

        let prep = PreparationConfig::new(
            AudioConfig::new(48_000.0, 256),
            PreparationFrameBound::NegotiatedHost,
        );
        let err =
            <Lifecycle<RejectRawPlugin> as PluginLogicCore>::prepare(&mut slot, &shared, &prep)
                .expect_err("failed deferred load must fail prepare");
        assert_eq!(err, PreparationError::NotReady);
        assert!(slot.processor().is_none());
        assert_eq!(DEACTIVATES.load(Ordering::Acquire), 1);

        // Retrying after clearing the failure replays the restored `pending_raw`.
        FAIL_LOAD.store(false, Ordering::Release);
        <Lifecycle<RejectRawPlugin> as PluginLogicCore>::prepare(&mut slot, &shared, &prep)
            .expect("retry succeeds");
        assert_eq!(
            slot.processor().expect("active").loaded.as_deref(),
            Some(&b"deferred-bytes"[..])
        );
    }
}
