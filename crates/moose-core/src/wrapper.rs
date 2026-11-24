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

use moose_params::ParamInfo;

use crate::export::PluginExport;

pub(crate) use plugin_cell::thread_token as current_thread_token;
pub use plugin_cell::{PluginCell, PluginGuard};

/// Take ownership of the plugin for a **non-real-time** lifecycle callback
/// (host main thread). Uncontended entry takes no OS lock; if a misbehaving
/// host overlaps audio processing, this waits politely for the in-flight block
/// to finish. Real-time audio callbacks (`process`, audio-thread `reset`) MUST
/// use [`PluginCell::try_enter`] instead so the audio thread never parks.
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

/// How long a host-thread save or load waits for an **actively streaming**
/// audio thread to serve it at the next block boundary before falling back.
/// Sleeping, bypassed, suspended, or single-threaded hosts do not stall for
/// this duration (the wrapper checks the audio heartbeat and ownership cell
/// first and returns immediately when no other thread is pumping blocks).
pub const AUDIO_HANDSHAKE_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Pop and apply one queued state load on the thread that owns the plugin,
/// record the result for a waiting host thread, and ask the shell for a
/// fresh snapshot at the end of the next `process`. `None` when nothing was
/// queued.
///
/// When the plugin opted into off-thread decoding
/// ([`PluginRuntime::decode_state`](crate::plugin::PluginRuntime::decode_state)),
/// the swap runs inside an [`RtSection`](crate::rt::RtSection) with zero
/// audio-thread allocations or deallocations: the displaced old state is
/// pushed into `queue`'s lock-free retirement channel to drop off the audio
/// thread. Otherwise `load_state` runs outside the real-time section for
/// backwards compatibility.
pub fn apply_queued_state<P: PluginExport>(
    plugin: &mut P,
    queue: &crate::state::StateLoadQueue,
) -> Option<Result<(), crate::state::StateLoadError>> {
    use moose_params::Params;

    let queued = queue.pop_queued()?;
    let state_ptr = std::sync::Arc::as_ptr(&queued.state) as usize;
    if let Some(mut decoded) = queued.decoded {
        let slot = plugin.snapshot_slot();
        {
            let _rt = crate::rt::RtSection::enter();
            plugin.params().restore_values(&queued.state.params);
            plugin.params().snap_smoothers();
            plugin.apply_decoded_state(&mut decoded);
            queue.retire_decoded(decoded);
            slot.mark_state_loaded();
            queue.set_accepted_custom(state_ptr);
            let ticket = slot.request_fresh();
            slot.record_load_ticket(state_ptr, ticket);
            queue.mark_applied(&queued.state, true);
        }
        return Some(Ok(()));
    }
    let result = crate::state::try_apply_state(plugin, &queued.state);
    if result.is_err()
        && let Some(backup) = &queued.backup
    {
        backup.restore(plugin.params());
    }
    let slot = plugin.snapshot_slot();
    if result.is_ok() && queued.state.extra.is_some() {
        slot.mark_state_loaded();
        queue.set_accepted_custom(state_ptr);
    }
    let ticket = slot.request_fresh();
    slot.record_load_ticket(state_ptr, ticket);
    queue.mark_applied(&queued.state, result.is_ok());
    Some(result)
}

