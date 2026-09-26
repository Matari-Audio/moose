//! `NativeLoader` - loads and hot-reloads a plugin dylib.
//!
//! Uses native Rust ABI (no C translation layer). Verifies
//! compatibility via `AbiCanary` + symbol presence before use. The
//! dylib exports a flat set of functions over an opaque `*mut ()` state
//! pointer (see `export_plugin!`); the loader resolves them into
//! [`LogicSymbols`]. The DSP state itself is owned by the shell
//! ([`crate::shell::HotShell`]), not the loader, so it can survive a
//! reload - the loader only swaps the code.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

/// Process-wide counter assigning a unique `instance_id` to each
/// `NativeLoader` constructed in this process. Used as a tiebreaker
/// in temp-file names so two plugins hot-reloading the same dylib
/// path (multi-instance / dual-bus session) can't collide on a
/// `<stem>-moose<id>.so` filename.
///
/// A truly per-instance counter wouldn't help: each `NativeLoader`
/// needs an ID *unique among other `NativeLoaders` in the same
/// process*, and only a process-scoped atomic can guarantee that.
/// `Relaxed` ordering is sufficient - the only consumer is the
/// owning `NativeLoader`, which reads the value back from its own
/// `instance_id` field, never via a re-load of `LOADER_ID`.
static LOADER_ID: AtomicU64 = AtomicU64::new(0);

use libloading::{Library, Symbol};

use crate::canary::AbiCanary;
use moose_core::buffer::AudioBuffer;
use moose_core::config::AudioConfig;
use moose_core::events::EventList;
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::state::StateLoadError;
use moose_core::tasks::AnyTaskSpawner;
use moose_params::sample::Sample;

/// Handlers are contractually short/nonblocking. A reload closes admission
/// and gives admitted schedulers plus in-flight handlers this bounded window
/// to leave; a missed deadline reopens the old generation unchanged.
const TASK_RETIRE_WAIT: Duration = Duration::from_millis(250);

/// The `moose_process` export's signature (state, params, buffer,
/// events, ctx) -> status. Aliased to keep [`LogicSymbols`] readable.
type ProcessFn<S> =
    fn(*mut (), *const (), &mut AudioBuffer<S>, &EventList, &mut ProcessContext) -> ProcessStatus;

/// The `moose_drop_state` export's signature. The shell keeps one of
/// these alongside its state so the allocation is freed by the dylib
/// that made it, even after a reload.
pub type StateDropFn = fn(*mut ());

/// The subset of a dylib's exports the shell binds to the exact state
/// allocation that dylib produced, so it can operate on that state even
/// after a reload swaps the active symbol table. Both are bare `fn`
/// pointers into the origin dylib's code. Every activated generation stays
/// mapped for the rest of the process, so these pointers remain valid even
/// when an editor or wrapper object outlives the loader instance.
#[derive(Clone, Copy)]
pub struct StateOrigin {
    /// Frees the allocation with the layout that made it.
    pub drop: StateDropFn,
    /// Serializes the live state into the plugin's persistence blob, so a
    /// reload can restore it into freshly-init'd state under the new code
    /// rather than reinterpret the old bytes - which is UB when the layout
    /// changed in a way size / align can't see.
    pub save: fn(*const ()) -> Vec<u8>,
}

/// The flat function-pointer table resolved from a loaded dylib. Every
/// entry operates on an opaque `*mut ()` / `*const ()` state pointer
/// (an erased `Box<State>`) plus a `*const ()` params pointer (the
/// shell's `Arc<Params>`). The loader keeps every activated generation
/// mapped for the rest of the process.
struct LogicSymbols<S: Sample> {
    warm_tasks: fn() -> bool,
    quiesce_tasks: fn(Duration) -> bool,
    shutdown_tasks: fn(),
    build_tasks: fn(*const ()) -> Option<AnyTaskSpawner>,
    init_state: fn(*const (), Option<AnyTaskSpawner>) -> *mut (),
    drop_state: StateDropFn,
    reset: fn(*mut (), *const (), &AudioConfig),
    process: ProcessFn<S>,
    latency: fn(*const ()) -> u32,
    tail: fn(*const ()) -> u32,
    save_state: fn(*const ()) -> Vec<u8>,
    snapshot_into: fn(*const (), &mut Vec<u8>) -> bool,
    snapshot_version: fn(*const ()) -> Option<u64>,
    load_state: fn(*mut (), &[u8]) -> Result<(), StateLoadError>,
    state_changed: fn(*mut (), *const ()),
    /// Whether the plugin opts into DSP-state carry-over across a reload
    /// (`PRESERVE_DSP_STATE`). When false the shell re-inits on every
    /// reload instead of round-tripping the state through save / load.
    preserve: bool,
}

