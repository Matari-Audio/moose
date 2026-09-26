//! Hot-reload mechanics for moose: dylib loading, ABI canary, and the
//! shells (`HotShell<P, S>`, `StaticShell<P, L, S>`) that bridge the
//! user-facing `moose_plugin::PluginLogic` / `moose_plugin::PluginLogic64`
//! leaf traits onto [`moose_core::PluginRuntime`] for format wrappers.
//!
//! Plugin authors don't reach into this crate directly. They write
//! `impl PluginLogic for MyPlugin` (the leaf trait is sample-pinned
//! via the prelude re-export) and the `moose::plugin!` macro picks
//! the static or hot shell based on the `shell` Cargo feature.
//!
//! # ABI boundary
//!
//! `PluginLogic` is a stateless descriptor with a separate `type DspState`,
//! so the DSP state can live in the *shell* rather than the reloadable
//! dylib. The dylib exports a flat set of Rust-ABI functions
//! (`export_plugin!`) over an opaque `*mut ()` state pointer (an erased
//! `Box<State>`) plus the shell's `Arc<Params>` pointer. `HotShell` owns
//! the state and, on a reload, carries it into the new code by a save /
//! load round-trip through the plugin's persistence blob (when
//! `PRESERVE_DSP_STATE` is set) - so a reverb tail survives the swap
//! whenever the plugin serializes it - and re-inits otherwise.
//! `StaticShell` holds a typed `L::DspState` directly.
//!
//! ```ignore
//! use moose_loader::{AbiCanary, PluginLogic, PluginLogicCore};
//!
//! struct MyPlugin;                 // stateless descriptor
//! impl PluginLogic for MyPlugin { type DspState = MyState; /* ... */ }
//!
//! // Emitted by `moose::plugin!` (plugin authors don't write these).
//! // `Sample` resolves through the prelude alias (`f32` for `prelude` /
//! // `prelude32` / `prelude64m`, `f64` for `prelude64`).
//! #[unsafe(no_mangle)]
//! pub fn moose_init_state(
//!     params: *const (),
//!     tasks: Option<AnyTaskSpawner>,
//! ) -> *mut () { /* Box<State> */ }
//! #[unsafe(no_mangle)]
//! pub fn moose_process(state: *mut (), params: *const (), /* ... */) { }
//!
//! #[unsafe(no_mangle)]
//! pub fn moose_abi_canary_v2() -> AbiCanary { AbiCanary::current::<Sample>() }
//! ```

#[doc(hidden)]
pub mod __macro_deps {
    pub use moose_core;
    // `moose_plugin` carries the `PluginLogicCore` blanket the
    // `export_plugin!` / `export_static!` macros need to name
    // (`<L as PluginLogicCore<Sample>>::supports_in_place()` etc.).
    // Re-exported here so the macro can resolve it via
    // `$crate::__macro_deps::moose_plugin` regardless of whether the
    // caller has `moose-plugin` as a direct dep.
    pub use moose_plugin;
}

mod canary;
mod safe_types;

#[cfg(feature = "shell")]
mod loader;
#[cfg(feature = "shell")]
pub mod shell;
pub mod static_shell;

pub use canary::{ABI_EPOCH, AbiCanary};
pub use safe_types::*;
// Source the leaf + core traits directly from `moose-plugin` rather
// than via the optional `moose-gui` re-export, so these names are
// reachable regardless of whether the `builtin-gui` feature is on.
pub use moose_plugin::{PluginLogic, PluginLogic64, PluginLogicCore};

#[cfg(feature = "shell")]
pub use loader::NativeLoader;