/// Load a host state into the plugin. With `can_own` (the wrapper is not
/// processing) the host thread takes the plugin and applies it now;
/// otherwise the state is queued for the audio thread. If another thread is
/// actively pumping audio blocks (measured via the audio heartbeat and
/// [`PluginCell::is_held_by_other_thread`]), this waits up to
/// [`AUDIO_HANDSHAKE_WAIT`] (woken immediately when the block finishes) so an
/// editor notified next reads the new state. On a sleeping, bypassed,
/// suspended, or single-threaded host, this returns `Ok(())` immediately
/// without stalling.
///
/// Params and `#[persist]` must already be applied (`read_and_apply_params`
/// / `apply_params`); `backup` is what they were before. When the plugin's
/// [`validate_state`](crate::plugin::PluginRuntime::validate_state),
/// [`decode_state`](crate::plugin::PluginRuntime::decode_state), or
/// `load_state` rejects the state, they are restored from `backup`, so the
/// load fails as a unit and the error is returned for the host. A load still
/// queued when this returns `Ok` reports its custom bytes on saves until it
/// lands (see [`custom_state_view`]).
///
/// # Errors
///
/// The plugin's `validate_state`, `decode_state`, or `load_state` error, when
/// it ran before this returned.
pub fn apply_or_queue_state<P: PluginExport>(
    plugin: &PluginCell<P>,
    params: &P::Params,
    slot: &crate::snapshot::SnapshotSlot,
    queue: &crate::state::StateLoadQueue,
    state: crate::state::DeserializedState,
    backup: &crate::state::ParamsBackup,
    can_own: bool,
) -> Result<(), crate::state::StateLoadError> {
    use moose_params::Params;

    // Host thread, before anything is applied or queued: a queued load may
    // answer the host before the audio thread runs, so validation and
    // off-thread decoding both run synchronously here.
    let decoded = if let Some(extra) = &state.extra {
        if let Err(err) = P::validate_state(extra) {
            backup.restore(params);
            return Err(err);
        }
        match P::decode_state(extra) {
            Ok(decoded) => decoded,
            Err(err) => {
                backup.restore(params);
                return Err(err);
            }
        }
    } else {
        None
    };
    // `try_enter`: a CLAP `reset` may hold the cell on the audio thread
    // while stopped; then queue instead of panicking on an overlap.
    if can_own && let Some(mut instance) = plugin.try_enter() {
        // An older queued load is superseded by this one.
        if let Some(stale) = queue.pop() {
            queue.mark_applied(&stale, true);
        }
        queue.reclaim_retired();
        let has_custom_extra = state.extra.is_some();

        let result = if let Some(mut decoded) = decoded {
            instance.params().restore_values(&state.params);
            instance.params().snap_smoothers();
            instance.apply_decoded_state(&mut decoded);
            // `decoded` (holding the swapped-out old state) drops here on the host thread.
            Ok(())
        } else {
            crate::state::try_apply_state(&mut *instance, &state)
        };
        if result.is_err() {
            backup.restore(params);
        } else if has_custom_extra {
            slot.mark_state_loaded();
            queue.set_accepted_custom(0);
        }
        instance.republish_snapshot();
        if result.is_ok()
            && let Some(extra) = state.extra
            && slot.read_current().is_none()
        {
            slot.store_pending_extra(extra);
        }
        return result;
    }
    let handle = queue.push_with_backup_and_decoded(state, Some(backup.clone()), decoded);
    let audio_active = plugin.is_held_by_other_thread()
        || queue.is_audio_active_on_other_thread()
        || slot.is_audio_active_on_other_thread();
    if !audio_active {
        return Ok(());
    }
    match queue.wait_applied(&handle, AUDIO_HANDSHAKE_WAIT) {
        Some(false) => {
            queue.reclaim_retired();
            Err(crate::state::StateLoadError::Other(
                "plugin load_state rejected the state".into(),
            ))
        }
        // Applied: the audio thread requested a fresh snapshot right after
        // the apply and recorded its ticket before marking completion. Wait on
        // that original ticket so we don't wait for a second block if the
        // snapshot was already served.
        Some(true) => {
            queue.reclaim_retired();
            let ticket = slot
                .ticket_for_load(std::sync::Arc::as_ptr(&handle) as usize)
                .unwrap_or_else(|| slot.request_fresh());
            // A late snapshot only delays freshness; the load itself landed.
            let _ = slot.wait_served(ticket, AUDIO_HANDSHAKE_WAIT);
            Ok(())
        }
        None => Ok(()),
    }
}

/// Custom state for a host save. With `can_own` the host thread takes the
/// plugin, applies any queued load, and serializes live state. Otherwise, if
/// another thread is actively pumping audio blocks, it asks the audio thread
/// for a fresh snapshot and waits up to [`AUDIO_HANDSHAKE_WAIT`]; if the host
/// is sleeping/bypassed/single-threaded or no block runs in time, it falls
/// back immediately to [`custom_state_view`].
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
        queue.reclaim_retired();
        return crate::export::current_custom_state(&*instance)
            .or_else(|| slot.read_current())
            .or_else(|| queue.accepted_extra())
            .unwrap_or_default();
    }
    let audio_active = plugin.is_held_by_other_thread()
        || queue.is_audio_active_on_other_thread()
        || slot.is_audio_active_on_other_thread();
    if !audio_active {
        return custom_state_view(slot, queue);
    }
    let ticket = slot.request_fresh();
    let _ = slot.wait_served(ticket, AUDIO_HANDSHAKE_WAIT);
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
        None => slot
            .read_current()
            .or_else(|| queue.accepted_extra())
            .unwrap_or_default(),
    }
}

/// Plugin ownership cell, uniform across platforms: the audio thread owns
/// the plugin while processing, the host thread while stopped. Ownership
/// is one atomic flag plus a release-acquire handoff, so an uncontended
/// `enter` takes no OS lock. A spec-compliant host never overlaps the two
/// owners; when one does, `enter` parks until the other releases rather
/// than dropping the call (audit finding 8).
mod plugin_cell {
    use crate::sync::{AtomicBool, AtomicU64, Ordering};
    use std::cell::UnsafeCell;
    use std::marker::PhantomData;
    use std::ops::{Deref, DerefMut};
    use std::sync::Mutex;
    use std::thread::{self, Thread};

