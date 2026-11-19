//! The [`Plugin`] trait: shared, main-thread and audio parts as three types.
//!
//! A plugin is three values with three owners:
//!
//! - [`Plugin::Shared`] - params and atomics, read from any thread.
//! - [`Plugin::MainThread`] - host-thread hooks, built once per instance.
//! - [`Plugin::Processor`] - the DSP state. It exists only after a successful
//!   [`Plugin::activate`] and only the audio side holds it.
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
use moose_core::main_thread::MainThreadHooks;
use moose_core::plugin::{PreparationConfig, PreparationError};
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::state::{ForeignState, MigratedState, StateLoadError};
use moose_params::Params;
use moose_params::sample::Sample;

use crate::{PluginLogic, PluginLogic64};

/// A plugin split by thread: `Shared` params, a `MainThread` part and a `Processor`.
///
/// `S` is the audio precision of the processor: `impl Plugin for X` is `f32`,
/// `impl Plugin<f64> for X` runs `f64` buffers (or `impl Plugin<Sample>` to
/// follow the prelude). Export it with `moose::plugin!` through [`Lifecycle`].
///
/// Note that `moose::plugin!` declares a `Plugin` type alias in the module it
/// is invoked in, which shadows this trait from the prelude there; write
/// `impl moose::Plugin for X` in that module.
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

    /// Supported audio bus configurations. Default: stereo and mono effect.
    #[must_use]
    fn bus_layouts() -> Vec<BusLayout> {
        BusLayout::stereo_and_mono()
    }

    /// Build the main-thread part. Called once per instance, on the host
    /// main thread.
    fn main_thread(shared: &Arc<Self::Shared>) -> Self::MainThread;

    /// Build a processor for `config`, on the host main thread. May allocate
    /// and block. This is the only way a processor comes into being; the
    /// previous one (if any) is deactivated first and its custom state
    /// carried into the new one.
    ///
    /// # Errors
    ///
    /// Return a [`PreparationError`] when the configuration can't be served;
    /// the wrapper then fails the host's activation and the plugin keeps
    /// producing silence.
    fn activate(
        shared: &Self::Shared,
        config: &AudioConfig,
    ) -> Result<Self::Processor, PreparationError>;

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

/// The DSP state [`Lifecycle`] hands the shell: a processor once activated.
#[derive(Debug)]
pub struct ProcessorSlot<T>(Slot<T>);

impl<T> Default for ProcessorSlot<T> {
    fn default() -> Self {
        Self(Slot::default())
    }
}

#[derive(Debug)]
enum Slot<T> {
    /// Not activated. Holds a state load that arrived before activation.
    Inactive {
        pending_state: Option<Vec<u8>>,
    },
    Active(T),
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
        match &self.0 {
            Slot::Active(processor) => Some(processor),
            Slot::Inactive { .. } => None,
        }
    }

    /// Mutable access to the processor, once activated.
    pub fn processor_mut(&mut self) -> Option<&mut T> {
        match &mut self.0 {
            Slot::Active(processor) => Some(processor),
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
    let carried = match std::mem::take(&mut slot.0) {
        Slot::Inactive { pending_state } => pending_state,
        Slot::Active(old) => {
            let bytes = P::save_state(&old);
            P::deactivate(old);
            (!bytes.is_empty()).then_some(bytes)
        }
    };
    match P::activate(shared, &config.audio) {
        Ok(mut processor) => {
            if let Some(bytes) = &carried {
                // The bytes passed `validate_state` on load, or came from this
                // plugin's own `save_state`; a failure here has no host to
                // report to, and the fresh processor is still usable.
                let _ = P::load_state(&mut processor, bytes);
            }
            slot.0 = Slot::Active(processor);
            Ok(())
        }
        Err(error) => {
            slot.0 = Slot::Inactive {
                pending_state: carried,
            };
            Err(error)
        }
    }
}

fn load_state<S: Sample, P: Plugin<S>>(
    slot: &mut ProcessorSlot<P::Processor>,
    data: &[u8],
) -> Result<(), StateLoadError> {
    match &mut slot.0 {
        Slot::Active(processor) => P::load_state(processor, data),
        Slot::Inactive { pending_state } => {
            P::validate_state(data)?;
            *pending_state = Some(data.to_vec());
            Ok(())
        }
    }
}

fn save_state<S: Sample, P: Plugin<S>>(slot: &ProcessorSlot<P::Processor>) -> Vec<u8> {
    match &slot.0 {
        Slot::Active(processor) => P::save_state(processor),
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
    if let Slot::Active(processor) = &mut slot.0 {
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

            fn bus_layouts() -> Vec<BusLayout> {
                P::bus_layouts()
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

            fn load_state(state: &mut Self::DspState, data: &[u8]) -> Result<(), StateLoadError> {
                load_state::<$sample, P>(state, data)
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
