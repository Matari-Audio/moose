use std::sync::Arc;

use crate::plugin::PluginRuntime;
use moose_params::{ParamInfo, Params};

/// A CLAP extension vtable whose storage lasts for the life of the process.
#[derive(Clone, Copy, Debug)]
pub struct CustomClapExtension(*const std::ffi::c_void);

impl CustomClapExtension {
    /// Wrap a static vtable.
    ///
    /// # Safety
    /// The table must implement the ABI associated with the advertised extension ID.
    pub unsafe fn from_static<T: Sync + 'static>(table: &'static T) -> Self {
        Self(std::ptr::from_ref(table).cast())
    }

    /// Return the address supplied to the CLAP host.
    #[must_use]
    pub fn as_ptr(self) -> *const std::ffi::c_void {
        self.0
    }
}

/// Unified export trait for all plugin formats.
///
/// Implement this once on your plugin type. All format wrappers
/// (CLAP, VST3, AU, standalone) use this to construct your plugin
/// and access its parameters.
///
/// ```ignore
/// impl PluginExport for MyPlugin {
///     type Params = MyParams;
///     fn create() -> Self { Self::new() }
///     fn params(&self) -> &MyParams { &self.params }
///     fn params_arc(&self) -> Arc<MyParams> { self.params.clone() }
/// }
/// ```
///
/// All parameter mutation goes through the atomic-backed accessors on
/// `&Params` - no `&mut Params` accessor is required, which keeps the
/// trait usable while the editor holds an `Arc<Params>` reader.
pub trait PluginExport: PluginRuntime + Sized {
    type Params: Params;

    /// Resolve a product CLAP extension. A remote-controls table replaces the
    /// generated grouping; other built-in extensions remain framework-owned.
    #[must_use]
    fn clap_extension(_id: &std::ffi::CStr) -> Option<CustomClapExtension> {
        None
    }

