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
//! [`Lifecycle`] adapts a [`Plugin`] to the existing leaf traits, so every
//! wrapper runs it unchanged:
//!
//! ```ignore
//! moose::plugin! { logic: moose::Lifecycle<MySynth>, params: MySynthParams }
//! ```

use std::marker::PhantomData;
use std::sync::Arc;

use moose_core::buffer::AudioBuffer;
use moose_core::bus::BusLayout;
use moose_core::config::AudioConfig;
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

use crate::{PluginLogic, PluginLogic64};

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

    /// Whether this plugin publishes a state snapshot after changed blocks,
    /// through [`Self::snapshot_into`]. A static capability: the shell
    /// probes it once, before any processor exists. Default `false`: a host
    /// save asks [`Self::save_state`] on demand.
    const SNAPSHOTS: bool = false;

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
    /// previous processor is deactivated and its custom state carried into
    /// the new one.
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

    /// Serialize custom (non-param) state. Same contract as
    /// [`PluginLogic::save_state`]. Default: none.
    fn save_state(processor: &Self::Processor) -> Vec<u8> {
        let _ = processor;
        Vec::new()
    }

    /// Fill `buf` (arriving cleared, capacity kept) with the bytes
    /// [`Self::save_state`] would produce. Runs on the audio thread after
    /// each block whose [`Self::snapshot_version`] changed, when
    /// [`Self::SNAPSHOTS`] is `true`, under the same real-time rules as
    /// `process`. The default copies [`Self::save_state`], which allocates:
    /// override it for real-time-safe serialization.
    fn snapshot_into(processor: &Self::Processor, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&Self::save_state(processor));
    }

    /// Generation token for [`Self::snapshot_into`]: bump it when custom
    /// state changes so unchanged blocks skip serialization. Read on the
    /// audio thread every block; keep it a field read. `None` (default)
    /// re-serializes every block.
    fn snapshot_version(processor: &Self::Processor) -> Option<u64> {
        let _ = processor;
        None
    }

    /// Bytes to pre-warm the inline snapshot buffer to, off the audio
    /// thread. Default 256.
    #[must_use]
    fn snapshot_prealloc_hint() -> usize {
        moose_core::snapshot::SNAPSHOT_PREALLOC
    }

    /// Restore custom state. Same contract as [`PluginLogic::load_state`].
    /// A load before activation is kept and applied to the processor
    /// [`Self::activate`] returns.
    ///
    /// # Errors
    ///
    /// The bytes are not a state this plugin can load.
    fn load_state(processor: &mut Self::Processor, data: &[u8]) -> Result<(), StateLoadError> {
        let _ = (processor, data);
        Ok(())
    }

    /// Called right after a successful [`Self::load_state`], on the same
    /// thread (also when a load held from before activation is applied).
    /// Recompute caches the next `process` reads. Default: no-op.
    fn state_changed(processor: &mut Self::Processor, shared: &Self::Shared) {
        let _ = (processor, shared);
    }

    /// Check custom state on the host thread. Same contract as
    /// [`PluginLogic::validate_state`].
    ///
    /// # Errors
    ///
    /// The bytes are not a state this plugin can load.
    fn validate_state(data: &[u8]) -> Result<(), StateLoadError> {
        let _ = data;
        Ok(())
    }

    /// Translate foreign state. Same contract as
    /// [`PluginLogic::migrate_state`].
    #[must_use]
    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState> {
        let _ = foreign;
        None
    }

    /// Build the editor from the shared part.
    fn editor(shared: Arc<Self::Shared>) -> Box<dyn Editor>;
}

/// Runs a [`Plugin`] through the existing wrappers (`moose::plugin!`).
///
/// A type-level marker, never constructed: it implements [`PluginLogic`] for
/// `P: Plugin<f32>` and [`PluginLogic64`] for `P: Plugin<f64>`, with
/// [`ProcessorSlot`] as the DSP state.
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

#[derive(Debug)]
enum Slot<T> {
    /// Not activated. Holds a state load that arrived before activation.
    Inactive { pending_state: Option<Vec<u8>> },
    /// Built by `activate` for `config`.
    Active { processor: T, config: AudioConfig },
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self::Inactive {
            pending_state: None,
        }
    }
}

impl<T> ProcessorSlot<T> {
    /// The processor, once [`Plugin::activate`] has succeeded.
    #[must_use]
    pub fn processor(&self) -> Option<&T> {
        match &self.slot {
            Slot::Active { processor, .. } => Some(processor),
            Slot::Inactive { .. } => None,
        }
    }

