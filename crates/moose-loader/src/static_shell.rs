//! `StaticShell` - embeds the plugin directly into the binary.
//!
//! No dlopen, no file watcher, no Mutex. Same types as `HotShell`
//! but zero runtime overhead. Use via `export_static!`.

use std::sync::Arc;

use moose_core::buffer::AudioBuffer;
use moose_core::bus::BusLayout;
use moose_core::config::AudioConfig;
use moose_core::events::{EventBody, EventList};
use moose_core::info::PluginInfo;
use moose_core::meters::MeterStore;
use moose_core::plugin::PluginRuntime;
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::snapshot::{SnapshotPublisher, SnapshotSlot};
use moose_core::state::{DecodedState, ForeignState, MigratedState, StateContext, StateLoadError};
use moose_core::tasks::{AnyTaskSpawner, InitContext, InstanceContext, warm_pool};
use moose_params::Params;
use moose_params::sample::Sample;
use moose_plugin::PluginLogicCore;

// ---------------------------------------------------------------------------
// StaticShell
// ---------------------------------------------------------------------------

/// A static plugin shell that embeds the user's plugin logic
/// directly into the format-wrapper binary.
///
/// Same bridging as `HotShell` but without `NativeLoader`, `Mutex`,
/// file watching, or any dynamic loading overhead. Use via `export_static!`.
pub struct StaticShell<P: Params, L: PluginLogicCore<S, Params = P>, S: Sample = f32> {
    pub params: Arc<P>,
    /// The user's mutable DSP state, owned by the shell (not the
    /// descriptor `L`). Built once via `L::init(&params)`. Boxed so a
    /// large DSP state (delay lines, FFT tables) doesn't ride the stack
    /// every time the shell itself is moved into the wrapper.
    state: Box<L::DspState>,
    meters: Arc<MeterStore>,
    /// Lock-free publish slot for off-thread `SnapshotPublisher` and on-demand saves.
    snapshots: Arc<SnapshotSlot>,
    sample_rate: f64,
    /// Background-task spawner bundle (one lane per declared task type),
    /// when the plugin wired `tasks:` on `plugin!`. Type-erased; stamped
    /// into each block's `ProcessContext` so `ctx.tasks::<T>()` works.
    /// `None` for a plugin with no background tasks.
    tasks: Option<AnyTaskSpawner>,
    _sample: std::marker::PhantomData<fn() -> S>,
}

impl<P: Params, L: PluginLogicCore<S, Params = P>, S: Sample> Drop for StaticShell<P, L, S> {
    fn drop(&mut self) {
        if let Some(tasks) = &self.tasks {
            tasks.close();
        }
    }
}

impl<P: Params + Default + 'static, L: PluginLogicCore<S, Params = P> + 'static, S: Sample>
    StaticShell<P, L, S>
{
    /// Build the shell from shared params, constructing the initial DSP
    /// state via `L::init(&params, &cx)`. The descriptor `L` is a
    /// type-only marker; the shell owns the state it produces. `tasks` is
    /// the plugin's background-task spawner (`Some` only when the plugin
    /// wired `tasks:` on `plugin!`); it reaches `init` through the
    /// `InitContext` and each block through the `ProcessContext`.
    /// `instance` is the format wrapper's [`InstanceContext`] (`None`
    /// outside a wrapper); it reaches `init` through the `InitContext`.
    pub fn from_parts(
        params: Arc<P>,
        tasks: Option<AnyTaskSpawner>,
        instance: Option<InstanceContext>,
    ) -> Self {
        // A wired spawner means the plugin may schedule background work,
        // possibly first from `process()` (a filter that only rebuilds on a
        // knob move, with nothing in `init` to warm the pool). Start the
        // pool here, on the instantiation (main) thread, so the first
        // audio-thread schedule never cold-starts worker threads inside the
        // callback - keeping the spawner's "safe from the audio thread"
        // guarantee true regardless of where the plugin first schedules.
        if tasks.is_some() {
            warm_pool();
        }
        // Build the snapshot slot before `init` so the plugin can capture
        // a publisher for the off-thread (large-state) lane.
        let snapshots = SnapshotSlot::new();
        let init_ctx = InitContext::new(tasks.clone())
            .with_snapshot(SnapshotPublisher::new(&snapshots))
            .with_instance(instance);
        let state = L::init_boxed(&params, &init_ctx);
        let mut shell = Self {
            params,
            state,
            meters: MeterStore::new(),
            snapshots,
            sample_rate: 44100.0,
            tasks,
            _sample: std::marker::PhantomData,
        };
        // Baseline snapshot, so a save that falls back to the slot before
        // any block or request still has the initial custom state.
        shell.republish_snapshot();
        shell
    }

    /// Shared meter storage handle - the GUI-thread-safe channel
    /// for meter reads (see `PluginExport::meter_store`).
    #[must_use]
    pub fn meter_store(&self) -> Arc<MeterStore> {
        Arc::clone(&self.meters)
    }

    /// Shared snapshot slot for lock-free state save (see
    /// `PluginExport::snapshot_slot`).
    #[must_use]
    pub fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
        Arc::clone(&self.snapshots)
    }

    /// The plugin's background-task spawner (see
    /// `PluginExport::task_spawner`). `None` unless the plugin wired
    /// `tasks:` on `plugin!`.
    #[must_use]
    pub fn task_spawner(&self) -> Option<AnyTaskSpawner> {
        self.tasks.clone()
    }

    /// Access the plugin's DSP state (for testing).
    #[must_use]
    pub fn state_ref(&self) -> &L::DspState {
        &self.state
    }

    /// Mutable access to the plugin's DSP state (for testing).
    pub fn state_ref_mut(&mut self) -> &mut L::DspState {
        &mut self.state
    }
}