    /// Optional Steinberg VST3 3.7.5+ `IPluginCompatibility` JSON payload
    /// describing replaced legacy VST2/VST3 class IDs. When `Some` (and
    /// non-empty), `moose-vst3` registers a `"Plugin Compatibility Class"`
    /// alongside the audio component in `IPluginFactory`.
    #[must_use]
    fn vst3_compatibility_json() -> Option<&'static str> {
        None
    }

    /// The instance's main-thread part. Called once at instantiation; the
    /// wrapper keeps it in a `MainThreadCell` and calls it only from host
    /// main-thread callbacks.
    fn main_thread(&self) -> Box<dyn crate::main_thread::MainThreadHooks> {
        Box::new(())
    }

    /// Static MIDI 2.0 controller-to-parameter assignments advertised via
    /// VST3 SDK 3.8 `IMidiMapping2`. Combined with any dynamic assignments
    /// returned by [`MainThreadHooks::midi2_controller_assignments`](crate::main_thread::MainThreadHooks::midi2_controller_assignments).
    #[must_use]
    fn midi2_controller_assignments(
        _direction: crate::main_thread::BusDirection,
    ) -> Vec<crate::main_thread::Midi2ControllerAssignment> {
        Vec::new()
    }

    /// Visit the concrete DSP state when this export supports typed access.
    fn with_dsp_state(&mut self, _callback: &mut dyn FnMut(&mut dyn std::any::Any)) -> bool {
        false
    }

    /// Construct a new instance of the plugin.
    fn create() -> Self;

    /// Immutable access to the parameter struct.
    fn params(&self) -> &Self::Params;

    /// Get a shared `Arc` reference to the parameter struct.
    ///
    /// Used by format wrappers to pass params to GUI closures without
    /// raw pointers. The Arc is cloned (cheap ref-count bump), not the
    /// params themselves.
    fn params_arc(&self) -> Arc<Self::Params>;

    /// Shared meter storage handle, mirroring [`Self::params_arc`]:
    /// the audio thread publishes meter values into this store from
    /// inside `process()` (via the shells' meter callback), and GUI
    /// `get_meter` closures read it - never the plugin instance,
    /// whose `&mut` the audio thread holds for the whole block.
    ///
    /// The `moose::plugin!` shells own the store and return their
    /// handle here; a hand-written `PluginExport` impl keeps one
    /// alongside its params `Arc`.
    fn meter_store(&self) -> Arc<crate::meters::MeterStore>;

    /// Shared snapshot slot for lock-free state save. The shell (audio
    /// thread) publishes the plugin's custom state into it after each
    /// block when the plugin overrides `snapshot_into`; the wrapper's
    /// `save_state` reads it without taking the plugin lock. A plugin
    /// that doesn't opt in never publishes, so the slot stays empty and
    /// `save_state` falls back to the locked path.
    ///
    /// The `moose::plugin!` shells own the slot and return their handle
    /// here; a hand-written impl keeps one alongside its params `Arc`.
    fn snapshot_slot(&self) -> Arc<crate::snapshot::SnapshotSlot>;

    /// The plugin's background-task spawner, or `None` when the plugin
    /// wired no `tasks:` on `plugin!`. Format wrappers stamp it into the
    /// editor's `PluginContext` (via `PluginContext::with_tasks`) so the
    /// GUI can schedule work, matching what the shell does for `process`.
    /// The default suits plugins with no background tasks.
    fn task_spawner(&self) -> Option<crate::tasks::AnyTaskSpawner> {
        None
    }

    /// A lock-free editor builder: hand it the param store and it
    /// returns the editor (or `None` for a headless plugin).
    ///
    /// Called once at instance creation - format wrappers cache the
    /// returned closure *outside* the plugin lock (alongside
    /// `params_arc`), then invoke it when the host opens the GUI, so
    /// editor construction never takes the plugin lock and never waits
    /// on an in-flight audio block. The closure binds only the
    /// lock-free param store, so a `--shell` build's closure rebuilds
    /// from the *reloaded* dylib (GUI hot-reload survives, picked up on
    /// the next editor close+open). The `moose::plugin!` shells provide
    /// this; a hand-written impl returns a closure that builds its
    /// editor directly. Default: a closure that returns `None`.
    fn editor_builder(&self) -> crate::editor::EditorBuilder<Self::Params> {
        Box::new(|_params| None)
    }

    /// Static parameter metadata for registration-time access.
    ///
    /// Format wrappers' `register_*` paths (`moose-vst2`, `moose-vst3`,
    /// `moose-au`, `moose-aax`) call this instead of the historical
    /// `Self::create().params().param_infos()` walk, which constructed
    /// a full plugin instance - including any allocation the
    /// constructor did (DSP buffers, FFT plans, image atlases) - just
    /// to read static metadata. On platforms where registration runs
    /// from C++ static initializers (notably AAX `Describe`) those
    /// allocations sit in a fragile init-order regime; avoiding them
    /// closes a class of platform-dependent registration bugs.
    ///
    /// Default impl prefers
    /// [`Params::param_infos_static`]
    /// when it returns a non-empty vec (the `#[derive(Params)]` path
    /// emits an override built from compile-time metadata) and falls
    /// back to the runtime construction otherwise - so plugins with
    /// hand-written `Params` impls that don't override the static
    /// path keep working unchanged.
    #[must_use]
    fn param_infos_static() -> Vec<ParamInfo> {
        let from_params = <Self::Params as Params>::param_infos_static();
        if from_params.is_empty() {
            Self::create().params().param_infos()
        } else {
            from_params
        }
    }

    /// Static "does this plugin have an editor" predicate. AAX's
    /// `Describe` path needs to know this at registration time
    /// (`has_editor` field on the static descriptor). Paired with
    /// [`Self::param_infos_static`], this is the second reason every
    /// format's registration walk constructed a plugin.
    ///
    /// Default impl falls back to that runtime path so unannotated
    /// plugins keep working. Plugins that want to avoid the
    /// static-init plugin construction (notably for AAX hosts that
    /// run `Describe` very early) override with a `const`-style
    /// answer:
    ///
    /// ```ignore
    /// impl PluginExport for MyPlugin {
    ///     // ...
    ///     fn has_editor_static() -> bool { true }
    /// }
    /// ```
    ///
    /// VST2 / VST3 / AU never call this - they don't need the answer
    /// at registration time.
    #[must_use]
    fn has_editor_static() -> bool {
        let plugin = Self::create();
        plugin.editor_builder()(plugin.params_arc()).is_some()
    }
}

