//! User-facing plugin trait and wrapper bridge.
//!
//! Plugin authors implement [`Plugin`] (shared, main-thread, and audio
//! processor parts as three types) and export it via `moose::plugin!`.
//!
//! `moose-plugin` depends on `moose-gui-types` (light: layout, render
//! trait, widget regions) without pulling in any windowing or renderer crate.

use moose_core::buffer::AudioBuffer;
use moose_core::bus::BusLayout;
use moose_core::config::AudioConfig;
use moose_core::editor::Editor;
use moose_core::events::EventList;
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::state::{ForeignState, MigratedState, StateLoadError};
use moose_gui_types::interaction::WidgetRegion;
use moose_gui_types::widgets::WidgetType;
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

    /// Restore plugin-specific state.
    ///
    /// # Errors
    /// Returns `Err(StateLoadError)` when the blob cannot be decoded.
    fn load_state(state: &mut Self::DspState, data: &[u8]) -> Result<(), StateLoadError> {
        let _ = (state, data);
        Ok(())
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

    fn state_changed(state: &mut Self::DspState, params: &Self::Params) {
        let _ = (state, params);
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

// ---------------------------------------------------------------------------
// Default hit test
// ---------------------------------------------------------------------------

/// Default hit test: circular for knobs, rectangular for everything
/// else, skip meters.
#[must_use]
pub fn default_hit_test(widgets: &[WidgetRegion], x: f32, y: f32) -> Option<usize> {
    for (i, w) in widgets.iter().enumerate() {
        if w.widget_type == WidgetType::Meter {
            continue;
        }
        if w.widget_type == WidgetType::Knob {
            let dx = x - w.cx;
            let dy = y - w.cy;
            if dx * dx + dy * dy <= w.radius * w.radius {
                return Some(i);
            }
        } else if x >= w.x && x <= w.x + w.w && y >= w.y && y <= w.y + w.h {
            return Some(i);
        }
    }
    None
}