impl<S: Sample> LogicSymbols<S> {
    /// Resolve every exported symbol from `lib`. Returns `None` (and
    /// logs) if any is missing - a stale dylib built before the flat ABI
    /// won't have them, and is refused rather than half-bound.
    ///
    /// # Safety
    /// `lib` must be a moose logic dylib whose `AbiCanary` already
    /// matched the shell (checked before this call), so each symbol has
    /// the signature named here.
    unsafe fn resolve(lib: &Library) -> Option<Self> {
        // Each `*sym` copies the bare `fn` pointer out of the borrowed
        // `Symbol`; it stays valid as long as `lib`'s code is mapped,
        // which the loader guarantees for every bound/retired generation.
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let s: Symbol<$ty> = match unsafe { lib.get($name) } {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!(
                            "missing export {} (stale pre-flat-ABI dylib?): {e}",
                            std::str::from_utf8($name).unwrap_or("?")
                        );
                        return None;
                    }
                };
                *s
            }};
        }
        let preserve_fn: fn() -> bool = sym!(b"moose_preserve_dsp_state", fn() -> bool);
        Some(Self {
            warm_tasks: sym!(b"moose_warm_tasks", fn() -> bool),
            quiesce_tasks: sym!(b"moose_quiesce_tasks", fn(Duration) -> bool),
            shutdown_tasks: sym!(b"moose_shutdown_tasks", fn()),
            build_tasks: sym!(
                b"moose_build_tasks",
                fn(*const ()) -> Option<AnyTaskSpawner>
            ),
            init_state: sym!(
                b"moose_init_state",
                fn(*const (), Option<AnyTaskSpawner>) -> *mut ()
            ),
            drop_state: sym!(b"moose_drop_state", fn(*mut ())),
            reset: sym!(b"moose_reset", fn(*mut (), *const (), &AudioConfig)),
            process: sym!(b"moose_process", ProcessFn<S>),
            latency: sym!(b"moose_latency", fn(*const ()) -> u32),
            tail: sym!(b"moose_tail", fn(*const ()) -> u32),
            save_state: sym!(b"moose_save_state", fn(*const ()) -> Vec<u8>),
            snapshot_into: sym!(b"moose_snapshot_into", fn(*const (), &mut Vec<u8>) -> bool),
            snapshot_version: sym!(b"moose_snapshot_version", fn(*const ()) -> Option<u64>),
            load_state: sym!(
                b"moose_load_state",
                fn(*mut (), &[u8]) -> Result<(), StateLoadError>
            ),
            state_changed: sym!(b"moose_state_changed", fn(*mut (), *const ())),
            preserve: preserve_fn(),
        })
    }
}

/// Verified candidate dylib + resolved symbol table, ready to swap in.
struct Candidate<S: Sample> {
    tasks: Option<AnyTaskSpawner>,
    symbols: LogicSymbols<S>,
    /// Declared after `tasks` so automatic field drop keeps the code mapped
    /// until every task queue/handler from this rejected candidate is gone.
    library: Library,
    hash: u32,
    mtime: SystemTime,
    /// Path of the versioned copy in the system temp dir. Rejected candidates
    /// remove it; activated generations retain it with their permanent map.
    temp_path: PathBuf,
}

struct RetiredGeneration {
    tasks: Option<AnyTaskSpawner>,
    library: Library,
}