    /// Longest a waiting [`PluginCell::enter`] parks before it re-checks the
    /// cell on its own. The releasing owner unparks the waiter, so this only
    /// bounds a wake-up lost to a `try_lock` race on the waker slot.
    #[cfg(not(all(test, feature = "loom")))]
    const WAKE_BACKSTOP: std::time::Duration = std::time::Duration::from_millis(2);

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
    pub(crate) fn thread_token() -> u64 {
        // loom runs its model threads on one OS thread, so the token must
        // be per loom thread there or every thread would look like the owner.
        #[cfg(all(test, feature = "loom"))]
        use loom::thread_local;
        thread_local! {
            static TOKEN: u64 = {
                static NEXT: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(1);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
        /// Intended for **non-real-time** lifecycle callbacks (host main thread).
        /// Real-time audio callbacks (`process`, audio-thread `reset`) MUST use
        /// [`Self::try_enter`] so a misbehaving host overlapping a lifecycle call
        /// can never park the audio thread. Under `rt-paranoid`, calling `enter()`
        /// inside an [`RtSection`](crate::rt::RtSection) is flagged as a lock
        /// violation.
        ///
        /// # Panics
        /// Panics when this thread already holds the cell: author code
        /// (e.g. `request_resize` from `set_size` / `state_changed`) reached
        /// back into it while the wrapper held it, which can never be
        /// waited out. Use [`Self::try_enter`] on such paths.
        pub fn enter(&self) -> PluginGuard<'_, T> {
            crate::rt::note_lock();
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
                // loom threads can't OS-park; yielding lets the model run
                // the owner to its release.
                #[cfg(all(test, feature = "loom"))]
                loom::thread::yield_now();
                #[cfg(not(all(test, feature = "loom")))]
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

        /// Whether another thread currently holds this cell. Wait-free.
        #[must_use]
        pub fn is_held_by_other_thread(&self) -> bool {
            if !self.held.load(Ordering::Acquire) {
                return false;
            }
            let owner = self.owner.load(Ordering::Relaxed);
            owner != 0 && owner != thread_token()
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
        self.failed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Host thread: take any recorded panic message once and pass it to `f`.
    ///
    /// Wrappers that expose a host logging API (such as CLAP `clap.log`) use
    /// this to forward the message to the host instead of printing to stderr.
    pub fn report_with(&self, f: impl FnOnce(&str)) {
        let msg = {
            let mut slot = self
                .message
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_empty() {
                String::new()
            } else {
                // Clone out and clear rather than `mem::take` so the slot
                // keeps its preallocated `PANIC_MESSAGE_CAP` capacity for
                // future audio-thread `record` calls (which never allocate).
                let out = slot.clone();
                slot.clear();
                out
            }
        };
        if !msg.is_empty() {
            f(&msg);
        }
    }

    /// Host thread: print a recorded message, once.
    pub fn report(&self) {
        self.report_with(|msg| eprintln!("{msg}"));
    }

    /// Host thread, after a successful prepare / activate: report any
    /// pending message through `f` and let `process` call the plugin again.
    pub fn clear_with(&self, f: impl FnOnce(&str)) {
        self.report_with(f);
        self.failed
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Host thread, after a successful prepare / activate: report any
    /// pending message and let `process` call the plugin again.
    pub fn clear(&self) {
        self.clear_with(|msg| eprintln!("{msg}"));
    }
}

impl Drop for PanicLatch {
    fn drop(&mut self) {
        self.report();
    }
}

/// [`run_extern_callback_with`] with a [`PanicLatch`]: `None` when `body`
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

    use super::{PluginCell, enter_plugin};

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
        assert_eq!(
            waiter.join().expect("waiter finished"),
            42,
            "saw the owner's write"
        );
    }

    #[test]
    fn lock_round_trips_data() {
        let plugin = PluginCell::new(41);
        *enter_plugin(&plugin) += 1;
        assert_eq!(*enter_plugin(&plugin), 42);
    }

    #[test]
    fn repeated_ownership_publishes_writes() {
        // Models the audio thread owning the cell block after block:
        // each release-acquire cycle observes the previous cycle's write.
        let plugin = PluginCell::new(0u64);
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
        let plugin = Arc::new(PluginCell::new(0u32));
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
        let plugin = Arc::new(PluginCell::new(7));
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
            assert!(
                slot.contains(" at ") && slot.contains("wrapper.rs:"),
                "{slot}"
            );
            assert_eq!(slot.capacity(), PANIC_MESSAGE_CAP, "the slot never grows");
        }
        latch.clear();
        {
            let slot = latch.message.lock().unwrap();
            assert!(!latch.is_failed() && slot.is_empty());
            assert_eq!(
                slot.capacity(),
                PANIC_MESSAGE_CAP,
                "clear/report must preserve preallocated capacity for future audio panics"
            );
        }
        // A second panic after clear still captures its message without allocating.
        let out2 = run_audio_block_latched::<(), ()>(&latch, "TEST", || {
            std::panic::panic_any("second panic")
        });
        assert!(out2.is_none() && latch.is_failed());
        let mut reported = String::new();
        latch.clear_with(|msg| reported.push_str(msg));
        assert!(reported.contains("second panic"), "{reported}");
        assert_eq!(latch.message.lock().unwrap().capacity(), PANIC_MESSAGE_CAP);
    }
}

#[cfg(test)]
mod state_handshake_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::{PluginCell, apply_or_queue_state, apply_queued_state, custom_state_for_save};
    use crate::buffer::AudioBuffer;
    use crate::config::AudioConfig;
    use crate::events::EventList;
    use crate::export::PluginExport;
    use crate::info::{MidiDialect, PluginCategory, PluginInfo};
    use crate::meters::MeterStore;
    use crate::plugin::PluginRuntime;
    use crate::process::{ProcessContext, ProcessStatus};
    use crate::snapshot::SnapshotSlot;
    use crate::state::{
        DecodedState, DeserializedState, ParamsBackup, StateLoadError, StateLoadQueue,
    };
    use moose_params::{DynamicParams, Params, SlotDef, SlotRange};

    struct TrackedTable {
        tag: u32,
        _buf: Vec<f32>,
        drops: Arc<AtomicUsize>,
        dropped_on_audio: Arc<AtomicBool>,
        audio_tid: std::thread::ThreadId,
    }

    impl Drop for TrackedTable {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
            if std::thread::current().id() == self.audio_tid {
                self.dropped_on_audio.store(true, Ordering::Release);
            }
        }
    }

