//! User-facing plugin trait and wrapper bridge.
//!
//! Plugin authors implement [`Plugin`] (shared, main-thread, and audio
//! processor parts as three types) and export it via `moose::plugin!`.

use moose_core::buffer::AudioBuffer;
use moose_core::bus::BusLayout;
use moose_core::config::AudioConfig;
use moose_core::editor::Editor;
use moose_core::events::EventList;
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::state::{DecodedState, ForeignState, MigratedState, StateContext, StateLoadError};
use moose_params::sample::Sample;

mod lifecycle;
pub use lifecycle::{Lifecycle, Plugin, ProcessorSlot};
pub use moose_core::tasks::{InitContext, TaskSpawner};

// ---------------------------------------------------------------------------
// PluginLogicCore - generic internal trait consumed by shells and wrappers
// ---------------------------------------------------------------------------

/// Wrapper-facing plugin trait, generic over the audio sample type.
///
/// Format wrappers (`StaticShell`, `HotShell`, CLAP / VST3 / AU) bind on
/// `PluginLogicCore<S>`. Plugin authors implement [`Plugin`]; [`Lifecycle`]
/// adapts any `Plugin<S>` to `PluginLogicCore<S>` and [`PluginEditor<S>`].
///
/// Hidden from docs: public only so the `moose::plugin!` expansion,
/// `moose-loader` and tests can name it.
#[doc(hidden)]
pub trait PluginLogicCore<S: Sample = f32>: 'static {
    /// The plugin's parameter struct.
    type Params: moose_params::Params;
    /// Mutable per-block audio state owned by the shell.
    type DspState: Send + 'static;

    /// Resolve a custom static CLAP extension table.
    #[must_use]
    fn clap_extension(_id: &std::ffi::CStr) -> Option<moose_core::export::CustomClapExtension> {
        None
    }

    /// Whether the hot-reload shell carries live DSP state across a reload
    /// via a `save_state` / `load_state` round-trip. Default `true`.
    const PRESERVE_DSP_STATE: bool = true;

    #[must_use]
    fn supports_in_place() -> bool {
        false
    }

    /// Maximum serialized host state; size document admission to this bound.
    #[must_use]
    fn max_host_state_bytes() -> usize {
        32 * 1024 * 1024
    }

    /// Supported audio bus configurations. Default: stereo and mono effect.
    #[must_use]
    fn bus_layouts() -> Vec<BusLayout> {
        BusLayout::stereo_and_mono()
    }

    /// The main-thread hooks built once per instance.
    fn main_thread(
        params: &std::sync::Arc<Self::Params>,
    ) -> Box<dyn moose_core::main_thread::MainThreadHooks> {
        let _ = params;
        Box::new(())
    }

    /// Static MIDI 2.0 controller-to-parameter assignments (VST3 SDK 3.8
    /// `IMidiMapping2`). Default: empty.
    #[must_use]
    fn midi2_controller_assignments(
        _direction: moose_core::main_thread::BusDirection,
    ) -> Vec<moose_core::main_thread::Midi2ControllerAssignment> {
        Vec::new()
    }

    /// Build initial state from params.
    fn init(params: &Self::Params, cx: &InitContext) -> Self::DspState;

    fn reset(state: &mut Self::DspState, params: &Self::Params, config: &AudioConfig);

    /// Worker/fallible-preparation logic must declare this requirement.
    #[must_use]
    fn requires_fallible_preparation() -> bool {
        false
    }

    /// Prepare the runtime before activation.
    ///
    /// # Errors
    /// Returns an error when configuration or preparation fails.
    fn prepare(
        state: &mut Self::DspState,
        params: &Self::Params,
        config: &moose_core::plugin::PreparationConfig,
    ) -> Result<(), moose_core::plugin::PreparationError> {
        config.validate()?;
        Self::reset(state, params, &config.audio);
        Ok(())
    }

    fn process(
        state: &mut Self::DspState,
        params: &Self::Params,
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    fn save_state(state: &Self::DspState) -> Vec<u8> {
        let _ = state;
        Vec::new()
    }

    /// Context-aware variant of [`Self::save_state`], called strictly on the
    /// host/main thread (never on the audio thread). Return `Some(bytes)` to
    /// override the snapshot / `save_state` fallback for `context` (including
    /// `Some(Vec::new())` to intentionally omit extra state). For context-aware
    /// saves while audio is actively running, publish off-thread via
    /// `SnapshotPublisher::publish_for_context`. Default returns `None`.
    fn save_state_with_context(state: &Self::DspState, _context: StateContext) -> Option<Vec<u8>> {
        let _ = state;
        None
    }

    /// Restore plugin-specific state.
    ///
    /// # Errors
    /// Returns `Err(StateLoadError)` when the blob cannot be decoded.
    fn load_state(state: &mut Self::DspState, data: &[u8]) -> Result<(), StateLoadError> {
        let _ = (state, data);
        Ok(())
    }

    /// Context-aware variant of [`Self::load_state`]. Default delegates to
    /// [`Self::load_state`] when `data` is non-empty.
    ///
    /// # Errors
    /// Returns `Err(StateLoadError)` when the blob cannot be decoded.
    fn load_state_with_context(
        state: &mut Self::DspState,
        data: &[u8],
        _context: StateContext,
    ) -> Result<(), StateLoadError> {
        if data.is_empty() {
            Ok(())
        } else {
            Self::load_state(state, data)
        }
    }

    /// Translate foreign state into moose shape.
    #[must_use]
    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState> {
        let _ = foreign;
        None
    }

    #[must_use]
    fn state_schema_version() -> u32 {
        0
    }

    #[must_use]
    fn state_migrations() -> moose_core::state::StateMigrationChain {
        moose_core::state::StateMigrationChain::new(Self::state_schema_version())
    }

    /// Check custom state on the host thread before it is applied.
    ///
    /// # Errors
    /// The bytes are not a state this plugin can load.
    fn validate_state(data: &[u8]) -> Result<(), StateLoadError> {
        let _ = data;
        Ok(())
    }

    /// Context-aware variant of [`Self::validate_state`].
    ///
    /// # Errors
    /// The bytes are not a state this plugin can load in `context`.
    fn validate_state_with_context(
        data: &[u8],
        _context: StateContext,
    ) -> Result<(), StateLoadError> {
        Self::validate_state(data)
    }

    /// Decode custom state off the audio thread into a ready-to-swap
    /// [`DecodedState`]. Default returns `Ok(None)` (legacy `load_state`).
    ///
    /// # Errors
    /// Returns `Err(StateLoadError)` when the bytes cannot be decoded.
    fn decode_state(data: &[u8]) -> Result<Option<DecodedState>, StateLoadError> {
        let _ = data;
        Ok(None)
    }

    /// Context-aware variant of [`Self::decode_state`].
    ///
    /// # Errors
    /// Returns `Err(StateLoadError)` when the bytes cannot be decoded in `context`.
    fn decode_state_with_context(
        data: &[u8],
        _context: StateContext,
    ) -> Result<Option<DecodedState>, StateLoadError> {
        Self::decode_state(data)
    }

    /// Swap a decoded state into `state` on the audio thread (zero-alloc,
    /// RT-safe). Displaced state stays in `decoded` and is dropped off-thread.
    fn apply_decoded_state(
        state: &mut Self::DspState,
        params: &Self::Params,
        decoded: &mut DecodedState,
    ) {
        let _ = (state, params, decoded);
    }

    /// Context-aware variant of [`Self::apply_decoded_state`].
    fn apply_decoded_state_with_context(
        state: &mut Self::DspState,
        params: &Self::Params,
        decoded: &mut DecodedState,
        _context: StateContext,
    ) {
        Self::apply_decoded_state(state, params, decoded);
    }

    fn state_changed(state: &mut Self::DspState, params: &Self::Params) {
        let _ = (state, params);
    }

    /// Context-aware variant of [`Self::state_changed`].
    fn state_changed_with_context(
        state: &mut Self::DspState,
        params: &Self::Params,
        _context: StateContext,
    ) {
        Self::state_changed(state, params);
    }

    fn latency(state: &Self::DspState) -> u32 {
        let _ = state;
        0
    }

    fn tail(state: &Self::DspState) -> u32 {
        let _ = state;
        0
    }
}