/// Manages a hot-reloadable plugin dylib.
///
/// Generic over `S` (the plugin's sample type - `f32` by default, the
/// host-wire format). A `prelude64` plugin built into a logic dylib
/// must be loaded by an `S = f64` shell; the precision is also baked
/// into [`AbiCanary::sample_precision`] so a mismatch fails the canary
/// check rather than silently binding to a wrong-shape vtable.
pub struct NativeLoader<S: Sample = f32> {
    dylib_path: PathBuf,
    library: Option<Library>,
    /// Resolved flat-ABI function table for the currently loaded dylib.
    /// `None` before the first successful load. State is not held here -
    /// the shell owns it so it can survive a reload.
    symbols: Option<LogicSymbols<S>>,
    /// Current logic generation's fixed typed lanes. The hot DSP path and
    /// `init` receive this exact bundle; the wrapper-facing route below is
    /// only an off-thread indirection for editor contexts.
    tasks: Option<AnyTaskSpawner>,
    task_route: Option<AnyTaskSpawner>,
    /// Raw pointer to the shell's `Arc<Params>` (type-erased), passed to
    /// every state-op symbol so the plugin shares the shell's params.
    params_ptr: *const (),
    last_modified: SystemTime,
    last_hash: u32,
    /// Set to true to stop the file watcher thread.
    watcher_stop: Arc<AtomicBool>,
    /// Old code + stopped task generations retained until instance teardown.
    /// Their library mappings are then intentionally kept for process
    /// lifetime because editors and state-origin pointers can outlive this
    /// loader.
    retired_generations: Vec<RetiredGeneration>,
    load_counter: u64,
    /// Count of successful library swaps (a `reload` that actually
    /// installed new code). Unlike `load_counter`, a failed reload
    /// attempt does not advance it, so the shell carries state over only
    /// when the code truly changed - a canary-rejected reload stays a
    /// no-op for the live DSP state.
    swap_generation: u64,
    /// Unique ID for this loader instance (used in temp file names).
    instance_id: u64,
}

// SAFETY: NativeLoader is only accessed from one thread at a time.
// The audio thread calls process/reload, the main thread calls render.
// The shell wraps access in a parking_lot::Mutex.
unsafe impl<S: Sample> Send for NativeLoader<S> {}

impl<S: Sample> NativeLoader<S> {
    /// Construct the loader and run the initial load.
    ///
    /// Does not spawn the file watcher - call
    /// [`NativeLoader::spawn_watcher`] after wrapping the loader in an
    /// `Arc<Mutex<...>>` so the watcher thread can drive reloads
    /// itself, off the audio thread.
    #[must_use]
    pub fn new(dylib_path: PathBuf, params_ptr: *const ()) -> Self {
        Self::new_with_tasks(dylib_path, params_ptr, None)
    }

    #[must_use]
    pub fn new_with_tasks(
        dylib_path: PathBuf,
        params_ptr: *const (),
        task_route: Option<AnyTaskSpawner>,
    ) -> Self {
        let mut loader = Self {
            dylib_path,
            library: None,
            symbols: None,
            tasks: None,
            task_route,
            params_ptr,
            last_modified: SystemTime::UNIX_EPOCH,
            last_hash: 0,
            watcher_stop: Arc::new(AtomicBool::new(false)),
            retired_generations: Vec::new(),
            load_counter: 0,
            swap_generation: 0,
            instance_id: LOADER_ID.fetch_add(1, Ordering::Relaxed),
        };
        loader.load();
        loader
    }

    /// Spawn the file-mtime watcher thread.
    ///
    /// The watcher polls the dylib path; when mtime advances and
    /// settles, it acquires `loader` and runs [`NativeLoader::reload`]
    /// directly. This keeps the codesign / dlopen / canary-probe work
    /// off the audio thread - the audio thread only observes
    /// reloads via [`NativeLoader::swap_generation`] advances and carries
    /// its live state into the new code, resetting to match the config.
    ///
    /// Held as a `Weak` so dropping the last `Arc<Mutex<NativeLoader>>`
    /// breaks the watcher's reference and lets the thread exit on its
    /// next stop-flag check.
    pub fn spawn_watcher(loader: &Arc<Mutex<Self>>) {
        let weak = Arc::downgrade(loader);
        let (path, stop) = {
            let guard = loader.lock();
            (guard.dylib_path.clone(), guard.watcher_stop.clone())
        };
        std::thread::Builder::new()
            .name("moose-hot-watcher".into())
            .spawn(move || watch_loop::<S>(&path, &weak, &stop))
            .ok();
    }