    struct SwapPlugin {
        params: Arc<DynamicParams<1>>,
        meters: Arc<MeterStore>,
        snapshot: Arc<SnapshotSlot>,
        table: Option<TrackedTable>,
        tag: Option<u32>,
        observed_tag: Arc<std::sync::atomic::AtomicU32>,
        raw_extra: Vec<u8>,
    }

    impl PluginRuntime for SwapPlugin {
        type Sample = f32;
        fn info() -> PluginInfo {
            PluginInfo {
                name: "Swap",
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
                bundle_id: "swap",
                vst3_id: "com.test.swap",
                clap_id: "com.test.swap",
                fourcc: *b"Swap",
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
        fn reset(&mut self, _config: &AudioConfig) {}
        fn process(
            &mut self,
            _buffer: &mut AudioBuffer<f32>,
            _events: &EventList,
            _context: &mut ProcessContext,
        ) -> ProcessStatus {
            ProcessStatus::Normal
        }
        fn save_state(&self) -> Vec<u8> {
            if let Some(table) = &self.table {
                table.tag.to_le_bytes().to_vec()
            } else if let Some(tag) = self.tag {
                tag.to_le_bytes().to_vec()
            } else {
                self.raw_extra.clone()
            }
        }
        fn decode_state(data: &[u8]) -> Result<Option<DecodedState>, StateLoadError> {
            if data == b"BAD!" {
                return Err(StateLoadError::Malformed("bad decoded state"));
            }
            if data.len() == 4 {
                let tag = u32::from_le_bytes(data.try_into().unwrap());
                return Ok(Some(DecodedState::new(tag)));
            }
            Ok(None)
        }
        fn apply_decoded_state(&mut self, decoded: &mut DecodedState) {
            if let Some(table) = &mut self.table {
                decoded.swap_with(table);
                return;
            }
            if let Some(&tag) = decoded.downcast_ref::<u32>() {
                self.tag = Some(tag);
                self.observed_tag.store(tag, Ordering::Release);
            }
        }
    }

    impl PluginExport for SwapPlugin {
        type Params = DynamicParams<1>;
        fn create() -> Self {
            let params = DynamicParams::<1>::new();
            let slot0 = params.slot(0).unwrap();
            params.edit(|e| {
                e.bind(
                    slot0,
                    SlotDef::new("Gain", SlotRange::Linear { min: 0.0, max: 1.0 }),
                );
            });
            Self {
                params: Arc::new(params),
                meters: MeterStore::new(),
                snapshot: SnapshotSlot::new(),
                table: None,
                tag: None,
                observed_tag: Arc::new(std::sync::atomic::AtomicU32::new(0)),
                raw_extra: Vec::new(),
            }
        }
        fn params(&self) -> &DynamicParams<1> {
            &self.params
        }
        fn params_arc(&self) -> Arc<DynamicParams<1>> {
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
    #[cfg_attr(miri, ignore = "wall-clock latency bound; Miri is ~100x slower")]
    fn sleeping_host_does_not_stall_50ms_on_load_or_save() {
        let plugin = PluginCell::new(SwapPlugin::create());
        let params = plugin.enter().params_arc();
        let slot = plugin.enter().snapshot_slot();
        let queue = StateLoadQueue::new(1);
        let backup = ParamsBackup::capture(&*params);

        let start = Instant::now();
        let state = DeserializedState {
            extra: Some(42u32.to_le_bytes().to_vec()),
            ..Default::default()
        };
        apply_or_queue_state(&plugin, &*params, &slot, &queue, state, &backup, false)
            .expect("queued load succeeds");
        let saved = custom_state_for_save(&plugin, &slot, &queue, false);
        let elapsed = start.elapsed();

        assert_eq!(saved, 42u32.to_le_bytes().to_vec());
        assert!(
            elapsed < Duration::from_millis(20),
            "sleeping host stalled for {elapsed:?} (must not hit 50 ms AUDIO_HANDSHAKE_WAIT)"
        );
    }

    #[test]
    fn decode_state_error_rolls_back_params_even_when_processing() {
        let plugin = PluginCell::new(SwapPlugin::create());
        let params = plugin.enter().params_arc();
        let slot = plugin.enter().snapshot_slot();
        let queue = StateLoadQueue::new(1);
        let gain_id = params.param_infos()[0].id;

        params.set_plain(gain_id, 0.25);
        let backup = ParamsBackup::capture(&*params);
        params.set_plain(gain_id, 0.99);

        let bad = DeserializedState {
            extra: Some(b"BAD!".to_vec()),
            ..Default::default()
        };
        let err = apply_or_queue_state(&plugin, &*params, &slot, &queue, bad, &backup, false);
        assert!(err.is_err(), "decode_state failure rejects load up front");
        assert!(
            (params.get_plain(gain_id).unwrap() - 0.25).abs() < 1e-6,
            "failed decode_state restored backup params"
        );
        assert!(queue.unapplied().is_none(), "failed load was not queued");
    }

    #[test]
    fn off_thread_decoded_state_swaps_on_audio_and_drops_old_state_off_audio() {
        let plugin = Arc::new(PluginCell::new(SwapPlugin::create()));
        let queue = Arc::new(StateLoadQueue::new(1));
        let drops = Arc::new(AtomicUsize::new(0));
        let dropped_on_audio = Arc::new(AtomicBool::new(false));

        let (tx_tid, rx_tid) = std::sync::mpsc::channel();
        let (tx_go, rx_go) = std::sync::mpsc::channel();
        let (tx_done, rx_done) = std::sync::mpsc::channel();

        let audio_plugin = Arc::clone(&plugin);
        let audio_queue = Arc::clone(&queue);
        let audio = std::thread::spawn(move || {
            tx_tid.send(std::thread::current().id()).unwrap();
            rx_go.recv().unwrap();
            audio_queue.record_audio_block();
            let mut guard = audio_plugin.try_enter().expect("audio owns plugin");
            let applied = apply_queued_state(&mut *guard, &audio_queue);
            assert!(matches!(applied, Some(Ok(()))));
            assert_eq!(guard.table.as_ref().unwrap().tag, 2);
            tx_done.send(()).unwrap();
        });

        let audio_tid = rx_tid.recv().unwrap();
        plugin.enter().table = Some(TrackedTable {
            tag: 1,
            _buf: vec![1.0; 1024],
            drops: Arc::clone(&drops),
            dropped_on_audio: Arc::clone(&dropped_on_audio),
            audio_tid,
        });

        let new_table = TrackedTable {
            tag: 2,
            _buf: vec![2.0; 1024],
            drops: Arc::clone(&drops),
            dropped_on_audio: Arc::clone(&dropped_on_audio),
            audio_tid,
        };
        let _handle = queue.push_with_backup_and_decoded(
            DeserializedState::default(),
            None,
            Some(DecodedState::new(new_table)),
        );
        tx_go.send(()).unwrap();
        rx_done.recv().unwrap();
        audio.join().unwrap();

        assert!(
            !dropped_on_audio.load(Ordering::Acquire),
            "old heap state must NEVER drop on the audio thread"
        );
        assert_eq!(
            drops.load(Ordering::Relaxed),
            0,
            "old state waits in retirement queue until host reclaims"
        );
        queue.reclaim_retired();
        assert_eq!(
            drops.load(Ordering::Relaxed),
            1,
            "old state dropped on host thread after reclaim"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "wall-clock latency bound; Miri is ~100x slower")]
    fn live_audio_thread_handshake_wakes_immediately_without_50ms_wait() {
        let plugin = Arc::new(PluginCell::new(SwapPlugin::create()));
        let params = plugin.enter().params_arc();
        let slot = plugin.enter().snapshot_slot();
        let observed_tag = Arc::clone(&plugin.enter().observed_tag);
        let queue = Arc::new(StateLoadQueue::new(1));
        let stop = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));

        let audio = {
            let plugin = Arc::clone(&plugin);
            let queue = Arc::clone(&queue);
            let stop = Arc::clone(&stop);
            let started = Arc::clone(&started);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    {
                        let mut guard = plugin.enter();
                        queue.record_audio_block();
                        let _ = apply_queued_state(&mut *guard, &queue);
                        if let Some(ticket) = guard.snapshot_slot().pending_request() {
                            let bytes = guard.save_state();
                            guard.snapshot_slot().serve(ticket, |buf| {
                                buf.extend_from_slice(&bytes);
                            });
                        }
                    }
                    started.store(true, Ordering::Release);
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };

        while !started.load(Ordering::Acquire) {
            std::thread::yield_now();
        }

        let expected = 42u32.to_le_bytes().to_vec();
        let backup = ParamsBackup::capture(&*params);
        let t0 = Instant::now();
        let load_res = apply_or_queue_state(
            &plugin,
            &*params,
            &slot,
            &queue,
            DeserializedState {
                extra: Some(expected.clone()),
                ..Default::default()
            },
            &backup,
            false,
        );
        let unapplied_after_load = queue.unapplied();
        let tag_after_load = observed_tag.load(Ordering::Acquire);
        let saved = custom_state_for_save(&plugin, &slot, &queue, false);
        let elapsed = t0.elapsed();

        // Stop and join the audio worker BEFORE asserting so a failure cannot
        // leave the background thread running.
        stop.store(true, Ordering::Release);
        audio.join().unwrap();

        load_res.expect("live audio thread applies queued state synchronously");
        assert!(
            unapplied_after_load.is_none(),
            "live audio handshake must apply load before returning"
        );
        assert_eq!(
            tag_after_load, 42,
            "processor must hold decoded tag immediately after live apply_or_queue_state"
        );
        assert_eq!(saved, expected);
        assert!(
            elapsed < Duration::from_millis(35),
            "live audio handshake should finish quickly via atomic polling, took {elapsed:?}"
        );
    }

    #[test]
    fn completed_waits_succeed_after_stop() {
        let queue = Arc::new(StateLoadQueue::new(1));
        let slot = SnapshotSlot::new();

        // Already-completed requests keep their result even if audio stops after.
        let done_handle = queue.push_tracked(DeserializedState::default());
        queue.mark_applied(&done_handle, true);
        queue.mark_audio_inactive();
        assert_eq!(
            queue.wait_applied(&done_handle, Duration::from_millis(50)),
            Some(true)
        );
        let done_ticket = slot.request_fresh();
        slot.serve(done_ticket, |buf| buf.extend_from_slice(b"ok"));
        slot.mark_audio_inactive();
        assert!(slot.wait_served(done_ticket, Duration::from_millis(50)));
    }

    #[test]
    fn stop_during_poll_interrupts_each_wait() {
        let queue = StateLoadQueue::new(1);
        let slot = SnapshotSlot::new();
        queue.record_audio_block();
        let _ = slot.pending_request();
        let pending = queue.push_tracked(DeserializedState::default());
        let ticket = slot.request_fresh();
        let mut queue_polls = 0;
        assert_eq!(
            queue.wait_applied_poll(&pending, Duration::from_secs(2), || {
                queue_polls += 1;
                assert_eq!(queue_polls, 1);
                queue.mark_audio_inactive();
            }),
            None
        );
        assert_eq!(queue_polls, 1);
        let mut slot_polls = 0;
        assert!(!slot.wait_served_poll(ticket, Duration::from_secs(2), || {
            slot_polls += 1;
            assert_eq!(slot_polls, 1);
            slot.mark_audio_inactive();
        }));
        assert_eq!(slot_polls, 1);
        assert!(
            queue.unapplied().is_some(),
            "pending state remains available for the next block after stop"
        );
    }

    #[test]
    fn param_only_load_preserves_offthread_publisher_snapshot() {
        let plugin = Arc::new(PluginCell::new(SwapPlugin::create()));
        let params = plugin.enter().params_arc();
        let slot = plugin.enter().snapshot_slot();
        let queue = Arc::new(StateLoadQueue::new(1));
        let backup = ParamsBackup::capture(&*params);

        crate::snapshot::SnapshotPublisher::new(&slot).publish(b"PUB_CUSTOM".to_vec());
        assert_eq!(super::save_extra(&slot), b"PUB_CUSTOM".to_vec());

        // Host-owned param-only load (`extra: None`) must NOT invalidate publisher bytes.
        apply_or_queue_state(
            &plugin,
            &*params,
            &slot,
            &queue,
            DeserializedState {
                extra: None,
                ..Default::default()
            },
            &backup,
            true,
        )
        .expect("param-only host-owned load succeeds");
        assert_eq!(
            custom_state_for_save(&plugin, &slot, &queue, true),
            b"PUB_CUSTOM".to_vec()
        );
        assert_eq!(slot.read_offthread(), Some(b"PUB_CUSTOM".to_vec()));

        // Queued audio-thread param-only load (`extra: None`) also preserves publisher bytes.
        let handle = queue.push_tracked(DeserializedState {
            extra: None,
            ..Default::default()
        });
        {
            let mut guard = plugin.enter();
            assert!(matches!(
                apply_queued_state(&mut *guard, &queue),
                Some(Ok(()))
            ));
        }
        assert_eq!(queue.wait_applied(&handle, Duration::ZERO), Some(true));
        assert_eq!(super::save_extra(&slot), b"PUB_CUSTOM".to_vec());
        assert_eq!(slot.read_offthread(), Some(b"PUB_CUSTOM".to_vec()));
    }

    #[test]
    #[cfg_attr(miri, ignore = "wall-clock latency bound; Miri is ~100x slower")]
    fn queued_load_reuses_original_snapshot_ticket_without_extra_block() {
        let plugin = Arc::new(PluginCell::new(SwapPlugin::create()));
        let params = plugin.enter().params_arc();
        let slot = plugin.enter().snapshot_slot();
        let queue = Arc::new(StateLoadQueue::new(1));
        let backup = ParamsBackup::capture(&*params);

        let (tx_ready, rx_ready) = std::sync::mpsc::channel();
        let (tx_run_one, rx_run_one) = std::sync::mpsc::channel();
        let audio_plugin = Arc::clone(&plugin);
        let audio_queue = Arc::clone(&queue);
        let audio = std::thread::spawn(move || {
            // Stamp active heartbeat, then wait for host to push load and serve
            // ONLY ONE block (applying load + serving its own ticket N), then stop
            // producing blocks without marking inactive.
            audio_queue.record_audio_block();
            let _ = audio_plugin.enter().snapshot_slot().pending_request();
            tx_ready.send(()).unwrap();
            rx_run_one.recv_timeout(Duration::from_secs(2)).unwrap();
            let mut guard = audio_plugin.enter();
            audio_queue.record_audio_block();
            let _ = apply_queued_state(&mut *guard, &audio_queue);
            if let Some(ticket) = guard.snapshot_slot().pending_request() {
                let bytes = guard.save_state();
                guard
                    .snapshot_slot()
                    .serve(ticket, |buf| buf.extend_from_slice(&bytes));
            }
        });
        rx_ready.recv_timeout(Duration::from_secs(2)).unwrap();

        let trigger_queue = Arc::clone(&queue);
        let trigger = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while trigger_queue.unapplied().is_none() {
                assert!(Instant::now() < deadline, "load was never queued");
                std::thread::yield_now();
            }
            tx_run_one.send(()).unwrap();
        });

