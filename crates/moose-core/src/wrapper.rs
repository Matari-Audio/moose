//! Helpers shared across format wrappers (CLAP, VST3, AU, standalone).
//!
//! Each wrapper still owns its format-specific descriptor types and
//! callback tables; those don't unify cleanly. What unifies is the
//! "boring" boundary glue: building `CStrings` from `ParamInfo`
//! fields, picking the default bus layout, and resolving install-time
//! name overrides.
//!
//! Each helper is a single small function so the wrappers stay
//! greppable - the per-format vtable construction code reads as
//! "for each param, get cstrings, build descriptor" without inlined
//! `CString::new(...).unwrap_or_default()` boilerplate.
//!
//! Adding a new format wrapper? Reach for these first; only fall back
//! to direct `CString::new` etc. when the format genuinely needs
//! something none of the other formats does.

use std::any::type_name;
use std::ffi::CString;
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use moose_params::ParamInfo;

use crate::export::PluginExport;

pub use plugin_cell::{PluginCell, PluginGuard};

/// The ownership cell the real-time format wrappers (CLAP, VST3, AU) put
/// around their plugin instance. The audio thread owns the
/// plugin while the host is processing (`process`, the queued state
/// apply); the host thread owns it while processing is stopped (`init`,
/// `reset`, an inactive state load). The host contract makes those two
/// mutually exclusive in time - a spec-compliant host never overlaps
/// `process` with a lifecycle callback - so [`PluginCell`] holds no OS
/// lock and the audio thread never waits. Ownership handoff carries a
/// release-acquire edge (each owner observes the previous owner's
/// writes), not mutual exclusion.
///
/// A host state save no longer touches the plugin at all: it reads the
/// lock-free [`SnapshotSlot`](crate::snapshot::SnapshotSlot) the audio
/// thread publishes each block (see [`save_extra`]). Meters ride the
/// lock-free `MeterStore`, and params are atomic. So nothing on a
/// non-audio thread contends with `process` on the hot path.
///
/// The soundness rests on the host's process/lifecycle exclusion
/// contract; a debug-build overlap detector trips if a host ever
/// violates it. The `Arc` is what makes GUI closures sound: they clone
/// the handle instead of stashing a raw pointer into the instance
/// struct.
pub type SharedPlugin<P> = Arc<PluginCell<P>>;

/// Wrap a freshly created plugin in the wrapper-standard ownership
/// cell. See [`SharedPlugin`].
pub fn shared_plugin<P>(plugin: P) -> SharedPlugin<P> {
    Arc::new(PluginCell::new(plugin))
}

/// Take ownership of the plugin for the current callback. Never blocks:
/// the audio thread owns the plugin while active, the host thread while
/// inactive, and the host contract keeps the two from overlapping, so
/// there is nothing to wait on. The returned guard's `&mut` is exclusive
/// by that contract; the `Acquire` inside observes the previous owner's
/// writes.
pub fn enter_plugin<P>(plugin: &PluginCell<P>) -> PluginGuard<'_, P> {
    plugin.enter()
}

/// Read the plugin's custom-state blob for a host state save.
///
/// Reads the lock-free [`SnapshotSlot`](crate::snapshot::SnapshotSlot)
/// the audio thread publishes each block, and never takes the plugin
/// lock: a host save must not stall the audio thread, which holds that
/// lock for the whole block. A plugin with custom state publishes it
/// through `snapshot_into`; one that publishes nothing has no custom
/// state to save, so an empty blob is correct. The wrapper republishes
/// the snapshot after any state change applied outside `process` (an
/// inactive load), so an inactive save still returns live state. Params
/// are serialized separately (also lock-free).
#[must_use]
pub fn save_extra(snapshot: &crate::snapshot::SnapshotSlot) -> Vec<u8> {
    snapshot.read().unwrap_or_default()
}

/// How long a host-thread save or load waits for the audio thread to
/// serve it at the next block boundary before falling back. Long enough
/// for any real block period; a host that stopped calling `process`
/// without telling us (a sleeping CLAP plugin) costs this much per call.
// ponytail: fixed bound, derive from the block period if 50 ms stalls show up.
pub const AUDIO_HANDSHAKE_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Pop and apply one queued state load on the thread that owns the plugin,
/// record the result for a waiting host thread, and ask the shell for a
/// fresh snapshot at the end of the next `process`. `None` when nothing was
/// queued. Wrappers call this at the top of `process`, outside the
/// real-time section (`load_state` may allocate).
pub fn apply_queued_state<P: PluginExport>(
    plugin: &mut P,
    queue: &crate::state::StateLoadQueue,
) -> Option<Result<(), crate::state::StateLoadError>> {
    let state = queue.pop()?;
    let result = crate::state::try_apply_state(plugin, &state);
    queue.mark_applied(&state, result.is_ok());
    plugin.snapshot_slot().request_fresh();
    Some(result)
}