/// Precision-keyed editor factory bridged from [`Plugin`] through [`Lifecycle`].
pub trait PluginEditor<S: Sample> {
    /// The plugin's parameter struct.
    type Params: moose_params::Params;

    /// Build the editor from the lock-free param store.
    fn editor(params: std::sync::Arc<Self::Params>) -> Box<dyn Editor>;
}

// ---------------------------------------------------------------------------
// Background tasks - opt-in managed off-thread work
// ---------------------------------------------------------------------------

/// Opt-in managed background work. Each **task type** implements this to
/// declare its params, its concurrency mode, and the handler the framework
/// runs on a shared background-thread pool; a plugin lists one or more task
/// types with the `tasks:` key on `moose::plugin!` (`tasks: [Rebuild,
/// Analyze]`).
pub trait BackgroundTask: Send + 'static {
    /// The plugin's parameter struct; must match `Plugin::Shared`.
    type Params: moose_params::Params;
    /// Run this lane's handler one at a time for a given instance.
    const SERIALIZED: bool = false;
    /// Allow a nonblocking OS wake when a task is first queued.
    const WAKE_ON_SCHEDULE: bool = false;
    /// Run one task on the pool.
    fn run(self, params: &Self::Params);

    /// Run one bounded pass and optionally return the task for another fair
    /// pool turn.
    fn run_once(self, params: &Self::Params) -> Option<Self>
    where
        Self: Sized,
    {
        self.run(params);
        None
    }
}