        let expected = 77u32.to_le_bytes().to_vec();
        let t0 = Instant::now();
        let res = apply_or_queue_state(
            &plugin,
            &*params,
            &slot,
            &queue,
            DeserializedState {
                extra: Some(expected.clone()),
                ..Default::default()
            },
            &backup,
            false,
        );
        let elapsed = t0.elapsed();
        trigger.join().unwrap();
        audio.join().unwrap();

        res.expect("single block applies load and serves its ticket");
        assert_eq!(super::save_extra(&slot), expected);
        assert!(
            elapsed < Duration::from_millis(35),
            "host must reuse load's served ticket N rather than stalling 50ms for N+1, took {elapsed:?}"
        );
    }

    #[test]
    fn host_owned_pre_activation_decoded_load_remains_visible_to_save() {
        // Plugin whose `save_state` returns empty while inactive (like `Lifecycle`).
        struct InactiveDecodedPlugin {
            params: Arc<DynamicParams<1>>,
            meters: Arc<MeterStore>,
            snapshot: Arc<SnapshotSlot>,
            pending_tag: Option<u32>,
            active_tag: Option<u32>,
        }
        impl PluginRuntime for InactiveDecodedPlugin {
            type Sample = f32;
            fn info() -> PluginInfo {
                SwapPlugin::info()
            }
            fn reset(&mut self, _config: &AudioConfig) {}
            fn process(
                &mut self,
                _buffer: &mut AudioBuffer<f32>,
                _events: &EventList,
                _context: &mut ProcessContext,
            ) -> ProcessStatus {
                ProcessStatus::Normal
            }
            fn save_state(&self) -> Vec<u8> {
                self.active_tag
                    .map_or_else(Vec::new, |t| t.to_le_bytes().to_vec())
            }
            fn decode_state(data: &[u8]) -> Result<Option<DecodedState>, StateLoadError> {
                if data.len() == 4 {
                    let tag = u32::from_le_bytes(data.try_into().unwrap());
                    return Ok(Some(DecodedState::new(tag)));
                }
                Ok(None)
            }
            fn apply_decoded_state(&mut self, decoded: &mut DecodedState) {
                if let Some(&tag) = decoded.downcast_ref::<u32>() {
                    self.pending_tag = Some(tag);
                }
            }
        }
        impl PluginExport for InactiveDecodedPlugin {
            type Params = DynamicParams<1>;
            fn create() -> Self {
                Self {
                    params: Arc::new(DynamicParams::<1>::new()),
                    meters: MeterStore::new(),
                    snapshot: SnapshotSlot::new(),
                    pending_tag: None,
                    active_tag: None,
                }
            }
            fn params(&self) -> &DynamicParams<1> {
                &self.params
            }
            fn params_arc(&self) -> Arc<DynamicParams<1>> {
                Arc::clone(&self.params)
            }
            fn meter_store(&self) -> Arc<MeterStore> {
                Arc::clone(&self.meters)
            }
            fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
                Arc::clone(&self.snapshot)
            }
        }