/// Serialize a plugin's custom state live, on a thread that owns the
/// plugin (never the audio thread). Prefers the off-thread publisher lane,
/// then `snapshot_into`, then the legacy `save_state`. CLAP / VST3 / AU
/// reach this through `wrapper::custom_state_for_save` when they can own
/// the plugin.
#[must_use]
pub fn read_custom_state_offthread<P: PluginExport>(plugin: &P) -> Vec<u8> {
    read_custom_state_offthread_with_context(plugin, crate::state::StateContext::Unknown)
}

/// Context-aware variant of [`read_custom_state_offthread`].
///
/// Invokes [`PluginRuntime::save_state_with_context`](crate::plugin::PluginRuntime::save_state_with_context)
/// first; if it returns `Some(bytes)` (including `Some(Vec::new())`), that
/// context-specific payload is used directly. When it returns `None`, falls
/// back to the off-thread snapshot lane, then `snapshot_into`, then
/// `save_state`.
#[must_use]
pub fn read_custom_state_offthread_with_context<P: PluginExport>(
    plugin: &P,
    context: crate::state::StateContext,
) -> Vec<u8> {
    current_custom_state_with_context(plugin, context).unwrap_or_default()
}

pub(crate) fn current_custom_state_with_context<P: PluginExport>(
    plugin: &P,
    context: crate::state::StateContext,
) -> Option<Vec<u8>> {
    if let Some(contextual) = plugin.save_state_with_context(context) {
        return Some(contextual);
    }
    let slot = plugin.snapshot_slot();
    if let Some(contextual) = slot.read_context_offthread(context) {
        return Some(contextual);
    }
    slot.read_offthread().or_else(|| {
        let mut buf = Vec::new();
        if plugin.snapshot_into(&mut buf) {
            Some(buf)
        } else {
            let legacy = plugin.save_state();
            (!legacy.is_empty()).then_some(legacy)
        }
    })
}

#[cfg(test)]
mod context_state_tests {
    use super::*;
    use crate::buffer::AudioBuffer;
    use crate::config::AudioConfig;
    use crate::events::EventList;
    use crate::info::{MidiDialect, PluginCategory, PluginInfo};
    use crate::meters::MeterStore;
    use crate::plugin::PluginRuntime;
    use crate::process::{ProcessContext, ProcessStatus};
    use crate::snapshot::SnapshotSlot;
    use crate::state::StateContext;
    use moose_params::DynamicParams;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_info(id: &'static str) -> PluginInfo {
        PluginInfo {
            name: id,
            vendor: "Test",
            url: "",
            version: "0.0.1",
            category: PluginCategory::Effect,
            description: None,
            clap_manual_url: None,
            clap_support_url: None,
            clap_features: &[],
            accepts_midi_in: false,
            emits_midi: false,
            midi_input_dialect: MidiDialect::Midi1,
            midi_output_dialect: MidiDialect::Midi1,
            midi_input_ports: 0,
            midi_output_ports: 0,
            bundle_id: id,
            vst3_id: id,
            clap_id: id,
            fourcc: *b"Test",
            au_type: *b"aufx",
            au_manufacturer: *b"Test",
            vst3_subcategory: None,
            preset_user_dir: None,
            preset_extension: "preset",
            vst3_name: None,
            clap_name: None,
            au_name: None,
            au3_name: None,
            mute_preview_output: false,
            automation: crate::Automation::DEFAULT,
            legacy_au_keys: &[],
            legacy_aax_chunks: &[],
        }
    }

    struct SnapshotOnlyPlugin {
        params: Arc<DynamicParams<0>>,
        meters: Arc<MeterStore>,
        snapshot: Arc<SnapshotSlot>,
    }