/// Load a host state into the plugin. With `can_own` (the wrapper is not
/// processing) the host thread takes the plugin and applies it now;
/// otherwise the state is queued for the audio thread and this waits up to
/// [`AUDIO_HANDSHAKE_WAIT`] for it to land and for the fresh snapshot, so an
/// editor notified next reads the new state.
///
/// Params and `#[persist]` must already be applied (`read_and_apply_params`
/// / `apply_params`); `backup` is what they were before. When the plugin's
/// `load_state` rejects the state, they are restored from it, so the load
/// fails as a unit and the error is returned for the host. The plugin's
/// [`validate_state`](crate::plugin::PluginRuntime::validate_state) runs
/// first, on this thread, and can reject the state up front. A load still
/// queued when the wait ends returns `Ok`: its envelope already parsed on
/// the host thread, and saves until it lands report its custom bytes (see
/// [`custom_state_view`]).
///
/// # Errors
///
/// The plugin's `load_state` error, when it ran before this returned.
pub fn apply_or_queue_state<P: PluginExport>(
    plugin: &PluginCell<P>,
    params: &P::Params,
    slot: &crate::snapshot::SnapshotSlot,
    queue: &crate::state::StateLoadQueue,
    state: crate::state::DeserializedState,
    backup: &crate::state::ParamsBackup,
    can_own: bool,
) -> Result<(), crate::state::StateLoadError> {
    let result = apply_or_queue_inner(plugin, slot, queue, state, can_own);
    if result.is_err() {
        backup.restore(params);
    }
    result
}

fn apply_or_queue_inner<P: PluginExport>(
    plugin: &PluginCell<P>,
    slot: &crate::snapshot::SnapshotSlot,
    queue: &crate::state::StateLoadQueue,
    state: crate::state::DeserializedState,
    can_own: bool,
) -> Result<(), crate::state::StateLoadError> {
    // Host thread, before anything is applied or queued: a queued load may
    // answer the host before `load_state` runs, so this is where it can fail.
    if let Some(extra) = &state.extra {
        P::validate_state(extra)?;
    }
    // `try_enter`: a CLAP `reset` may hold the cell on the audio thread
    // while stopped; then queue instead of panicking on an overlap.
    if can_own && let Some(mut instance) = plugin.try_enter() {
        // An older queued load is superseded by this one.
        if let Some(stale) = queue.pop() {
            queue.mark_applied(&stale, true);
        }
        let result = crate::state::try_apply_state(&mut *instance, &state);
        instance.republish_snapshot();
        return result;
    }
    let handle = queue.push_tracked(state);
    match queue.wait_applied(&handle, AUDIO_HANDSHAKE_WAIT) {
        Some(false) => Err(crate::state::StateLoadError::Other(
            "plugin load_state rejected the state".into(),
        )),
        // Applied: the audio thread requested a fresh snapshot right after
        // the apply, served at the end of that block. Wait for it so a
        // `get_state` right after this doesn't read the pre-load snapshot.
        Some(true) => {
            let ticket = slot.request_fresh();
            // A late snapshot only delays freshness; the load itself landed.
            let _ = slot.wait_served(ticket, AUDIO_HANDSHAKE_WAIT);
            Ok(())
        }
        None => Ok(()),
    }
}

/// Custom state for a host save. With `can_own` the host thread takes the
/// plugin, applies any queued load, and serializes live state. Otherwise it
/// asks the audio thread for a fresh snapshot and waits up to
/// [`AUDIO_HANDSHAKE_WAIT`]; if no block runs in time it falls back to
/// [`custom_state_view`].
///
/// Why a request instead of a lock: the audio thread never waits, and a
/// running audio thread serves the request within one block, so the host
/// gets live state without a dropout. Serializing on the audio thread only
/// happens on request, not every block.
pub fn custom_state_for_save<P: PluginExport>(
    plugin: &PluginCell<P>,
    slot: &crate::snapshot::SnapshotSlot,
    queue: &crate::state::StateLoadQueue,
    can_own: bool,
) -> Vec<u8> {
    if can_own && let Some(mut instance) = plugin.try_enter() {
        if apply_queued_state(&mut *instance, queue).is_some() {
            instance.republish_snapshot();
        }
        return crate::export::read_custom_state_offthread(&*instance);
    }
    let ticket = slot.request_fresh();
    if slot.wait_served(ticket, AUDIO_HANDSHAKE_WAIT) && queue.unapplied().is_none() {
        return save_extra(slot);
    }
    custom_state_view(slot, queue)
}