        for can_own in [true, false] {
            let plugin = PluginCell::new(InactiveDecodedPlugin::create());
            let params = plugin.enter().params_arc();
            let slot = plugin.enter().snapshot_slot();
            let queue = StateLoadQueue::new(1);
            let backup = ParamsBackup::capture(&*params);
            let expected = 42u32.to_le_bytes().to_vec();

            // 1. Host-owned pre-activation load (`can_own = true`).
            apply_or_queue_state(
                &plugin,
                &*params,
                &slot,
                &queue,
                DeserializedState {
                    extra: Some(expected.clone()),
                    ..Default::default()
                },
                &backup,
                can_own,
            )
            .expect("pre-activation load succeeds");

            // 2. Save BEFORE activation (both host-owned and non-owned views) sees `expected`.
            assert_eq!(
                custom_state_for_save(&plugin, &slot, &queue, true),
                expected
            );
            assert_eq!(super::custom_state_view(&slot, &queue), expected);

            // 3. Activate consumes pending tag; live save continues returning `expected`.
            {
                let mut guard = plugin.enter();
                assert_eq!(guard.pending_tag.take(), Some(42));
                guard.active_tag = Some(43);
            }
            assert_eq!(
                custom_state_for_save(&plugin, &slot, &queue, true),
                43u32.to_le_bytes()
            );
        }
    }

    #[test]
    fn older_publisher_snapshot_does_not_override_decoded_state_load() {
        let plugin = Arc::new(PluginCell::new(SwapPlugin::create()));
        let slot = plugin.enter().snapshot_slot();
        let queue = Arc::new(StateLoadQueue::new(1));

        slot.store_inline(|buf| buf.push(1));
        // Background publisher publishes A before any load.
        crate::snapshot::SnapshotPublisher::new(&slot).publish(b"OLD_A".to_vec());
        assert_eq!(slot.read(), Some(b"OLD_A".to_vec()));

        // Queued decoded load swaps in B (tag = 99) on the audio thread.
        let expected_b = 99u32.to_le_bytes().to_vec();
        let handle = queue.push_with_backup_and_decoded(
            DeserializedState {
                extra: Some(expected_b.clone()),
                ..Default::default()
            },
            None,
            Some(DecodedState::new(99u32)),
        );
        {
            let mut guard = plugin.enter();
            assert!(matches!(
                apply_queued_state(&mut *guard, &queue),
                Some(Ok(()))
            ));
            assert_eq!(super::custom_state_view(&slot, &queue), expected_b);
            let ticket = guard
                .snapshot_slot()
                .pending_request()
                .expect("fresh requested");
            let bytes = guard.save_state();
            guard
                .snapshot_slot()
                .serve(ticket, |buf| buf.extend_from_slice(&bytes));
        }
        assert_eq!(queue.wait_applied(&handle, Duration::ZERO), Some(true));

        // Active save and off-thread save must both see B, not pre-load publisher A.
        assert_eq!(super::save_extra(&slot), expected_b);
        assert!(slot.read_offthread().is_none());
        assert_eq!(
            crate::export::read_custom_state_offthread(&*plugin.enter()),
            expected_b
        );

        // A new publisher snapshot C belonging to the current generation wins again.
        crate::snapshot::SnapshotPublisher::new(&slot).publish(b"NEW_C".to_vec());
        assert_eq!(super::save_extra(&slot), b"NEW_C".to_vec());
        assert_eq!(slot.read_offthread(), Some(b"NEW_C".to_vec()));
        crate::snapshot::SnapshotPublisher::new(&slot).publish(Vec::new());
        assert_eq!(
            custom_state_for_save(&plugin, &slot, &queue, true),
            Vec::<u8>::new()
        );
        assert_eq!(super::custom_state_view(&slot, &queue), Vec::<u8>::new());
    }

    #[test]
    fn negotiated_block_interval_expands_heartbeat_active_window() {
        let queue = Arc::new(StateLoadQueue::new(1));
        let slot = SnapshotSlot::new();
        // 8,192 frames @ 44.1 kHz ≈ 186 ms (exceeds the 150 ms default window).
        queue.set_block_interval(44_100.0, 8192);
        slot.set_block_interval(44_100.0, 8192);

        // Never processed -> still immediately inactive.
        assert!(!queue.is_audio_active_on_other_thread());
        assert!(!slot.is_audio_active_on_other_thread());

        let q = Arc::clone(&queue);
        let s = Arc::clone(&slot);
        std::thread::spawn(move || {
            crate::state::with_test_time(1_000, || {
                q.record_audio_block();
                let _ = s.pending_request();
            });
        })
        .join()
        .unwrap();

        // 175 ms later (> 150 ms default, < 186 ms + 50 ms margin): still active!
        crate::state::with_test_time(176_000, || {
            assert!(queue.is_audio_active_on_other_thread());
            assert!(slot.is_audio_active_on_other_thread());
        });

        // Explicit stop clears activity immediately.
        queue.mark_audio_inactive();
        slot.mark_audio_inactive();
        assert!(!queue.is_audio_active_on_other_thread());
        assert!(!slot.is_audio_active_on_other_thread());
    }
}