    /// Build, verify, and instantiate a fresh dylib at `dylib_path`.
    /// Does not touch `self.library` / `self.plugin`. Caller decides
    /// whether to swap the old state out for the result.
    ///
    /// `new_hash` comes from the caller to avoid re-reading the dylib;
    /// `load` and `reload` already hashed it to detect "unchanged"
    /// before deciding to call us. Re-hashing inside here would double
    /// the per-reload I/O on a 5-20 MB dylib.
    fn build_candidate(&mut self, new_hash: u32) -> Option<Candidate<S>> {
        // Copy to versioned temp path to defeat macOS dyld cache.
        let temp = match self.copy_versioned() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("failed to copy dylib: {e}");
                return None;
            }
        };

        // macOS: ad-hoc codesign (required by SIP). If the temp path
        // is non-UTF-8 (rare - `std::env::temp_dir()` usually lives
        // under a UTF-8 prefix, but the user can override via env)
        // `to_str` fails and codesign would silently no-op against an
        // empty path. The `Library::new` call below then fails on
        // an unsigned dylib with an opaque SIP error, two error
        // sites from the root cause. Log up front so the cause is
        // visible.
        #[cfg(target_os = "macos")]
        if let Some(temp_str) = temp.to_str() {
            let _ = std::process::Command::new("codesign")
                .args(["--sign", "-", "--force", temp_str])
                .output();
        } else {
            log::warn!(
                "codesign skipped: temp dylib path is not valid UTF-8 ({}); \
                 dlopen will likely fail under SIP",
                temp.display()
            );
        }

        let lib = match unsafe { Library::new(&temp) } {
            Ok(l) => l,
            Err(e) => {
                log::warn!("dlopen failed: {e}");
                let _ = std::fs::remove_file(&temp);
                return None;
            }
        };

        // After this point, every early-return drops `lib` (which may
        // close the dylib handle) and we then unlink the temp file so
        // it doesn't accumulate in /tmp across dozens of failed reloads
        // during iterative plugin development.
        let cleanup_temp = |lib: Library, temp: &std::path::Path| {
            drop(lib);
            let _ = std::fs::remove_file(temp);
        };

        // The versioned symbol makes canary-layout evolution safe: the
        // struct returns by value, so a shell must never call a canary
        // of a different shape. A dylib exporting only an older
        // `moose_abi_canary*` fails the lookup and is refused here.
        let canary_fn: Symbol<fn() -> AbiCanary> = match unsafe { lib.get(b"moose_abi_canary_v2") }
        {
            Ok(f) => f,
            Err(e) => {
                log::warn!("missing moose_abi_canary_v2 export (stale pre-2.0 logic dylib?): {e}");
                cleanup_temp(lib, &temp);
                return None;
            }
        };
        let dylib_canary = canary_fn();
        let shell_canary = AbiCanary::current::<S>();
        if !shell_canary.matches(&dylib_canary) {
            log::error!(
                "ABI mismatch - rebuild both shell and logic:\n{}",
                shell_canary.diff_report(&dylib_canary)
            );
            cleanup_temp(lib, &temp);
            return None;
        }

        // Resolve the flat-ABI symbol table. Symbol presence (plus the
        // canary above) replaces the old vtable probe: a mismatched or
        // stale dylib is missing these exports and is refused here.
        // SAFETY: the canary matched, so the exports have the signatures
        // `LogicSymbols::resolve` names.
        let Some(symbols) = (unsafe { LogicSymbols::<S>::resolve(&lib) }) else {
            cleanup_temp(lib, &temp);
            return None;
        };

        let tasks = (symbols.build_tasks)(self.params_ptr);
        Some(Candidate {
            tasks,
            symbols,
            library: lib,
            hash: new_hash,
            mtime: file_mtime(&self.dylib_path),
            temp_path: temp,
        })
    }

    /// Initial load. Called from `new()`.
    fn load(&mut self) -> bool {
        let Some(new_hash) = crc32_file(&self.dylib_path) else {
            log::warn!(
                "failed to hash dylib at {} (missing / unreadable / mid-write); skipping load",
                self.dylib_path.display()
            );
            return false;
        };
        if new_hash == self.last_hash && self.library.is_some() {
            log::debug!("dylib unchanged (CRC32 match), skipping reload");
            return true;
        }
        match self.build_candidate(new_hash) {
            Some(cand) => {
                // Host/main thread: warm the pool compiled into this exact
                // logic dylib before `init` or `process` can schedule work.
                // Warming the shell's separate moose-core copy would leave
                // this generation's first audio-thread schedule cold.
                if !(cand.symbols.warm_tasks)() {
                    log::warn!("hot-reload task-pool startup failed; refusing logic generation");
                    discard_candidate(cand);
                    return false;
                }
                self.install_tasks(cand.tasks);
                self.library = Some(cand.library);
                self.symbols = Some(cand.symbols);
                self.last_hash = cand.hash;
                self.last_modified = cand.mtime;
                log::info!("loaded plugin dylib: {}", self.dylib_path.display());
                true
            }
            None => false,
        }
    }

    /// Reload the dylib. Verifies the *new* dylib first; only swaps the
    /// symbol table after the candidate is fully constructed, so a failed
    /// canary or missing symbol leaves the host on the previous code
    /// instead of silence.
    ///
    /// State is not touched here: the shell owns it and, on seeing the
    /// [`swap_generation`](Self::swap_generation) advance, carries it over
    /// into the new code via a save / load round-trip (when
    /// [`preserve_dsp_state`](Self::preserve_dsp_state) is set) or re-inits.
    pub fn reload(&mut self) -> bool {
        let Some(new_hash) = crc32_file(&self.dylib_path) else {
            log::warn!(
                "failed to hash dylib at {} (missing / unreadable / mid-write); keeping previous code loaded",
                self.dylib_path.display()
            );
            return false;
        };
        if new_hash == self.last_hash && self.library.is_some() {
            log::debug!("dylib unchanged (CRC32 match), skipping reload");
            return true;
        }

        // Build + verify the candidate while the old code is still live.
        let Some(candidate) = self.build_candidate(new_hash) else {
            log::warn!("hot-reload failed; keeping previous code loaded");
            return false;
        };

        // Warm the candidate's own module-local pool before touching the live
        // generation. A failed spawn can then reject the candidate without a
        // gap in old task service or any audio-thread startup work.
        if !(candidate.symbols.warm_tasks)() {
            log::warn!("hot-reload task-pool startup failed; keeping previous code loaded");
            discard_candidate(candidate);
            return false;
        }

        // Close old admission before changing the route or symbols. The lane
        // gate and pool execution gate are separate linearization points: the
        // first drains scheduling critical sections, the second prevents a
        // worker from entering after the active-handler count reaches zero.
        if let Some(tasks) = &self.tasks
            && !tasks.retire(TASK_RETIRE_WAIT)
        {
            log::warn!(
                "hot-reload task retirement exceeded {TASK_RETIRE_WAIT:?}; \
                 keeping the previous logic generation active"
            );
            discard_candidate(candidate);
            return false;
        }

        if let Some(symbols) = &self.symbols
            && !(symbols.quiesce_tasks)(TASK_RETIRE_WAIT)
        {
            if let Some(tasks) = &self.tasks {
                tasks.resume();
            }
            log::warn!(
                "hot-reload task handler exceeded {TASK_RETIRE_WAIT:?}; \
                 keeping the previous logic generation active"
            );
            discard_candidate(candidate);
            return false;
        }

        // No old producer can inject and no old handler can enter. Join its
        // workers before switching generations, then discard queued work
        // while the old code is still mapped.
        if let Some(symbols) = &self.symbols {
            (symbols.shutdown_tasks)();
        }
        if let Some(tasks) = &self.tasks {
            tasks.close();
        }

        // Every successfully activated generation remains mapped for process
        // lifetime because editor objects and state-origin function pointers
        // may outlive the loader fields. Its worker threads are already gone.
        let old_tasks = self.tasks.take();
        if let Some(old) = self.library.take() {
            self.retired_generations.push(RetiredGeneration {
                tasks: old_tasks,
                library: old,
            });
        }

        self.install_tasks(candidate.tasks);
        self.library = Some(candidate.library);
        self.symbols = Some(candidate.symbols);
        self.last_hash = candidate.hash;
        self.last_modified = candidate.mtime;
        self.swap_generation += 1;

        log::info!(
            "hot-reload complete (load #{}, {} retired generations)",
            self.load_counter,
            self.retired_generations.len()
        );
        true
    }

    /// Whether the currently loaded dylib opts into DSP-state carry-over
    /// across a reload. `false` when nothing is loaded.
    #[must_use]
    pub fn preserve_dsp_state(&self) -> bool {
        self.symbols.as_ref().is_some_and(|s| s.preserve)
    }

    /// Allocate fresh DSP state from the current dylib. Returns the
    /// opaque state pointer and the origin dylib's `StateOrigin` - the
    /// `drop` / `save` fns the shell keeps to free or serialize that exact
    /// allocation, since they live in this dylib's code.
    #[must_use]
    pub fn init_state(&self) -> Option<(*mut (), StateOrigin)> {
        let s = self.symbols.as_ref()?;
        Some((
            (s.init_state)(self.params_ptr, self.tasks.clone()),
            StateOrigin {
                drop: s.drop_state,
                save: s.save_state,
            },
        ))
    }

    #[must_use]
    pub fn task_spawner(&self) -> Option<&AnyTaskSpawner> {
        self.tasks.as_ref()
    }

    fn install_tasks(&mut self, tasks: Option<AnyTaskSpawner>) {
        if let (Some(route), Some(tasks)) = (&self.task_route, &tasks) {
            let _ = route.replace_with(tasks);
        } else if let Some(route) = &self.task_route {
            let _ = route.clear_route();
        }
        self.tasks = tasks;
    }

    /// Run the current dylib's `process` on `state` (opaque, layout must
    /// match the current fingerprint). Returns `Normal` if nothing is
    /// loaded (silent block).
    pub fn process(
        &self,
        state: *mut (),
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        match self.symbols.as_ref() {
            Some(s) => (s.process)(state, self.params_ptr, buffer, events, ctx),
            None => ProcessStatus::Normal,
        }
    }

    /// Run the current dylib's `reset` on `state`.
    pub fn reset(&self, state: *mut (), config: &AudioConfig) {
        if let Some(s) = self.symbols.as_ref() {
            (s.reset)(state, self.params_ptr, config);
        }
    }

    #[must_use]
    pub fn latency(&self, state: *const ()) -> u32 {
        self.symbols.as_ref().map_or(0, |s| (s.latency)(state))
    }

    #[must_use]
    pub fn tail(&self, state: *const ()) -> u32 {
        self.symbols.as_ref().map_or(0, |s| (s.tail)(state))
    }

    #[must_use]
    pub fn save_state(&self, state: *const ()) -> Vec<u8> {
        self.symbols
            .as_ref()
            .map_or_else(Vec::new, |s| (s.save_state)(state))
    }

    pub fn snapshot_into(&self, state: *const (), buf: &mut Vec<u8>) -> bool {
        self.symbols
            .as_ref()
            .is_some_and(|s| (s.snapshot_into)(state, buf))
    }

    /// Snapshot generation token for the loaded logic, or `None` when no
    /// dylib is loaded or the plugin doesn't version its snapshot (in
    /// which case the shell re-serializes every block, as before).
    #[must_use]
    pub fn snapshot_version(&self, state: *const ()) -> Option<u64> {
        self.symbols
            .as_ref()
            .and_then(|s| (s.snapshot_version)(state))
    }

    /// Restore `state` from `data`, then fire `state_changed` in the same
    /// window (matching the format-wrapper bridges' policy).
    ///
    /// # Errors
    /// Forwards the dylib's `load_state` failure (malformed / stale blob).
    pub fn load_state(&self, state: *mut (), data: &[u8]) -> Result<(), StateLoadError> {
        match self.symbols.as_ref() {
            Some(s) => {
                let r = (s.load_state)(state, data);
                (s.state_changed)(state, self.params_ptr);
                r
            }
            None => Ok(()),
        }
    }

    /// Build the loaded plugin's editor and return the current fixed task
    /// bundle with it. Both are read from one loader generation under the
    /// caller's lock, so the editor can never be paired with a route that a
    /// concurrent reload already advanced. Receiverless by design: this does
    /// not borrow the logic instance whose `&mut` the audio thread owns.
    /// `None` when no library is loaded or the editor symbol is missing.
    #[must_use]
    pub fn build_editor(
        &self,
        params_ptr: *const (),
    ) -> Option<(Box<dyn moose_core::editor::Editor>, Option<AnyTaskSpawner>)> {
        type BuildEditorFn = fn(*const ()) -> Box<dyn moose_core::editor::Editor>;
        let library = self.library.as_ref()?;
        // SAFETY: `export_plugin!` fixes this symbol's signature, and the
        // ABI canary already verified this dylib matches the shell
        // before the library was bound.
        let build: Symbol<BuildEditorFn> = unsafe { library.get(b"moose_build_editor").ok()? };
        Some((build(params_ptr), self.tasks.clone()))
    }

    /// Whether a dylib is currently loaded (symbols resolved).
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        self.symbols.is_some()
    }

    /// Monotonic counter of reload *attempts*: bumps once per
    /// `copy_versioned()` invocation, which precedes every candidate
    /// build (successful or not). Used for unique temp-file names.
    #[must_use]
    pub fn load_counter(&self) -> u64 {
        self.load_counter
    }

    /// Monotonic count of successful library swaps. A failed reload
    /// (canary mismatch, missing symbol) does not advance it, so a
    /// consumer sharing this `NativeLoader` detects a genuine code swap -
    /// and only then carries live state into the new code - without
    /// reacting to attempts that left the old code in place.
    #[must_use]
    pub fn swap_generation(&self) -> u64 {
        self.swap_generation
    }

    fn copy_versioned(&mut self) -> Result<PathBuf, std::io::Error> {
        self.load_counter += 1;
        let ext = self
            .dylib_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("dylib");
        let stem = self
            .dylib_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin");
        let temp = std::env::temp_dir().join(format!(
            "moose-hot-{stem}-{}-{}.{ext}",
            self.instance_id, self.load_counter
        ));
        std::fs::copy(&self.dylib_path, &temp)?;
        Ok(temp)
    }
}