/// Custom state without touching the plugin: the bytes of a load still
/// waiting for the audio thread, else the last published snapshot. For
/// the editor's `get_state` and as the save fallback.
#[must_use]
pub fn custom_state_view(
    slot: &crate::snapshot::SnapshotSlot,
    queue: &crate::state::StateLoadQueue,
) -> Vec<u8> {
    match queue.unapplied() {
        Some(state) => state.extra.clone().unwrap_or_default(),
        None => save_extra(slot),
    }
}

/// Plugin ownership cell, uniform across platforms: the audio thread owns
/// the plugin while processing, the host thread while stopped. Ownership
/// is one atomic flag plus a release-acquire handoff, so an uncontended
/// `enter` takes no OS lock. A spec-compliant host never overlaps the two
/// owners; when one does, `enter` parks until the other releases rather
/// than dropping the call (audit finding 8).
mod plugin_cell {
    use std::cell::UnsafeCell;
    use std::marker::PhantomData;
    use std::ops::{Deref, DerefMut};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread::{self, Thread};
    use std::time::Duration;

    /// Longest a waiting [`PluginCell::enter`] parks before it re-checks the
    /// cell on its own. The releasing owner unparks the waiter, so this only
    /// bounds a wake-up lost to a `try_lock` race on the waker slot.
    const WAKE_BACKSTOP: Duration = Duration::from_millis(2);

    pub struct PluginCell<T> {
        data: UnsafeCell<T>,
        /// Release-acquire handoff counter. Each owner `Acquire`s on
        /// entry (observing the previous owner's writes) and `Release`s
        /// on exit (publishing its own), carrying the happens-before edge
        /// between the audio thread and the host thread.
        handoff: AtomicU64,
        /// Ownership flag; the only arbiter of who holds the cell.
        held: AtomicBool,
        /// Token of the thread holding the cell (0 when free), so `enter`
        /// can tell same-thread re-entry (a bug that would deadlock) from
        /// another thread's overlap (worth waiting out).
        owner: AtomicU64,
        /// A thread is parked in `enter` waiting for the cell.
        waiting: AtomicBool,
        /// The parked thread to wake. Written by waiters; the releasing
        /// owner only `try_lock`s it, so release never blocks.
        waker: Mutex<Option<Thread>>,
    }

    /// A never-zero per-thread token. `0` is reserved for "cell free", so
    /// the counter starts at 1.
    fn thread_token() -> u64 {
        thread_local! {
            static TOKEN: u64 = {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                NEXT.fetch_add(1, Ordering::Relaxed)
            };
        }
        TOKEN.with(|&t| t)
    }

    // SAFETY: `T` is reached only through a guard, and `held` admits one
    // guard at a time - the same guarantee `Mutex<T>` gives, so
    // `Send`/`Sync` need only `T: Send`.
    unsafe impl<T: Send> Send for PluginCell<T> {}
    unsafe impl<T: Send> Sync for PluginCell<T> {}

    impl<T> PluginCell<T> {
        pub fn new(value: T) -> Self {
            Self {
                data: UnsafeCell::new(value),
                handoff: AtomicU64::new(0),
                held: AtomicBool::new(false),
                owner: AtomicU64::new(0),
                waiting: AtomicBool::new(false),
                waker: Mutex::new(None),
            }
        }

        /// Take ownership, waiting while another thread holds the cell.
        ///
        /// A spec-compliant host never overlaps the audio thread and a
        /// lifecycle callback, so this normally returns at once. When a
        /// host does overlap them, the caller parks until the other owner
        /// releases (one block or one callback) instead of failing the
        /// call. The wait is event-driven: the releasing owner unparks it.
        ///
        /// # Panics
        /// Panics when this thread already holds the cell: author code
        /// (e.g. `request_resize` from `set_size` / `state_changed`) reached
        /// back into it while the wrapper held it, which can never be
        /// waited out. Use [`Self::try_enter`] on such paths.
        pub fn enter(&self) -> PluginGuard<'_, T> {
            let me = thread_token();
            loop {
                if let Some(guard) = self.try_enter() {
                    return guard;
                }
                assert_ne!(
                    self.owner.load(Ordering::Relaxed),
                    me,
                    "plugin ownership cell re-entered on the same thread: author \
                     editor code (e.g. request_resize called from set_size / \
                     state_changed) reached back into the cell while the wrapper \
                     still held it"
                );
                self.park_until_released();
            }
        }