#[cfg(all(test, feature = "loom"))]
mod plugin_cell_loom_tests {
    use super::PluginCell;
    use loom::cell::UnsafeCell;
    use loom::sync::Arc;
    use loom::thread;

    /// Two owners take turns through `enter`. The plugin state is a loom
    /// `UnsafeCell`, so loom reports any interleaving where the handoff
    /// fails to order one owner's writes before the other's access, and the
    /// final count proves neither increment was lost.
    #[test]
    fn loom_handoff_orders_each_owners_writes() {
        loom::model(|| {
            let cell = Arc::new(PluginCell::new(UnsafeCell::new(0u32)));
            let other = Arc::clone(&cell);
            let audio = thread::spawn(move || {
                let guard = other.enter();
                // SAFETY: the guard is the only access to the state.
                guard.with_mut(|n| unsafe { *n += 1 });
            });
            {
                let guard = cell.enter();
                // SAFETY: as above.
                guard.with_mut(|n| unsafe { *n += 1 });
            }
            audio.join().unwrap();
            let guard = cell.enter();
            // SAFETY: both other owners have released.
            assert_eq!(guard.with(|n| unsafe { *n }), 2);
        });
    }

    /// `try_enter` never hands out a second guard while one is live.
    #[test]
    fn loom_try_enter_admits_one_owner() {
        loom::model(|| {
            let cell = Arc::new(PluginCell::new(UnsafeCell::new(0u32)));
            let other = Arc::clone(&cell);
            let audio = thread::spawn(move || {
                if let Some(guard) = other.try_enter() {
                    // SAFETY: the guard is the only access to the state.
                    guard.with_mut(|n| unsafe { *n += 1 });
                }
            });
            if let Some(guard) = cell.try_enter() {
                // SAFETY: as above.
                guard.with_mut(|n| unsafe { *n += 1 });
            }
            audio.join().unwrap();
        });
    }
}