impl<S: Sample> Drop for NativeLoader<S> {
    fn drop(&mut self) {
        self.watcher_stop.store(true, Ordering::Relaxed);
        if let Some(route) = &self.task_route {
            let _ = route.clear_route();
        }
        if let Some(tasks) = &self.tasks {
            tasks.close();
        }
        if let Some(symbols) = &self.symbols {
            (symbols.shutdown_tasks)();
        }
        // The loader owns only the resolved symbol table (bare fn
        // pointers, nothing to drop); the DSP state is owned and freed
        // by the shell. Drop the symbols before the library, matching
        // library-outlives-its-code ordering.
        self.symbols = None;
        if let Some(library) = self.library.take() {
            let generation = RetiredGeneration {
                tasks: self.tasks.take(),
                library,
            };
            retain_generation_mapping(generation);
        }
        for generation in self.retired_generations.drain(..) {
            retain_generation_mapping(generation);
        }
    }
}

fn retain_generation_mapping(generation: RetiredGeneration) {
    let RetiredGeneration { tasks, library } = generation;
    // Drop loader-owned task handles while their code is still mapped. Other
    // handles may remain in editors or host wrappers; leaking the Library
    // handle below keeps their closures/vtables valid for process lifetime.
    drop(tasks);
    std::mem::forget(library);
}