impl<P: Params + Default + 'static, L: PluginLogicCore<S, Params = P> + 'static, S: Sample>
    PluginRuntime for StaticShell<P, L, S>
{
    type Sample = S;

    fn info() -> PluginInfo
    where
        Self: Sized,
    {
        unreachable!("StaticShell::info() should not be called statically")
    }

    fn bus_layouts() -> Vec<BusLayout>
    where
        Self: Sized,
    {
        unreachable!("StaticShell::bus_layouts() should not be called statically")
    }

    fn init(&mut self) {}

    fn reset(&mut self, config: &AudioConfig) {
        self.sample_rate = config.sample_rate;
        // Params plumbing is the shell's job, not the plugin's: settle
        // smoother coefficients and state before the user's `reset` so
        // its body reads post-snap values.
        self.params.set_sample_rate(config.sample_rate);
        self.params.snap_smoothers();
        L::reset(&mut self.state, &self.params, config);
    }

    fn requires_fallible_preparation(&self) -> bool {
        L::requires_fallible_preparation()
    }

    fn prepare(
        &mut self,
        config: &moose_core::plugin::PreparationConfig,
    ) -> Result<(), moose_core::plugin::PreparationError> {
        config.validate()?;
        self.params.set_sample_rate(config.audio.sample_rate);
        self.params.snap_smoothers();
        L::prepare(&mut self.state, &self.params, config)?;
        self.sample_rate = config.audio.sample_rate;
        Ok(())
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Apply parameter change events to the shell's params.
        // ParamChange values from format wrappers are PLAIN (already
        // denormalized). `set_normalized` here would double-denormalize.
        for e in events.iter() {
            if let EventBody::ParamChange { id, value } = &e.body {
                self.params.set_plain(*id, *value);
            }
        }

        // No sync needed - plugin reads from the same Arc<Params>.

        // Build a ProcessContext with param/meter callbacks for the logic.
        let latency_change_request = context.deferred_latency_change_request();
        let params = &self.params;
        let meters = &self.meters;
        let param_fn = |id: u32| -> f64 { params.get_plain(id).unwrap_or(0.0) };
        let meter_fn = |id: u32, v: f32| meters.write(id, v);
        let ctx = ProcessContext::new(
            context.transport,
            context.sample_rate,
            buffer.num_samples(),
            &mut *context.output_events,
        )
        .with_process_mode(context.process_mode)
        .with_bus_routing(context.bus_routing)
        .with_params(&param_fn)
        .with_meters(&meter_fn);
        let ctx = match latency_change_request {
            Some(request) => ctx.with_deferred_latency_changes(request),
            None => ctx,
        };
        // Stamp the background-task spawner so `ctx.tasks::<T>()` works.
        let mut ctx = match &self.tasks {
            Some(t) => ctx.with_tasks(t),
            None => ctx,
        };

        let status = L::process(&mut self.state, &self.params, buffer, events, &mut ctx);
        if let Some(ticket) = self.snapshots.pending_request() {
            // A host save (or a just-applied load) asked for live state.
            // Runs once per request, not per block; `save_state` may
            // allocate, which the request accepts.
            moose_core::rt::allow_alloc(|| {
                self.snapshots
                    .serve(ticket, |buf| live_snapshot::<S, L>(&self.state, buf));
            });
        }
        status
    }

    fn save_state(&self) -> Vec<u8> {
        L::save_state(&self.state)
    }

    fn save_state_with_context(&self, context: StateContext) -> Option<Vec<u8>> {
        L::save_state_with_context(&self.state, context)
    }

    fn republish_snapshot(&mut self) {
        // Host thread, plugin owned: serialize live.
        self.snapshots
            .store_inline(|buf| live_snapshot::<S, L>(&self.state, buf));
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), StateLoadError> {
        let result = L::load_state(&mut self.state, data);
        // Plugin-side cache invalidation runs in the same `&mut`
        // borrow window so the next `process()` block sees the
        // refreshed caches - fire it whether or not load_state
        // succeeded so partial state still triggers a refresh.
        L::state_changed(&mut self.state, &self.params);
        result
    }

    fn load_state_with_context(
        &mut self,
        data: &[u8],
        context: StateContext,
    ) -> Result<(), StateLoadError> {
        let result = L::load_state_with_context(&mut self.state, data, context);
        L::state_changed_with_context(&mut self.state, &self.params, context);
        result
    }

    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState>
    where
        Self: Sized,
    {
        <L as PluginLogicCore<S>>::migrate_state(foreign)
    }

    fn state_schema_version() -> u32
    where
        Self: Sized,
    {
        <L as PluginLogicCore<S>>::state_schema_version()
    }

    fn state_migrations() -> moose_core::state::StateMigrationChain
    where
        Self: Sized,
    {
        <L as PluginLogicCore<S>>::state_migrations()
    }

    fn validate_state(data: &[u8]) -> Result<(), StateLoadError>
    where
        Self: Sized,
    {
        <L as PluginLogicCore<S>>::validate_state(data)
    }

    fn validate_state_with_context(data: &[u8], context: StateContext) -> Result<(), StateLoadError>
    where
        Self: Sized,
    {
        <L as PluginLogicCore<S>>::validate_state_with_context(data, context)
    }

    fn decode_state(data: &[u8]) -> Result<Option<DecodedState>, StateLoadError>
    where
        Self: Sized,
    {
        <L as PluginLogicCore<S>>::decode_state(data)
    }

    fn decode_state_with_context(
        data: &[u8],
        context: StateContext,
    ) -> Result<Option<DecodedState>, StateLoadError>
    where
        Self: Sized,
    {
        <L as PluginLogicCore<S>>::decode_state_with_context(data, context)
    }

    /// Swap an off-thread decoded state into the live processor and run
    /// `L::state_changed`.
    ///
    /// When a plugin opts into off-thread decoding (`decode_state` returning
    /// `Ok(Some(_))`), queued audio-thread loads call this method inside an
    /// `RtSection` — so `L::state_changed` **must** also be real-time safe
    /// (zero allocations, locks, or blocking). Plugins returning `Ok(None)`
    /// continue using `load_state` outside `RtSection`.
    fn apply_decoded_state(&mut self, decoded: &mut DecodedState) {
        L::apply_decoded_state(&mut self.state, &self.params, decoded);
        L::state_changed(&mut self.state, &self.params);
    }

    fn apply_decoded_state_with_context(
        &mut self,
        decoded: &mut DecodedState,
        context: StateContext,
    ) {
        L::apply_decoded_state_with_context(&mut self.state, &self.params, decoded, context);
        L::state_changed_with_context(&mut self.state, &self.params, context);
    }

    fn apply_decoded_persist(&mut self, decoded: &mut DecodedState, context: StateContext) {
        L::apply_decoded_persist(&mut self.state, &self.params, decoded, context);
        L::state_changed_with_context(&mut self.state, &self.params, context);
    }

    fn latency(&self) -> u32 {
        L::latency(&self.state)
    }
    fn tail(&self) -> u32 {
        L::tail(&self.state)
    }

    fn get_meter(&self, meter_id: u32) -> f32 {
        self.meters.read(meter_id)
    }
}