        /// Park this thread until the current owner releases (or the
        /// backstop elapses). Spurious returns are fine: `enter` re-checks.
        fn park_until_released(&self) {
            *self.waker.lock().expect("plugin cell waker slot poisoned") = Some(thread::current());
            // SeqCst pairs with the guard's drop: either the owner sees
            // `waiting` after clearing `held`, or this load sees `held` clear.
            self.waiting.store(true, Ordering::SeqCst);
            if self.held.load(Ordering::SeqCst) {
                thread::park_timeout(WAKE_BACKSTOP);
            }
            self.waiting.store(false, Ordering::Relaxed);
        }

        /// Take ownership only if the cell is free, returning `None` when it
        /// is already held. Never waits and never asserts: the primitive for
        /// paths where author code may re-enter the cell, and for the audio
        /// thread's lock-free checks. Callers defer the work (stash it and
        /// apply on the next entry) when they get `None`.
        pub fn try_enter(&self) -> Option<PluginGuard<'_, T>> {
            self.handoff.load(Ordering::Acquire);
            if self
                .held
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                return None;
            }
            self.owner.store(thread_token(), Ordering::Relaxed);
            Some(PluginGuard {
                cell: self,
                _not_send: PhantomData,
            })
        }

        /// Wake a thread parked in [`Self::enter`], if any. Never blocks:
        /// a waiter still registering is caught by its own `held` re-check.
        fn wake_waiter(&self) {
            if self.waiting.load(Ordering::SeqCst)
                && let Ok(slot) = self.waker.try_lock()
                && let Some(waiter) = slot.as_ref()
            {
                waiter.unpark();
            }
        }
    }

    /// Guard handing out the exclusive `&mut T`; releases the handoff on
    /// drop so the next owner's `Acquire` sees this owner's writes.
    pub struct PluginGuard<'a, T> {
        cell: &'a PluginCell<T>,
        /// The acquiring thread must also release, for the handoff edge
        /// to mean anything - so the guard can't cross threads.
        _not_send: PhantomData<*const ()>,
    }

    impl<T> Deref for PluginGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            // SAFETY: `held` admits one guard at a time, so no other
            // reference to `data` exists while this guard lives.
            unsafe { &*self.cell.data.get() }
        }
    }

    impl<T> DerefMut for PluginGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: as in `deref` - sole owner, so this `&mut` is unique.
            unsafe { &mut *self.cell.data.get() }
        }
    }

    impl<T> Drop for PluginGuard<'_, T> {
        fn drop(&mut self) {
            self.cell.owner.store(0, Ordering::Relaxed);
            // Release: publish this owner's writes to the next `Acquire`.
            self.cell.handoff.fetch_add(1, Ordering::Release);
            self.cell.held.store(false, Ordering::SeqCst);
            self.cell.wake_waiter();
        }
    }
}

/// `CStrings` derived from a single `ParamInfo`. All four conversions
/// follow the same pattern (`unwrap_or_default()` so a `\0` in metadata
/// degrades to an empty C string instead of panicking the host); pulling
/// them into one struct keeps the per-format vtable loops uniform.
pub struct ParamCStrings {
    pub name: CString,
    pub short_name: CString,
    pub unit: CString,
    pub group: CString,
}

impl ParamCStrings {
    /// Build all four `CStrings` for one parameter.
    #[must_use]
    pub fn from_info(info: &ParamInfo) -> Self {
        Self {
            name: CString::new(info.name).unwrap_or_default(),
            short_name: CString::new(info.short_name).unwrap_or_default(),
            unit: CString::new(info.unit.as_str()).unwrap_or_default(),
            group: CString::new(info.group).unwrap_or_default(),
        }
    }
}

/// `(input_channels, output_channels)` for the plugin's default bus
/// layout, or `None` when the plugin declares no layouts.
/// Used by every format's vtable / descriptor to advertise channel
/// counts at registration time.
///
/// MIDI-only plugins declare `bus_layouts: [BusLayout::new()]` (zero
/// input and output channels), for which this returns `Some((0, 0))`.
///
/// `None` indicates a plugin-author bug: zero-bus plugins must return
/// `vec![BusLayout::new()]` explicitly. Callers should log a
/// diagnostic and skip registration (see how each `register_*` entry
/// point handles this) rather than substitute a silent default that
/// would misreport channel counts to the host.
#[must_use]
pub fn default_io_channels<P: PluginExport>() -> Option<(u32, u32)> {
    P::bus_layouts()
        .first()
        .map(|l| (l.total_input_channels(), l.total_output_channels()))
}