fn discard_candidate<S: Sample>(candidate: Candidate<S>) {
    let Candidate {
        tasks,
        symbols,
        library,
        hash: _,
        mtime: _,
        temp_path,
    } = candidate;
    // The candidate was never activated, so no producer can reach its lanes.
    // Stop any pool warmed during candidate validation before unloading code.
    (symbols.shutdown_tasks)();
    drop(tasks);
    drop(library);
    let _ = std::fs::remove_file(temp_path);
}

/// File watcher loop. Polls mtime ~every 500ms, but checks the stop
/// flag every 50ms so dropping the loader doesn't block waiting for
/// the next poll cycle.
///
/// On a stable mtime advance, takes the loader lock with
/// `try_lock_for` and runs `reload()` directly. Earlier shapes only
/// set a `reload_pending` flag and let the audio thread call
/// `reload()` itself, which spawned `codesign` and dlopen on the
/// audio thread. Driving reload here keeps that work off the audio
/// path entirely.
fn watch_loop<S: Sample>(
    path: &std::path::Path,
    loader: &Weak<Mutex<NativeLoader<S>>>,
    stop: &AtomicBool,
) {
    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    const STOP_CHECK: Duration = Duration::from_millis(50);
    const SETTLE: Duration = Duration::from_millis(200);
    /// How long to wait for the audio thread to release the loader
    /// mutex before giving up and retrying on the next poll. Short
    /// enough that a stuck audio thread doesn't pin the watcher; long
    /// enough to cover a single `process()` call (typically ≪ 50 ms).
    const LOCK_WAIT: Duration = Duration::from_millis(50);
    // Both constants are sub-second; the u128 → u32 cast is bounded.
    #[allow(clippy::cast_possible_truncation)]
    let chunks = (POLL_INTERVAL.as_millis() / STOP_CHECK.as_millis()) as u32;
    #[allow(clippy::cast_possible_truncation)]
    let settle_chunks = (SETTLE.as_millis() / STOP_CHECK.as_millis()) as u32;

    let mut last_mtime = file_mtime(path);
    while !stop.load(Ordering::Relaxed) {
        for _ in 0..chunks {
            std::thread::sleep(STOP_CHECK);
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
        let mtime = file_mtime(path);
        if mtime <= last_mtime {
            continue;
        }
        // Wait for the compiler to finish writing - broken into
        // STOP_CHECK chunks so dropping the loader during the settle
        // window doesn't block for the full SETTLE duration.
        for _ in 0..settle_chunks {
            std::thread::sleep(STOP_CHECK);
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
        last_mtime = file_mtime(path);

        let Some(loader) = loader.upgrade() else {
            return;
        };
        let Some(mut guard) = loader.try_lock_for(LOCK_WAIT) else {
            // Audio thread holds the lock; try again on the next poll.
            continue;
        };
        guard.reload();
    }
}

fn file_mtime(path: &std::path::Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Streaming CRC32 fingerprint of `path`'s contents.
///
/// Reads through an 8 KiB buffer so a 5–20 MB dylib polled every
/// 500 ms doesn't allocate its full contents per poll cycle.
///
/// Returns `None` on `open` / `read` failure (file missing,
/// permissions, mid-write interruption - the compiler's mid-write
/// window is the common case). An empty file successfully hashes
/// to `Some(0)` - distinct from the I/O failure case so a caller
/// can't conflate "unreadable" with "unchanged" against an initial
/// `last_hash = 0`.
fn crc32_file(path: &std::path::Path) -> Option<u32> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            // Partial-read interruption / I/O failure - return None and
            // let the caller retry on the next poll. The compiler's
            // mid-write window is the common case here.
            Err(_) => return None,
        }
    }
    Some(hasher.finalize())
}
