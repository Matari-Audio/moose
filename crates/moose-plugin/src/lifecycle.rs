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
use moose_core::state::{ForeignState, MigratedState, StateLoadError};
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
    /// previous processor is deactivated first; the new one reads everything
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
    /// Custom state lives in `#[persist]` fields on [`Self::Shared`].
    fn state_changed(processor: &mut Self::Processor, shared: &Self::Shared) {
        let _ = (processor, shared);
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
/// activated, and the instance's [`InitContext`] for each activation.
pub struct ProcessorSlot<T> {
    slot: Slot<T>,
    cx: InitContext,
}

impl<T> Default for ProcessorSlot<T> {
    fn default() -> Self {
        Self {
            slot: Slot::default(),
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
    /// Built by `activate` for `config`.
    Active { processor: T, config: AudioConfig },
}

impl<T> ProcessorSlot<T> {
    /// The processor, once [`Plugin::activate`] has succeeded.
    #[must_use]
    pub fn processor(&self) -> Option<&T> {
        match &self.slot {
            Slot::Active { processor, .. } => Some(processor),
            Slot::Inactive => None,
        }
    }

    /// Mutable access to the processor, once activated.
    pub fn processor_mut(&mut self) -> Option<&mut T> {
        match &mut self.slot {
            Slot::Active { processor, .. } => Some(processor),
            Slot::Inactive => None,
        }
    }
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
    // Custom state lives in `#[persist]` on `Shared`, so a rebuilt
    // processor carries nothing over: it reads everything from `shared`.
    if let Slot::Active { processor: old, .. } = std::mem::take(&mut slot.slot) {
        P::deactivate(old);
    }
    let processor = P::activate(shared, &config.audio, &slot.cx)?;
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