/// Find the `bus_layouts()` index whose total input/output channel counts
/// match `(inputs, outputs)`. Wrappers that negotiate a layout from a
/// host-proposed arrangement (VST3 `setBusArrangements`, AU channel-config
/// selection, the standalone device match) use
/// this to map a request onto a supported layout. `None` when nothing
/// matches; the caller then rejects the arrangement or falls back to the
/// first layout.
#[must_use]
pub fn find_bus_layout<P: PluginExport>(inputs: u32, outputs: u32) -> Option<usize> {
    P::bus_layouts()
        .iter()
        .position(|l| l.total_input_channels() == inputs && l.total_output_channels() == outputs)
}

/// Standard diagnostic emitted by `register_*` when
/// [`default_io_channels`] returns `None`. Centralised so every
/// wrapper prints the same actionable message.
pub fn log_missing_bus_layout<P: PluginExport>(format: &str) {
    eprintln!(
        "[moose {format}] {}::bus_layouts() returned an empty list - \
         plugin will not register. Plugins with no audio I/O (e.g. \
         MIDI effects) should return vec![BusLayout::new()] \
         explicitly.",
        type_name::<P>(),
    );
}

/// Run a `register_*` body under [`std::panic::catch_unwind`].
///
/// Format wrappers' `register_*` entry points run during plugin
/// registration - some from `extern "C" fn init` static
/// initializers (`.init_array` / `__mod_init_func` / `.CRT$XCU`),
/// others lazily on the first host query (AAX, to keep the Windows
/// loader-lock window empty during Pro Tools' scan). A panic that
/// escapes them crosses an `extern "C"`
/// boundary and aborts the host process - a `panic = "abort"`
/// configuration would do the same. Catching the unwind here turns
/// any panic during registration into a logged diagnostic plus
/// "host sees no plugin," which is the same outcome a plugin author
/// would expect from a missing `bus_layouts` declaration.
///
/// `AssertUnwindSafe` is applied internally - the panic is treated
/// as fatal-for-this-plugin, so leaving an `Arc` ref-count or
/// `OnceLock` half-set is acceptable: the host won't load the
/// plugin and the process will exit shortly after registration
/// finishes anyway.
pub fn run_register<P>(format: &str, body: impl FnOnce()) {
    let result = catch_unwind(AssertUnwindSafe(body));
    if let Err(payload) = result {
        eprintln!(
            "[moose {format}] panic during register for {}: {}",
            type_name::<P>(),
            extract_panic_msg(&payload),
        );
    }
}

/// Bytes kept of a `process` panic message; the slot never grows.
const PANIC_MESSAGE_CAP: usize = 256;

/// Per-instance "process panicked" latch (audit finding 5). After the
/// first caught panic the wrapper stops calling the plugin, outputs
/// silence and reports the error status once; the next successful
/// prepare / activate clears it. Creating a latch installs a chained panic
/// hook that keeps `process` panics off stderr. The message goes into a preallocated
/// slot on the audio thread and is printed from a host thread
/// ([`Self::report`], [`Self::clear`], or drop).
pub struct PanicLatch {
    failed: std::sync::atomic::AtomicBool,
    message: std::sync::Mutex<String>,
}

impl Default for PanicLatch {
    fn default() -> Self {
        Self::new()
    }
}

impl PanicLatch {
    #[must_use]
    pub fn new() -> Self {
        quiet_panic_hook::install();
        Self {
            failed: std::sync::atomic::AtomicBool::new(false),
            message: std::sync::Mutex::new(String::with_capacity(PANIC_MESSAGE_CAP)),
        }
    }