    /// Mutable access to the processor, once activated.
    pub fn processor_mut(&mut self) -> Option<&mut T> {
        match &mut self.slot {
            Slot::Active { processor, .. } => Some(processor),
            Slot::Inactive { .. } => None,
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
    let carried = match std::mem::take(&mut slot.slot) {
        Slot::Inactive { pending_state } => pending_state,
        Slot::Active { processor: old, .. } => {
            let bytes = P::save_state(&old);
            P::deactivate(old);
            (!bytes.is_empty()).then_some(bytes)
        }
    };
    match P::activate(shared, &config.audio, &slot.cx) {
        Ok(mut processor) => {
            // The bytes passed `validate_state` on load, or came from this
            // plugin's own `save_state`; a failure here has no host to
            // report to, and the fresh processor is still usable.
            if let Some(bytes) = &carried
                && P::load_state(&mut processor, bytes).is_ok()
            {
                P::state_changed(&mut processor, shared);
            }
            slot.slot = Slot::Active {
                processor,
                config: config.audio,
            };
            Ok(())
        }
        Err(error) => {
            slot.slot = Slot::Inactive {
                pending_state: carried,
            };
            Err(error)
        }
    }
}

/// A processor built for `built` can serve `wanted` unchanged.
fn fits(built: &AudioConfig, wanted: &AudioConfig) -> bool {
    built.sample_rate.to_bits() == wanted.sample_rate.to_bits()
        && built.process_mode == wanted.process_mode
        && wanted.max_block_size <= built.max_block_size
}

fn load_state<S: Sample, P: Plugin<S>>(
    slot: &mut ProcessorSlot<P::Processor>,
    data: &[u8],
) -> Result<(), StateLoadError> {
    match &mut slot.slot {
        Slot::Active { processor, .. } => P::load_state(processor, data),
        Slot::Inactive { pending_state } => {
            P::validate_state(data)?;
            *pending_state = Some(data.to_vec());
            Ok(())
        }
    }
}

fn snapshot_into<S: Sample, P: Plugin<S>>(
    slot: &ProcessorSlot<P::Processor>,
    buf: &mut Vec<u8>,
) -> bool {
    if !P::SNAPSHOTS {
        return false;
    }
    match &slot.slot {
        Slot::Active { processor, .. } => P::snapshot_into(processor, buf),
        Slot::Inactive {
            pending_state: Some(bytes),
        } => buf.extend_from_slice(bytes),
        Slot::Inactive {
            pending_state: None,
        } => {}
    }
    true
}

fn save_state<S: Sample, P: Plugin<S>>(slot: &ProcessorSlot<P::Processor>) -> Vec<u8> {
    match &slot.slot {
        Slot::Active { processor, .. } => P::save_state(processor),
        Slot::Inactive { pending_state } => pending_state.clone().unwrap_or_default(),
    }
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

/// `impl $leaf for Lifecycle<P>` where `P: Plugin<$sample>`.
macro_rules! lifecycle_leaf {
    ($leaf:ident, $sample:ty) => {
        impl<P: Plugin<$sample>> $leaf for Lifecycle<P> {
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
                activate::<$sample, P>(state, params, config)
            }

            fn process(
                state: &mut Self::DspState,
                params: &P::Shared,
                buffer: &mut AudioBuffer<$sample>,
                events: &EventList,
                context: &mut ProcessContext,
            ) -> ProcessStatus {
                process::<$sample, P>(state, params, buffer, events, context)
            }

            fn save_state(state: &Self::DspState) -> Vec<u8> {
                save_state::<$sample, P>(state)
            }

            fn snapshot_into(state: &Self::DspState, buf: &mut Vec<u8>) -> bool {
                snapshot_into::<$sample, P>(state, buf)
            }

            fn snapshot_version(state: &Self::DspState) -> Option<u64> {
                state.processor().and_then(P::snapshot_version)
            }

            fn snapshot_prealloc_hint() -> usize {
                P::snapshot_prealloc_hint()
            }

            fn load_state(state: &mut Self::DspState, data: &[u8]) -> Result<(), StateLoadError> {
                load_state::<$sample, P>(state, data)
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

            fn latency(state: &Self::DspState) -> u32 {
                state.processor().map_or(0, P::latency)
            }

            fn tail(state: &Self::DspState) -> u32 {
                state.processor().map_or(0, P::tail)
            }

            fn editor(params: Arc<P::Shared>) -> Box<dyn Editor> {
                P::editor(params)
            }
        }
    };
}

lifecycle_leaf!(PluginLogic, f32);
lifecycle_leaf!(PluginLogic64, f64);