/// Export the `#[unsafe(no_mangle)]` symbols the hot-reload shell binds.
///
/// The dylib no longer hands the shell a `Box<dyn PluginLogicCore>`
/// trait object. Instead it exports a flat set of Rust-ABI functions
/// that operate on an **opaque state pointer** (`*mut ()`, an erased
/// `Box<State>`): the shell owns the state, so on a hot-reload it can
/// serialize the state through the origin dylib and restore it into
/// freshly-init'd state under the new code (gated by the
/// `moose_preserve_dsp_state` export).
///
/// `params_ptr` is a raw `Arc<Params>` pointer from the shell; each call
/// borrows `&Params` from it (no refcount change - the shell keeps the
/// `Arc` alive for the call's duration). `Sample` is the prelude's
/// `type Sample` alias (`f32` for `prelude` / `prelude32` / `prelude64m`,
/// `f64` for `prelude64`); the canary's `sample_precision` byte guards a
/// precision-mismatched load.
#[macro_export]
macro_rules! export_plugin {
    ($logic:ty, $params:ty $(, tasks: [$($task:ty),+])?) => {
        /// Start this logic generation's own task pool before any DSP call
        /// can schedule onto it. The shell calls this export on its loader
        /// thread before activation and rejects the generation if no worker
        /// can be created.
        #[unsafe(no_mangle)]
        pub fn moose_warm_tasks() -> bool {
            #[allow(unused_mut)]
            let mut ready = true;
            $(
                let _ = ::core::marker::PhantomData::<($($task,)+)>;
                ready = $crate::__macro_deps::moose_core::tasks::warm_hot_reload_pool();
            )?
            ready
        }

        /// Pause this generation at a worker-entry boundary. A timeout is
        /// reversible: the pool resumes without losing its queued work.
        #[unsafe(no_mangle)]
        pub fn moose_quiesce_tasks(timeout: ::std::time::Duration) -> bool {
            #[allow(unused_mut)]
            let mut quiescent = true;
            $(
                let _ = ::core::marker::PhantomData::<($($task,)+)>;
                quiescent = $crate::__macro_deps::moose_core::tasks::quiesce_hot_reload_pool(timeout);
            )?
            quiescent
        }

        /// Stop and join this generation's worker pool. The loader closes all
        /// task lanes before calling this off-thread.
        #[unsafe(no_mangle)]
        pub fn moose_shutdown_tasks() {
            $(
                let _ = ::core::marker::PhantomData::<($($task,)+)>;
                $crate::__macro_deps::moose_core::tasks::shutdown_hot_reload_pool();
            )?
        }

        /// Build this logic generation's typed managed-task lanes. The
        /// queues and handler vtables originate in the same dylib as the
        /// task values that `init` / `process` enqueue into them.
        #[unsafe(no_mangle)]
        pub fn moose_build_tasks(
            params_ptr: *const (),
        ) -> ::core::option::Option<
            $crate::__macro_deps::moose_core::tasks::AnyTaskSpawner,
        > {
            #[allow(unused_mut)]
            let mut bundle =
                $crate::__macro_deps::moose_core::tasks::TaskSpawnerBundle::new();
            $(
                // SAFETY: the shell passes `Arc::as_ptr(&params)` and owns
                // that Arc for the complete loader/task-generation lifetime.
                let params: Arc<$params> = unsafe {
                    Arc::increment_strong_count(params_ptr.cast::<$params>());
                    Arc::from_raw(params_ptr.cast::<$params>())
                };
                $(
                    let run = {
                        let params = Arc::clone(&params);
                        move |task| {
                            <$task as $crate::__macro_deps::moose_plugin::BackgroundTask>::run_once(
                                task,
                                &params,
                            )
                        }
                    };
                    let spawner = if <$task as $crate::__macro_deps::moose_plugin::BackgroundTask>::SERIALIZED {
                        $crate::__macro_deps::moose_core::tasks::TaskSpawner::<$task>::new_managed_serialized(run)
                    } else {
                        $crate::__macro_deps::moose_core::tasks::TaskSpawner::<$task>::new_managed(run)
                    };
                    bundle.push(spawner);
                )+
            )?
            bundle.into_any()
        }

        /// Build the initial DSP state; returns an erased `Box<State>`.
        #[unsafe(no_mangle)]
        pub fn moose_init_state(
            params_ptr: *const (),
            tasks: ::core::option::Option<
                $crate::__macro_deps::moose_core::tasks::AnyTaskSpawner,
            >,
        ) -> *mut () {
            let params: &$params = unsafe { &*(params_ptr as *const $params) };
            let cx = $crate::__macro_deps::moose_core::tasks::InitContext::new(tasks);
            let state = <$logic as $crate::PluginLogicCore<Sample>>::init(params, &cx);
            Box::into_raw(Box::new(state)).cast::<()>()
        }

        /// Drop a state allocated by *this* dylib's `moose_init_state`.
        /// Called by the shell through the origin dylib (kept mapped by
        /// the loader's retired-generation ownership) so `State`'s `Drop` runs
        /// with the code that produced it.
        #[unsafe(no_mangle)]
        pub fn moose_drop_state(state: *mut ()) {
            drop(unsafe {
                Box::from_raw(state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>())
            });
        }

        /// Whether the shell should carry live DSP state across a reload
        /// (the plugin's `PRESERVE_DSP_STATE`). Carry-over is a save / load
        /// round-trip through this dylib's persistence blob, never a raw
        /// reinterpretation of the old bytes, so it stays sound even when
        /// the `State` layout changed between builds.
        #[unsafe(no_mangle)]
        pub fn moose_preserve_dsp_state() -> bool {
            <$logic as $crate::PluginLogicCore<Sample>>::PRESERVE_DSP_STATE
        }

        #[unsafe(no_mangle)]
        pub fn moose_reset(
            state: *mut (),
            params_ptr: *const (),
            config: &$crate::__macro_deps::moose_core::config::AudioConfig,
        ) {
            let state = unsafe {
                &mut *state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>()
            };
            let params: &$params = unsafe { &*(params_ptr as *const $params) };
            <$logic as $crate::PluginLogicCore<Sample>>::reset(state, params, config);
        }

        #[unsafe(no_mangle)]
        pub fn moose_process(
            state: *mut (),
            params_ptr: *const (),
            buffer: &mut $crate::__macro_deps::moose_core::buffer::AudioBuffer<Sample>,
            events: &$crate::__macro_deps::moose_core::events::EventList,
            ctx: &mut $crate::__macro_deps::moose_core::process::ProcessContext,
        ) -> $crate::__macro_deps::moose_core::process::ProcessStatus {
            let state = unsafe {
                &mut *state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>()
            };
            let params: &$params = unsafe { &*(params_ptr as *const $params) };
            <$logic as $crate::PluginLogicCore<Sample>>::process(state, params, buffer, events, ctx)
        }

        #[unsafe(no_mangle)]
        pub fn moose_latency(state: *const ()) -> u32 {
            let state =
                unsafe { &*state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>() };
            <$logic as $crate::PluginLogicCore<Sample>>::latency(state)
        }

        #[unsafe(no_mangle)]
        pub fn moose_tail(state: *const ()) -> u32 {
            let state =
                unsafe { &*state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>() };
            <$logic as $crate::PluginLogicCore<Sample>>::tail(state)
        }

        #[unsafe(no_mangle)]
        pub fn moose_save_state(state: *const ()) -> Vec<u8> {
            let state =
                unsafe { &*state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>() };
            <$logic as $crate::PluginLogicCore<Sample>>::save_state(state)
        }

        #[unsafe(no_mangle)]
        pub fn moose_snapshot_into(state: *const (), buf: &mut Vec<u8>) -> bool {
            let state =
                unsafe { &*state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>() };
            <$logic as $crate::PluginLogicCore<Sample>>::snapshot_into(state, buf)
        }

        #[unsafe(no_mangle)]
        pub fn moose_snapshot_version(state: *const ()) -> ::core::option::Option<u64> {
            let state =
                unsafe { &*state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>() };
            <$logic as $crate::PluginLogicCore<Sample>>::snapshot_version(state)
        }

        #[unsafe(no_mangle)]
        pub fn moose_load_state(
            state: *mut (),
            data: &[u8],
        ) -> Result<(), $crate::__macro_deps::moose_core::state::StateLoadError> {
            let state = unsafe {
                &mut *state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>()
            };
            <$logic as $crate::PluginLogicCore<Sample>>::load_state(state, data)
        }

        #[unsafe(no_mangle)]
        pub fn moose_state_changed(state: *mut (), params_ptr: *const ()) {
            let state = unsafe {
                &mut *state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>()
            };
            let params: &$params = unsafe { &*(params_ptr as *const $params) };
            <$logic as $crate::PluginLogicCore<Sample>>::state_changed(state, params);
        }

        // Editor construction lives in its own symbol: it is
        // receiverless (over the shared `Arc<Params>`), so the shell
        // rebuilds the editor from this dylib's `$logic` without
        // touching the DSP state. A reload swaps in the new editor
        // code - the host picks it up on the next editor close+open.
        #[unsafe(no_mangle)]
        pub fn moose_build_editor(
            params_ptr: *const (),
        ) -> Box<dyn $crate::__macro_deps::moose_core::editor::Editor> {
            let params: Arc<$params> = unsafe {
                Arc::increment_strong_count(params_ptr as *const $params);
                Arc::from_raw(params_ptr as *const $params)
            };
            <$logic as $crate::__macro_deps::moose_plugin::PluginEditor<Sample>>::editor(params)
        }

        // `_v2` because `AbiCanary` crosses this boundary *by value*
        // (sret): if the two sides disagreed about its size, the call
        // itself would corrupt the caller's stack before any field
        // compare. A canary-layout change therefore renames the symbol,
        // so a mismatched pair fails at `dlsym` - cleanly.
        #[unsafe(no_mangle)]
        pub fn moose_abi_canary_v2() -> $crate::AbiCanary {
            $crate::AbiCanary::current::<Sample>()
        }
    };
}