    /// The plugin panicked in `process` since the last prepare.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Audio thread: latch and keep the message without allocating
    /// (`try_lock`, then truncated to the slot's capacity).
    fn record(&self, parts: &[&str]) {
        if let Ok(mut slot) = self.message.try_lock() {
            slot.clear();
            for part in parts {
                let room = slot.capacity() - slot.len();
                let mut end = part.len().min(room);
                while !part.is_char_boundary(end) {
                    end -= 1;
                }
                slot.push_str(&part[..end]);
            }
        }
        self.failed.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Host thread: print a recorded message, once.
    pub fn report(&self) {
        let mut slot = self
            .message
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !slot.is_empty() {
            eprintln!("{slot}");
            slot.clear();
        }
    }

    /// Host thread, after a successful prepare / activate: report any
    /// pending message and let `process` call the plugin again.
    pub fn clear(&self) {
        self.report();
        self.failed.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl Drop for PanicLatch {
    fn drop(&mut self) {
        self.report();
    }
}

/// [`run_audio_block_with`] with a [`PanicLatch`]: `None` when `body`
/// panicked (the latch is now set and holds the message; nothing is
/// printed on the audio thread). Wrappers check
/// [`PanicLatch::is_failed`] inside `body` and output silence without
/// calling the plugin.
pub fn run_audio_block_latched<P, R>(
    latch: &PanicLatch,
    format: &str,
    body: impl FnOnce() -> R,
) -> Option<R> {
    let result = quiet_panic_hook::silenced(|| catch_unwind(AssertUnwindSafe(body)));
    match result {
        Ok(r) => Some(r),
        Err(payload) => {
            quiet_panic_hook::with_location(|location| {
                latch.record(&[
                    "[moose ",
                    format,
                    "] panic in process() for ",
                    type_name::<P>(),
                    location,
                    ": ",
                    extract_panic_msg(&payload),
                    " (plugin silenced until the next activate)",
                ]);
            });
            None
        }
    }
}

/// A process-wide panic hook that keeps `process` panics off stderr.
///
/// std's default hook formats and prints the panic on the panicking thread,
/// which for `process` is the audio thread. The hook installed here wraps
/// whatever hook was set before it: outside [`silenced`] it calls that hook
/// unchanged, inside it only copies the panic location into a fixed
/// thread-local buffer, which [`PanicLatch::report`] later prints from a
/// host thread together with the message.
///
/// A host or plugin that replaces the hook afterwards wins; then its hook
/// prints, as before this existed.
mod quiet_panic_hook {
    use std::cell::{Cell, RefCell};
    use std::fmt::Write as _;
    use std::sync::Once;

    /// Bytes kept of `" at <file>:<line>:<col>"`; longer paths are cut.
    const LOCATION_CAP: usize = 160;

    /// A fixed-capacity, non-allocating text buffer.
    struct Location {
        bytes: [u8; LOCATION_CAP],
        len: usize,
    }

    impl Location {
        fn as_str(&self) -> &str {
            // Only `write_str` fills the buffer, and it cuts on char boundaries.
            std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
        }
    }

    impl std::fmt::Write for Location {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            let mut end = s.len().min(LOCATION_CAP - self.len);
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            self.bytes[self.len..self.len + end].copy_from_slice(&s.as_bytes()[..end]);
            self.len += end;
            Ok(())
        }
    }

    thread_local! {
        static SILENCED: Cell<bool> = const { Cell::new(false) };
        static LOCATION: RefCell<Location> =
            const { RefCell::new(Location { bytes: [0; LOCATION_CAP], len: 0 }) };
    }

    /// Install the hook once per process. Host thread only: it allocates.
    pub(super) fn install() {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if SILENCED.get() {
                    LOCATION.with_borrow_mut(|buf| {
                        buf.len = 0;
                        if let Some(at) = info.location() {
                            let _ = write!(buf, " at {}:{}:{}", at.file(), at.line(), at.column());
                        }
                    });
                } else {
                    previous(info);
                }
            }));
        });
    }

    /// Run `f` with this thread's panics kept off stderr.
    pub(super) fn silenced<R>(f: impl FnOnce() -> R) -> R {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                SILENCED.set(self.0);
            }
        }
        let _restore = Restore(SILENCED.replace(true));
        f()
    }

    /// The location of this thread's last silenced panic (empty if none).
    pub(super) fn with_location<R>(f: impl FnOnce(&str) -> R) -> R {
        LOCATION.with_borrow(|buf| f(buf.as_str()))
    }
}

/// Run a generic `extern "C"` callback body under
/// [`std::panic::catch_unwind`]. Returns `body`'s value on a clean
/// exit, `fallback` if the body panicked.
///
/// Parameterized on `action` (e.g. `"save_state"`, `"load_state"`) so the panic log
/// pinpoints which callback boundary fired. Use this for non-process
/// FFI surfaces - state save / load, param formatting, anything the
/// host calls through an `extern "C" fn` where a panic would unwind
/// across an ABI that doesn't promise abort-on-unwind.
///
/// Audio-thread process bodies use [`run_audio_block_latched`] instead,
/// which never prints on the audio thread.
pub fn run_extern_callback_with<P, R>(
    format: &str,
    action: &str,
    fallback: R,
    body: impl FnOnce() -> R,
) -> R {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(r) => r,
        Err(payload) => {
            eprintln!(
                "[moose {format}] panic in {action} for {}: {}",
                type_name::<P>(),
                extract_panic_msg(&payload),
            );
            fallback
        }
    }
}

fn extract_panic_msg(payload: &Box<dyn std::any::Any + Send>) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}