    impl PluginRuntime for SnapshotOnlyPlugin {
        type Sample = f32;
        fn info() -> PluginInfo {
            test_info("com.test.snap")
        }
        fn reset(&mut self, _: &AudioConfig) {}
        fn process(
            &mut self,
            _: &mut AudioBuffer<f32>,
            _: &EventList,
            _: &mut ProcessContext,
        ) -> ProcessStatus {
            ProcessStatus::Normal
        }
        fn save_state(&self) -> Vec<u8> {
            b"stale-legacy".to_vec()
        }
    }

    impl PluginExport for SnapshotOnlyPlugin {
        type Params = DynamicParams<0>;
        fn create() -> Self {
            let snapshot = SnapshotSlot::new();
            snapshot.store_arc(b"live-snapshot".to_vec());
            Self {
                params: Arc::new(DynamicParams::new()),
                meters: MeterStore::new(),
                snapshot,
            }
        }
        fn params(&self) -> &Self::Params {
            &self.params
        }
        fn params_arc(&self) -> Arc<Self::Params> {
            Arc::clone(&self.params)
        }
        fn meter_store(&self) -> Arc<MeterStore> {
            Arc::clone(&self.meters)
        }
        fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
            Arc::clone(&self.snapshot)
        }
    }

    struct ExplicitContextPlugin {
        params: Arc<DynamicParams<0>>,
        meters: Arc<MeterStore>,
        snapshot: Arc<SnapshotSlot>,
        calls: AtomicUsize,
    }

    impl PluginRuntime for ExplicitContextPlugin {
        type Sample = f32;
        fn info() -> PluginInfo {
            test_info("com.test.ctx")
        }
        fn reset(&mut self, _: &AudioConfig) {}
        fn process(
            &mut self,
            _: &mut AudioBuffer<f32>,
            _: &EventList,
            _: &mut ProcessContext,
        ) -> ProcessStatus {
            ProcessStatus::Normal
        }
        fn save_state_with_context(&self, context: StateContext) -> Option<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match context {
                StateContext::Preset => Some(Vec::new()),
                StateContext::Project => Some(b"project-blob".to_vec()),
                _ => None,
            }
        }
    }

    impl PluginExport for ExplicitContextPlugin {
        type Params = DynamicParams<0>;
        fn create() -> Self {
            let snapshot = SnapshotSlot::new();
            snapshot.store_arc(b"live-snapshot".to_vec());
            Self {
                params: Arc::new(DynamicParams::new()),
                meters: MeterStore::new(),
                snapshot,
                calls: AtomicUsize::new(0),
            }
        }
        fn params(&self) -> &Self::Params {
            &self.params
        }
        fn params_arc(&self) -> Arc<Self::Params> {
            Arc::clone(&self.params)
        }
        fn meter_store(&self) -> Arc<MeterStore> {
            Arc::clone(&self.meters)
        }
        fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
            Arc::clone(&self.snapshot)
        }
    }

    #[test]
    fn snapshot_only_plugin_keeps_live_snapshot_under_non_unknown_context() {
        let plugin = SnapshotOnlyPlugin::create();
        assert_eq!(
            current_custom_state_with_context(&plugin, StateContext::Preset),
            Some(b"live-snapshot".to_vec())
        );
    }

    #[test]
    fn explicit_empty_context_override_suppresses_snapshot_and_runs_once() {
        let plugin = ExplicitContextPlugin::create();
        assert_eq!(
            current_custom_state_with_context(&plugin, StateContext::Preset),
            Some(Vec::new())
        );
        assert_eq!(plugin.calls.load(Ordering::Relaxed), 1);

        assert_eq!(
            current_custom_state_with_context(&plugin, StateContext::Project),
            Some(b"project-blob".to_vec())
        );
        assert_eq!(plugin.calls.load(Ordering::Relaxed), 2);

        // Returning None falls through to the live snapshot cleanly.
        assert_eq!(
            current_custom_state_with_context(&plugin, StateContext::Duplicate),
            Some(b"live-snapshot".to_vec())
        );
        assert_eq!(plugin.calls.load(Ordering::Relaxed), 3);
    }
}