/// Live custom state serialization via `PluginLogicCore::save_state`.
fn live_snapshot<S: Sample, L: PluginLogicCore<S>>(state: &L::DspState, buf: &mut Vec<u8>) {
    buf.clear();
    buf.extend_from_slice(&L::save_state(state));
}

// ---------------------------------------------------------------------------
// export_static! macro
// ---------------------------------------------------------------------------

/// Compile-time static embedding of a `PluginLogicCore` adapter into the binary.
///
/// Produces a `__HotShellWrapper` struct that implements `PluginRuntime + PluginExport`,
/// so format export macros (`export_clap!`, `export_vst3!`, etc.) work unchanged.
/// No dlopen, no file watcher, zero runtime overhead. Bus layouts come from
/// `<$logic as PluginLogicCore<Sample>>::bus_layouts()`. Override the
/// corresponding `Plugin` method to pick a layout other than stereo.
///
/// ```ignore
/// export_static! {
///     params: GainParams,
///     info: plugin_info!(...),
///     logic: Gain,
/// }
///
/// #[cfg(feature = "clap")]
/// moose_clap::export_clap!(__HotShellWrapper);
/// ```
#[macro_export]
macro_rules! export_static {
    (
        params: $params:ty,
        info: $info:expr,
        $(vst3_class_id: $vst3_class_id:expr,)?
        logic: $logic:ty,
        $(tasks: [$($task:ty),+],)?
    ) => {
        pub struct __HotShellWrapper {
            // `Sample` here resolves to the type alias the user
            // imported from a prelude (`prelude` / `prelude32` →
            // `f32`; `prelude64` → `f64`; `prelude64m` → `f32`). The
            // `PluginLogic<Sample>` bound on the user's impl must
            // match this, so the prelude is what picks the audio
            // buffer precision end-to-end.
            inner: $crate::static_shell::StaticShell<$params, $logic, Sample>,
        }

        impl __HotShellWrapper {
            fn __moose_create(
                instance: ::core::option::Option<
                    $crate::__macro_deps::moose_core::tasks::InstanceContext,
                >,
            ) -> Self {
                let params = <$params as $crate::__macro_deps::moose_params::Params>::new_arc();
                // Each `tasks: [..]` type gets its own lane (queue + mode).
                // The bundle collapses to `None` when no types were listed,
                // so a plugin with no tasks runs with no pool.
                #[allow(unused_mut)]
                let mut __task_bundle =
                    $crate::__macro_deps::moose_core::tasks::TaskSpawnerBundle::new();
                $(
                    $({
                        let __task_run = {
                            let params = std::sync::Arc::clone(&params);
                            move |task| {
                                <$task as $crate::__macro_deps::moose_plugin::BackgroundTask>::run_once(
                                    task, &params,
                                )
                            }
                        };
                        // `SERIALIZED` picks one-slot vs concurrent draining
                        // for this lane; the const folds the branch at
                        // compile time.
                        let __spawner = if <$task as $crate::__macro_deps::moose_plugin::BackgroundTask>::WAKE_ON_SCHEDULE {
                            $crate::__macro_deps::moose_core::tasks::TaskSpawner::<$task>::new_managed_waking(__task_run, <$task as $crate::__macro_deps::moose_plugin::BackgroundTask>::SERIALIZED)
                        } else if <$task as $crate::__macro_deps::moose_plugin::BackgroundTask>::SERIALIZED {
                            $crate::__macro_deps::moose_core::tasks::TaskSpawner::<$task>::new_managed_serialized(__task_run)
                        } else {
                            $crate::__macro_deps::moose_core::tasks::TaskSpawner::<$task>::new_managed(__task_run)
                        };
                        __task_bundle.push(__spawner);
                    })+
                )?
                let tasks = __task_bundle.into_any();
                // The descriptor `$logic` is stateless; `from_parts`
                // builds the DSP state via `<$logic>::init(&params, &cx)`.
                Self {
                    inner: $crate::static_shell::StaticShell::from_parts(params, tasks, instance),
                }
            }
        }

        impl $crate::__macro_deps::moose_core::plugin::PluginRuntime for __HotShellWrapper {
            type Sample = Sample;

            fn supports_in_place() -> bool
            where
                Self: Sized,
            {
                // `PluginLogicCore<Sample>` is the wrapper-facing
                // trait; the user impl'd one of the leaf traits
                // (`PluginLogic` / `PluginLogic64`), and the blanket
                // bridge defined alongside those traits in
                // `moose-plugin` makes them also satisfy
                // `PluginLogicCore<Sample>` automatically. Sample
                // resolves through the prelude alias in scope at the
                // macro call site.
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::supports_in_place()
            }

            fn max_host_state_bytes() -> usize {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::max_host_state_bytes()
            }

            fn info() -> $crate::__macro_deps::moose_core::info::PluginInfo
            where
                Self: Sized,
            {
                $info
            }

            $(
                fn vst3_class_id() -> Option<[u8; 16]>
                where
                    Self: Sized,
                {
                    $vst3_class_id
                }
            )?

            fn bus_layouts() -> Vec<$crate::__macro_deps::moose_core::bus::BusLayout>
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::bus_layouts()
            }

            fn init(&mut self) {
                self.inner.init();
            }

            fn reset(&mut self, config: &$crate::__macro_deps::moose_core::config::AudioConfig) {
                self.inner.reset(config);
            }

            fn requires_fallible_preparation(&self)->bool{self.inner.requires_fallible_preparation()}

            fn prepare(&mut self,
                config: &$crate::__macro_deps::moose_core::plugin::PreparationConfig,
            ) -> Result<(), $crate::__macro_deps::moose_core::plugin::PreparationError> {
                self.inner.prepare(config)
            }

            fn process(
                &mut self,
                buffer: &mut $crate::__macro_deps::moose_core::buffer::AudioBuffer<Sample>,
                events: &$crate::__macro_deps::moose_core::events::EventList,
                context: &mut $crate::__macro_deps::moose_core::process::ProcessContext,
            ) -> $crate::__macro_deps::moose_core::process::ProcessStatus {
                self.inner.process(buffer, events, context)
            }

            fn save_state(&self) -> Vec<u8> {
                self.inner.save_state()
            }

            fn save_state_with_context(
                &self,
                context: $crate::__macro_deps::moose_core::state::StateContext,
            ) -> Option<Vec<u8>> {
                self.inner.save_state_with_context(context)
            }

            fn snapshot_into(&self, buf: &mut Vec<u8>) -> bool {
                self.inner.snapshot_into(buf)
            }

            fn load_state(
                &mut self,
                data: &[u8],
            ) -> Result<(), $crate::__macro_deps::moose_core::state::StateLoadError> {
                self.inner.load_state(data)
            }

            fn load_state_with_context(
                &mut self,
                data: &[u8],
                context: $crate::__macro_deps::moose_core::state::StateContext,
            ) -> Result<(), $crate::__macro_deps::moose_core::state::StateLoadError> {
                self.inner.load_state_with_context(data, context)
            }

            fn republish_snapshot(&mut self) {
                self.inner.republish_snapshot();
            }

            fn migrate_state(
                foreign: &$crate::__macro_deps::moose_core::state::ForeignState,
            ) -> Option<$crate::__macro_deps::moose_core::state::MigratedState>
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::migrate_state(foreign)
            }

            fn state_schema_version() -> u32
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::state_schema_version()
            }

            fn state_migrations() -> $crate::__macro_deps::moose_core::state::StateMigrationChain
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::state_migrations()
            }

            fn validate_state(
                data: &[u8],
            ) -> Result<(), $crate::__macro_deps::moose_core::state::StateLoadError>
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::validate_state(data)
            }

            fn validate_state_with_context(
                data: &[u8],
                context: $crate::__macro_deps::moose_core::state::StateContext,
            ) -> Result<(), $crate::__macro_deps::moose_core::state::StateLoadError>
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::validate_state_with_context(data, context)
            }

            fn decode_state(
                data: &[u8],
            ) -> Result<
                Option<$crate::__macro_deps::moose_core::state::DecodedState>,
                $crate::__macro_deps::moose_core::state::StateLoadError,
            >
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::decode_state(data)
            }

            fn decode_state_with_context(
                data: &[u8],
                context: $crate::__macro_deps::moose_core::state::StateContext,
            ) -> Result<
                Option<$crate::__macro_deps::moose_core::state::DecodedState>,
                $crate::__macro_deps::moose_core::state::StateLoadError,
            >
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::decode_state_with_context(data, context)
            }

            fn apply_decoded_state(
                &mut self,
                decoded: &mut $crate::__macro_deps::moose_core::state::DecodedState,
            ) {
                self.inner.apply_decoded_state(decoded);
            }

            fn apply_decoded_state_with_context(
                &mut self,
                decoded: &mut $crate::__macro_deps::moose_core::state::DecodedState,
                context: $crate::__macro_deps::moose_core::state::StateContext,
            ) {
                self.inner.apply_decoded_state_with_context(decoded, context);
            }

            fn apply_decoded_persist(
                &mut self,
                decoded: &mut $crate::__macro_deps::moose_core::state::DecodedState,
                context: $crate::__macro_deps::moose_core::state::StateContext,
            ) {
                self.inner.apply_decoded_persist(decoded, context);
            }

            fn latency(&self) -> u32 {
                self.inner.latency()
            }
            fn tail(&self) -> u32 {
                self.inner.tail()
            }
            fn get_meter(&self, meter_id: u32) -> f32 {
                self.inner.get_meter(meter_id)
            }
        }

        impl $crate::__macro_deps::moose_core::export::PluginExport for __HotShellWrapper {
            type Params = $params;

            fn clap_extension(id: &::std::ffi::CStr) -> Option<$crate::__macro_deps::moose_core::export::CustomClapExtension> {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::clap_extension(id)
            }

            fn with_dsp_state(&mut self, callback: &mut dyn FnMut(&mut dyn ::std::any::Any)) -> bool {
                callback(self.inner.state_ref_mut());
                true
            }


            fn create() -> Self {
                Self::__moose_create(None)
            }

            fn create_for(
                instance: $crate::__macro_deps::moose_core::tasks::InstanceContext,
            ) -> Self {
                Self::__moose_create(Some(instance))
            }

            fn params(&self) -> &$params {
                &self.inner.params
            }

            fn params_arc(&self) -> std::sync::Arc<$params> {
                std::sync::Arc::clone(&self.inner.params)
            }

            fn meter_store(
                &self,
            ) -> std::sync::Arc<$crate::__macro_deps::moose_core::meters::MeterStore> {
                self.inner.meter_store()
            }

            fn snapshot_slot(
                &self,
            ) -> std::sync::Arc<$crate::__macro_deps::moose_core::snapshot::SnapshotSlot> {
                self.inner.snapshot_slot()
            }

            fn task_spawner(
                &self,
            ) -> ::core::option::Option<
                $crate::__macro_deps::moose_core::tasks::AnyTaskSpawner,
            > {
                self.inner.task_spawner()
            }

            fn main_thread(
                &self,
            ) -> Box<dyn $crate::__macro_deps::moose_core::main_thread::MainThreadHooks> {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::main_thread(
                    &self.inner.params,
                )
            }

            fn midi2_controller_assignments(
                direction: $crate::__macro_deps::moose_core::main_thread::BusDirection,
            ) -> Vec<$crate::__macro_deps::moose_core::main_thread::Midi2ControllerAssignment> {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::midi2_controller_assignments(
                    direction,
                )
            }

            fn decode_persist_state(
                params: &$params,
                persist: &[u8],
                context: $crate::__macro_deps::moose_core::state::StateContext,
            ) -> Result<
                Option<$crate::__macro_deps::moose_core::state::DecodedState>,
                $crate::__macro_deps::moose_core::state::StateLoadError,
            > {
                <$logic as $crate::__macro_deps::moose_plugin::PluginLogicCore<Sample>>::decode_persist_state(
                    params,
                    persist,
                    context,
                )
            }

            fn editor_builder(
                &self,
            ) -> $crate::__macro_deps::moose_core::editor::EditorBuilder<$params> {
                // Builds from the lock-free param store, never the
                // embedded logic - the audio thread's `&mut logic` is
                // irrelevant here, so opening the editor takes no lock.
                Box::new(|params| {
                    Some(
                        <$logic as $crate::__macro_deps::moose_plugin::PluginEditor<Sample>>::editor(
                            params,
                        ),
                    )
                })
            }
        }
    };
}