/// Copy `text` into the host's C-string buffer `out` of capacity `out_len`
/// bytes (including the trailing NUL), NUL-terminate, and return the number
/// of content bytes written (excluding the NUL).
///
/// Truncates on a UTF-8 char boundary: audio param display strings are full
/// of multi-byte characters (`°`, `µs`, `−12 dB`, `Δ`, `♯`), and cutting one
/// mid-codepoint yields an invalid C string that strict host readers reject
/// wholesale. Every format wrapper's `format_value` path funnels through
/// here so the truncation rule lives in one place, not five copies.
///
/// # Safety
/// `out` must be valid for writes of `out_len` bytes, and `out_len` must be
/// `> 0`. Callers guard `out_len == 0` / a null `out` as "host wants
/// nothing" before calling.
#[must_use]
pub unsafe fn copy_c_str(out: *mut c_char, out_len: usize, text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut len = bytes.len().min(out_len - 1);
    // Walk back off a torn multi-byte tail. `is_char_boundary(bytes.len())`
    // is always true, so a string that fits untouched never loops.
    while len > 0 && !text.is_char_boundary(len) {
        len -= 1;
    }
    // SAFETY: `len < out_len` (so `out.add(len)` is in bounds for the NUL),
    // and `out` is valid for `out_len` writes per the contract. Reinterpreting
    // the `u8` bytes as `c_char` is a bit-preserving copy on every platform
    // (`c_char` is `i8` or `u8`).
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, len);
        *out.add(len) = 0;
    }
    len
}

#[cfg(test)]
mod plugin_cell_tests {
    use std::sync::Arc;

    use super::{PluginCell, enter_plugin, shared_plugin};

    #[test]
    fn try_enter_declines_while_held_then_accepts_when_free() {
        // The request_resize re-entry guard: `try_enter` must return `None`
        // while the cell is held (so the closure defers instead of aliasing)
        // and `Some` once released.
        let cell = PluginCell::new(0u32);
        let guard = cell.enter();
        assert!(cell.try_enter().is_none(), "held cell declines try_enter");
        drop(guard);
        assert!(cell.try_enter().is_some(), "freed cell accepts try_enter");
    }

    #[test]
    #[should_panic(expected = "re-entered on the same thread")]
    fn enter_while_held_reports_same_thread_reentry() {
        // Author editor code calling back into the cell (request_resize from
        // set_size) must be blamed accurately, not reported as a host overlap.
        let cell = PluginCell::new(0u32);
        let _held = cell.enter();
        let _reenter = cell.enter();
    }

    /// A host that overlaps two owners gets the call served late, not
    /// dropped: `enter` from another thread waits for the release.
    #[test]
    fn enter_from_another_thread_waits_for_the_owner() {
        let cell = std::sync::Arc::new(PluginCell::new(0u32));
        let mut held = cell.enter();
        let waiter = {
            let cell = std::sync::Arc::clone(&cell);
            std::thread::spawn(move || {
                let mut guard = cell.enter();
                *guard += 1;
                *guard
            })
        };
        // The waiter can't get in while this thread holds the cell.
        std::thread::sleep(std::time::Duration::from_millis(20));
        *held = 41;
        drop(held);
        assert_eq!(waiter.join().expect("waiter finished"), 42, "saw the owner's write");
    }

    #[test]
    fn lock_round_trips_data() {
        let plugin = shared_plugin(41);
        *enter_plugin(&plugin) += 1;
        assert_eq!(*enter_plugin(&plugin), 42);
    }

    #[test]
    fn repeated_ownership_publishes_writes() {
        // Models the audio thread owning the cell block after block:
        // each release-acquire cycle observes the previous cycle's write.
        let plugin = shared_plugin(0u64);
        for _ in 0..1000 {
            *enter_plugin(&plugin) += 1;
        }
        assert_eq!(*enter_plugin(&plugin), 1000);
    }

    #[test]
    fn handoff_carries_writes_across_a_thread() {
        // A non-overlapping handoff (the host contract): the worker owns
        // the cell, writes, and releases; only after it joins does the
        // main thread acquire. The cell's `Acquire` makes the worker's
        // write visible - no overlap, so the detector never trips.
        let plugin = shared_plugin(0u32);
        let worker = {
            let plugin = Arc::clone(&plugin);
            std::thread::spawn(move || {
                *enter_plugin(&plugin) = 99;
            })
        };
        worker.join().unwrap();
        assert_eq!(*enter_plugin(&plugin), 99);
    }

