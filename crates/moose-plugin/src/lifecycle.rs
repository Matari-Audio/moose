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
use moose_core::main_thread::MainThreadHooks;
use moose_core::plugin::{PreparationConfig, PreparationError};
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::state::{DecodedState, ForeignState, MigratedState, StateLoadError};
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

    /// Build a processor for `config`, on the host main thread. May allocate
    /// and block. This is the only way a processor comes into being.
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
        shared: &Self::Shared,
        config: &AudioConfig,
        cx: &InitContext,
    ) -> Result<Self::Processor, PreparationError>;

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
    fn deactivate(processor: Self::Processor) {
        drop(processor);
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
/// activated, any pre-activation decoded state waiting for the first
/// activation, and the instance's [`InitContext`] for each activation.
pub struct ProcessorSlot<T> {
    slot: Slot<T>,
    pending_decoded: Option<DecodedState>,
    cx: InitContext,
}

impl<T> Default for ProcessorSlot<T> {
    fn default() -> Self {
        Self {
            slot: Slot::default(),
            pending_decoded: None,
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
    P::activate(shared, audio, cx).map(Box::new)
}

#[inline(never)]
#[allow(clippy::boxed_local)] // Intentionally unboxes inside a separate frame so `activate()` stays tiny.
fn deactivate_boxed<S: Sample, P: Plugin<S>>(processor: Box<P::Processor>) {
    P::deactivate(*processor);
}

fn activate<S: Sample, P: Plugin<S>>(
    slot: &mut ProcessorSlot<P::Processor>,
    shared: &P::Shared,
    config: &PreparationConfig,
) -> Result<(), PreparationError> {
    config.validate()?;
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
    if let Some(mut pending) = slot.pending_decoded.take() {
        P::apply_decoded_state(&mut processor, shared, &mut pending);
        P::state_changed(&mut processor, shared);
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
            cx: cx.clone(),
        }
    }

    fn main_thread(params: &Arc<P::Shared>) -> Box<dyn MainThreadHooks> {
        Box::new(P::main_thread(params))
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

    fn state_changed(state: &mut Self::DspState, params: &P::Shared) {
        if let Some(processor) = state.processor_mut() {
            P::state_changed(processor, params);
        }
    }

    fn validate_state(data: &[u8]) -> Result<(), StateLoadError> {
        P::validate_state(data)
    }

    fn decode_state(data: &[u8]) -> Result<Option<DecodedState>, StateLoadError> {
        P::decode_state(data)
    }

    fn apply_decoded_state(
        state: &mut Self::DspState,
        params: &P::Shared,
        decoded: &mut DecodedState,
    ) {
        if let Some(processor) = state.processor_mut() {
            P::apply_decoded_state(processor, params, decoded);
        } else if let Some(prev) = &mut state.pending_decoded {
            std::mem::swap(prev, decoded);
        } else {
            state.pending_decoded = Some(std::mem::replace(decoded, DecodedState::empty()));
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
}