    #[test]
    fn panicking_owner_does_not_wedge_the_cell() {
        // A panic in an owner unwinds through the guard's `Drop`, which
        // releases the handoff (and clears the debug overlap flag), so
        // the cell stays usable - one bad block can't wedge it.
        let plugin = shared_plugin(7);
        let for_panic = Arc::clone(&plugin);
        let _ = std::thread::spawn(move || {
            let _guard = enter_plugin(&for_panic);
            panic!("wedge attempt");
        })
        .join();
        assert_eq!(*enter_plugin(&plugin), 7);
    }

    /// The `extern "C"` firewall: a body that panics returns the fallback
    /// (so a panic can't unwind across the ABI and abort the caller), and
    /// a clean body returns its value. The generated `__moose_screenshot`
    /// symbol relies on this.
    #[test]
    fn run_extern_callback_with_catches_panic_and_returns_fallback() {
        let ok = super::run_extern_callback_with::<(), u32>("test", "clean", 9, || 3);
        assert_eq!(ok, 3, "a clean body returns its own value");

        let caught =
            super::run_extern_callback_with::<(), u32>("test", "boom", 9, || panic!("boom"));
        assert_eq!(caught, 9, "a panicking body returns the fallback");
    }
}

#[cfg(test)]
mod copy_c_str_tests {
    use super::copy_c_str;
    use std::os::raw::c_char;

    /// Copy `text` into a `cap`-byte buffer; return `(written_len, content)`
    /// where `content` is the NUL-terminated bytes read back as a `String`.
    fn run(text: &str, cap: usize) -> (usize, String) {
        let mut buf = vec![0 as c_char; cap];
        // SAFETY: `buf` is `cap` `c_char`, and `cap > 0` in every case here.
        let n = unsafe { copy_c_str(buf.as_mut_ptr(), cap, text) };
        // `c_char` -> `u8` is the bit-preserving inverse of the copy.
        #[allow(clippy::cast_sign_loss)]
        let bytes: Vec<u8> = buf[..n].iter().map(|&c| c as u8).collect();
        (n, String::from_utf8(bytes).unwrap())
    }

    #[test]
    fn copies_when_it_fits() {
        let (n, s) = run("-6 dB", 32);
        assert_eq!(n, 5);
        assert_eq!(s, "-6 dB");
    }

    #[test]
    fn writes_the_trailing_nul() {
        let mut buf = [1 as c_char; 8];
        // SAFETY: 8-byte buffer, capacity 8.
        let n = unsafe { copy_c_str(buf.as_mut_ptr(), 8, "ab") };
        assert_eq!(n, 2);
        assert_eq!(buf[2], 0, "NUL terminator written after the content");
    }

    #[test]
    fn truncates_ascii_to_capacity() {
        // cap 4 -> 3 content bytes + NUL.
        let (n, s) = run("abcdef", 4);
        assert_eq!(n, 3);
        assert_eq!(s, "abc");
    }

    #[test]
    fn truncates_on_a_char_boundary() {
        // "12°" is [0x31, 0x32, 0xC2, 0xB0]. cap 4 -> 3 content bytes would
        // cut '°' (U+00B0) mid-codepoint; the guard backs off to 2 so the
        // result is valid UTF-8, not a torn tail a strict host rejects.
        let (n, s) = run("12°", 4);
        assert_eq!(n, 2, "dropped the half-written degree sign");
        assert_eq!(s, "12");
    }

    #[test]
    fn multibyte_that_fits_is_untouched() {
        // U+2212 MINUS SIGN (3 bytes) plus " 12 dB".
        let text = "−12 dB";
        let (n, s) = run(text, 32);
        assert_eq!(n, text.len());
        assert_eq!(s, text);
    }
}

#[cfg(test)]
mod panic_latch_tests {
    use super::*;

    #[test]
    fn latch_keeps_a_bounded_message_and_clears() {
        let latch = PanicLatch::new();
        let long = "é".repeat(400);
        let out = run_audio_block_latched::<(), ()>(&latch, "TEST", || {
            std::panic::panic_any(long.clone())
        });
        assert!(out.is_none() && latch.is_failed());
        {
            let slot = latch.message.lock().unwrap();
            assert!(slot.len() <= PANIC_MESSAGE_CAP && slot.starts_with("[moose TEST]"));
            // The quiet hook kept the panic off stderr and handed its location to the latch.
            assert!(slot.contains(" at ") && slot.contains("wrapper.rs:"), "{slot}");
            assert_eq!(slot.capacity(), PANIC_MESSAGE_CAP, "the slot never grows");
        }
        latch.clear();
        assert!(!latch.is_failed() && latch.message.lock().unwrap().is_empty());
    }
}
