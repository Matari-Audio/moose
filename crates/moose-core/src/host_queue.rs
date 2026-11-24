//! One bounded main-thread mailbox per plugin instance.
//!
//! Everything a plugin instance has to tell its host "later, on the main
//! thread" goes through one [`HostQueue`]: the editor marking the project
//! dirty or asking for a context menu, the audio thread reporting a latency
//! change, a parameter set asking for a rescan, the plugin's note names
//! changing. The wrapper drains it in its main-thread callback (CLAP
//! `on_main_thread`, the VST3 run-loop handler or timer) and makes the
//! format's calls there.
//!
//! Two lanes, both preallocated:
//!
//! - [`HostSignal`]s coalesce. Raising one that is already pending is a
//!   no-op, so the lane can never overflow.
//! - [`HostMessage`]s carry a payload and keep their order. They sit in a
//!   fixed-capacity ring; [`HostQueue::post`] hands the message
//!   back when the ring is full.
//!
//! Raising a signal or posting a message never allocates. Neither provides
//! a bounded audio deadline: wake registration retries a weak CAS and
//! production `ArrayQueue::push` may wait for a consumer to release a slot. The first one after a drain calls the
//! wrapper's wake once; later ones ride on that wake until the next drain.

use std::fmt;
use std::marker::PhantomData;

// A `loom` test build swaps in loom's atomics and a loom-visible ring, so
// the loom tests below model-check this code rather than a copy of it.
#[cfg(all(test, feature = "loom"))]
use loom::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
#[cfg(not(all(test, feature = "loom")))]
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

#[cfg(not(all(test, feature = "loom")))]
type Ring = crossbeam_queue::ArrayQueue<HostMessage>;
#[cfg(all(test, feature = "loom"))]
type Ring = loom_ring::Ring;

use crate::editor::EditGroup;
use crate::main_thread::{HostNotice, MenuTarget};
use crate::thread::MainThread;

/// Default capacity of the message ring. Messages are user gestures
/// (a context-menu click), so a handful per main-thread callback is the
/// most a host ever sees.
pub const HOST_QUEUE_CAPACITY: usize = 16;

bitflags::bitflags! {
    /// Coalescing "tell the host" requests. Several raises before the next
    /// drain reach the host once.
    ///
    /// The low bits match [`HostNotice`], so a notice converts without a
    /// lookup.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct HostSignal: u32 {
        /// See [`HostNotice::NOTE_NAMES`].
        const NOTE_NAMES = 1 << 0;
        /// See [`HostNotice::VOICE_INFO`].
        const VOICE_INFO = 1 << 1;
        /// See [`HostNotice::AUDIO_PORTS`].
        const AUDIO_PORTS = 1 << 2;
        /// See [`HostNotice::KEYSWITCHES`].
        const KEYSWITCHES = 1 << 3;
        /// The project changed outside the parameters (CLAP
        /// `state.mark_dirty`, VST3 `IComponentHandler2::setDirty`).
        const MARK_DIRTY = 1 << 8;
        /// Deactivate and reactivate the plugin (CLAP `request_restart`,
        /// VST3 `restartComponent`).
        const RESTART = 1 << 9;
        /// The processor's latency moved off what the host was told
        /// (CLAP a restart or `latency.changed` while inactive; VST3
        /// `kLatencyChanged`; AU `kAudioUnitProperty_Latency` / KVO `"latency"`).
        /// See [`crate::delay::ReportedDelay`].
        const LATENCY = 1 << 10;
        /// Parameter values changed without an output event the host
        /// would see (CLAP `params.rescan(VALUES)`).
        const PARAM_RESCAN = 1 << 11;
        /// Parameter names, ranges or text changed (CLAP
        /// `params.rescan(INFO | TEXT)`, VST3 `kParamTitlesChanged`).
        const PRESENTATION = 1 << 12;
        /// A `#[persist]` field changed and wants a host-visible edit.
        const PERSIST = 1 << 13;
        /// The user closed a floating editor window (CLAP `gui.closed`).
        const FLOATING_CLOSED = 1 << 14;
        /// The processor's release tail moved off what the host was told
        /// (CLAP a restart; AU `kAudioUnitProperty_TailTime` / KVO `"tailTime"`).
        /// See [`crate::delay::ReportedDelay`].
        const TAIL = 1 << 15;
        /// Pending edit-group transitions (`clap.undo/4`) waiting to drain on
        /// the host main thread.
        const UNDO_GROUP = 1 << 16;
        /// Audio-thread parameter automation or modulation update requesting a
        /// coalesced wakeup of the open editor's 0 Hz render loop on the main thread.
        const EDITOR_WAKE = 1 << 17;
    }
}

impl From<HostNotice> for HostSignal {
    fn from(notice: HostNotice) -> Self {
        Self::from_bits_truncate(u32::from(notice.bits()))
    }
}

impl HostSignal {
    /// The plugin-facing notices among these signals.
    #[must_use]
    pub fn notices(self) -> HostNotice {
        // The notice bits are the low byte (see the type docs).
        HostNotice::from_bits_truncate((self.bits() & 0xff) as u8)
    }
}

/// A "tell the host" request with a payload, delivered in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostMessage {
    /// Show the host's context menu for `target` at `(x, y)`, in logical
    /// points relative to the editor's top-left corner (CLAP
    /// `context-menu.popup`, VST3 `IComponentHandler3::createContextMenu`).
    ContextMenu { target: MenuTarget, x: i32, y: i32 },
    /// The editor's pointer is hovering `Some(param_id)` or left all controls
    /// (`None`) — CLAP `clap.param-hovered/1`.
    ParamHovered(Option<u32>),
    /// Open or close a grouped edit transaction (CLAP `clap.undo/4`
    /// `begin_change` / `change_made`, VST3 `startGroupEdit` / `finishGroupEdit`).
    EditGroup(EditGroup),
    /// Ask the host to undo the last change (CLAP `clap.undo/4` `request_undo`).
    RequestUndo,
    /// Ask the host to redo the last undone change (CLAP `clap.undo/4` `request_redo`).
    RequestRedo,
    /// Forward an unhandled XKB key event from an embedded/nested Wayland
    /// editor to the host (`clap.gui-xkb-key-forwarding/1`).
    ForwardXkbKey {
        keycode: u32,
        keysym: u32,
        modifiers: u32,
        pressed: bool,
    },
}

/// Set in [`HostQueue::waking`] once [`HostQueue::close`] ran.
const CLOSED_BIT: usize = 1 << (usize::BITS - 1);

/// The per-instance main-thread mailbox. See the module docs.
///
/// # Ordering
///
/// Two handshakes keep it correct under any interleaving (both are
/// model-checked by the `loom` tests at the bottom of this file):
///
/// - Producers publish (a signal bit or a ring slot), then
///   sets the low bit of `armed` with an `AcqRel` RMW. A drain first clears
///   that bit and advances its generation with an `AcqRel` RMW,
///   then reads. Both are read-modify-writes of one atomic, so one reads
///   the other's value: either the drain's comes first and the producer's
///   reads a clear low bit and wakes, or the producer's comes first and the drain's
///   acquires it, so the drain sees what was published. Nothing is
///   stranded with no wake pending. (A plain store in the drain
///   would allow the store-buffering outcome where neither happens.)
/// - The closed flag and the count of running wakes share one word
///   (`waking`), so a wake either registers before [`HostQueue::close`]
///   sets the flag (and `close` waits for it) or sees the flag and never
///   calls the host.
pub struct HostQueue {
    signals: AtomicU32,
    messages: Ring,
    /// A wake was sent and no drain has run since.
    armed: AtomicUsize,
    /// [`CLOSED_BIT`] once closed; the low bits count the wakes running
    /// now, which [`Self::close`] waits for.
    waking: AtomicUsize,
    wake: Box<dyn Fn() + Send + Sync>,
    defer_wake: Option<Box<dyn Fn() -> bool + Send + Sync>>,
}

impl HostQueue {
    /// A queue whose ring holds [`HOST_QUEUE_CAPACITY`] messages.
    ///
    /// `wake` asks the host for a main-thread callback in which the
    /// wrapper calls [`Self::drain`]. It runs on whichever thread raised
    /// the signal, the audio thread included, so it must not lock or
    /// allocate (CLAP `request_callback` is thread-safe; a VST3 wrapper
    /// writes an eventfd or lets its UI timer poll).
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self::with_capacity(HOST_QUEUE_CAPACITY, wake)
    }

    /// A queue whose ring holds `capacity` messages (at least one).
    pub fn with_capacity(capacity: usize, wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            signals: AtomicU32::new(0),
            messages: Ring::new(capacity.max(1)),
            armed: AtomicUsize::new(0),
            waking: AtomicUsize::new(0),
            wake: Box::new(wake),
            defer_wake: None,
        }
    }

    /// Build a queue with a producer-side gate for timer-driven wakeups.
    ///
    /// After publication, `defer_wake` can arrange a later drain and return
    /// true to leave the immediate wake unarmed for other producers.
    pub fn with_deferred_wake(
        wake: impl Fn() + Send + Sync + 'static,
        defer_wake: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        let mut queue = Self::new(wake);
        queue.defer_wake = Some(Box::new(defer_wake));
        queue
    }

    /// Raise `signal`. Allocation-free, any thread, with unbounded wake retries.
    pub fn signal(&self, signal: impl Into<HostSignal>) {
        let signal = signal.into();
        if signal.is_empty() {
            return;
        }
        self.signal_no_wake(signal);
        self.wake_once();
    }

    /// Record `signal` without invoking the immediate wake callback.
    ///
    /// Strictly wait-free (a single atomic RMW) with no OS wake syscall, for
    /// real-time audio threads on formats whose main-thread drain is driven by
    /// a periodic timer (AU, AAX) while non-RT callers still use [`Self::signal`]
    /// for immediate wakes. Because `armed` is left untouched, a subsequent
    /// [`Self::signal`] from another thread still wakes immediately and drains
    /// both.
    pub fn signal_no_wake(&self, signal: impl Into<HostSignal>) {
        let signal = signal.into();
        if signal.is_empty() {
            return;
        }
        self.signals.fetch_or(signal.bits(), Ordering::AcqRel);
    }

    /// Queue `message` behind any posted before it. Allocation-free, any
    /// thread; ring publication and wake registration can retry without a bound.
    ///
    /// # Errors
    ///
    /// Hands `message` back when the ring is full; the host is woken
    /// anyway so it drains.
    pub fn post(&self, message: HostMessage) -> Result<(), HostMessage> {
        let result = self.messages.push(message);
        self.wake_once();
        result
    }

    /// Run `f` while holding this queue open against [`Self::close`].
    ///
    /// Returns `None` once [`Self::close`] has begun. While `f` runs,
    /// [`Self::close`] waits for it to finish before returning, so callers
    /// holding a cloned handle (for example a retained `HostContext` or an
    /// asynchronous main-queue drain) never access instance state freed
    /// after `close`.
    ///
    /// Must not call [`Self::close`] from inside `f`.
    pub fn with_open<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let mut state = self.waking.load(Ordering::Acquire);
        loop {
            if state & CLOSED_BIT != 0 {
                return None;
            }
            match self.waking.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(now) => state = now,
            }
        }
        let _unregister = Unregister(&self.waking);
        Some(f())
    }

    /// Call the wake unless one is already pending. Callers have just
    /// published a signal bit or a message.
    fn wake_once(&self) {
        if self.defer_wake.as_ref().is_some_and(|defer| defer()) {
            return;
        }
        // Pairs with the RMW in `disarm` (see the type docs).
        let claim = self.armed.fetch_or(1, Ordering::AcqRel);
        if claim & 1 != 0 {
            return;
        }
        let disarm_on_panic = DisarmOnPanic {
            armed: &self.armed,
            claim: claim | 1,
            completed: false,
        };
        let _ = self.with_open(|| (self.wake)());
        let mut disarm_on_panic = disarm_on_panic;
        disarm_on_panic.completed = true;
    }

    /// Consume the pending wake before reading what it was for, so a
    /// publish this read misses wakes the host again.
    fn disarm(&self) {
        // An RMW, not a store: it must read the producer's RMW when that
        // came first, to acquire what was published (see the type docs).
        let mut state = self.armed.load(Ordering::Acquire);
        loop {
            match self.armed.compare_exchange(
                state,
                state.wrapping_add(2) & !1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(now) => state = now,
            }
        }
    }

    /// Take everything pending: the raised signals, and the messages in
    /// the order they were posted. A raise or post made while the drain
    /// runs wakes the host again, so nothing is stranded. Main thread.
    pub fn drain(&self, _main: &MainThread) -> Drain<'_> {
        // Disarm before taking, so a raise that lands after the swap
        // below sends a fresh wake.
        self.disarm();
        let signals = HostSignal::from_bits_truncate(self.signals.swap(0, Ordering::AcqRel));
        Drain {
            signals,
            queue: self,
            _not_send: PhantomData,
        }
    }

    /// Take only the signals in `mask`, leaving the rest and the messages
    /// pending. Like [`Self::drain`] it consumes the wake, so call it from
    /// the main-thread callback that wake asked for; anything left pending
    /// wakes the host again.
    pub fn take(&self, mask: HostSignal) -> HostSignal {
        self.disarm();
        let before =
            HostSignal::from_bits_truncate(self.signals.fetch_and(!mask.bits(), Ordering::AcqRel));
        if !(before - mask).is_empty() || !self.messages.is_empty() {
            self.wake_once();
        }
        before & mask
    }

    /// Signals raised and not yet drained.
    #[must_use]
    pub fn pending(&self) -> HostSignal {
        HostSignal::from_bits_truncate(self.signals.load(Ordering::Acquire))
    }

    /// Prevent any future [`Self::with_open`] or wake calls from starting,
    /// without waiting for in-flight registrations yet.
    ///
    /// Pair with [`Self::close`] once the current registration returns (used
    /// when a host destroys an instance re-entrantly from inside a
    /// [`Self::with_open`] drain).
    pub fn begin_close(&self) {
        self.waking.fetch_or(CLOSED_BIT, Ordering::AcqRel);
    }

    /// Stop waking the host, after any wake in flight returns. Wrappers
    /// call this before the instance is freed. Later raises and posts stay
    /// pending but call no wake.
    ///
    /// Must not be called from inside the wake itself (it would wait for
    /// its own return).
    pub fn close(&self) {
        self.begin_close();
        while self.waking.load(Ordering::Acquire) & !CLOSED_BIT != 0 {
            #[cfg(not(all(test, feature = "loom")))]
            std::thread::yield_now();
            #[cfg(all(test, feature = "loom"))]
            loom::thread::yield_now();
        }
    }

    /// Whether [`Self::close`] ran.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.waking.load(Ordering::Acquire) & CLOSED_BIT != 0
    }
}

impl fmt::Debug for HostQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostQueue")
            .field("signals", &self.pending())
            .field("messages", &self.messages.len())
            .field("capacity", &self.messages.capacity())
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

/// Leaves the running-wake count when dropped, so a wake that unwinds
/// can't leave [`HostQueue::close`] spinning forever.
struct Unregister<'a>(&'a AtomicUsize);

impl Drop for Unregister<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Clears `armed` if a wake unwinds before returning, so a caught panic
/// does not permanently silence subsequent signals.
struct DisarmOnPanic<'a> {
    armed: &'a AtomicUsize,
    claim: usize,
    completed: bool,
}

impl Drop for DisarmOnPanic<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.armed.compare_exchange(
                self.claim,
                self.claim & !1,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

/// What one [`HostQueue::drain`] took. Iterating yields the messages;
/// any left when it drops wake the host again.
///
/// `!Send` so the single-consumer drain stays on the main thread.
#[derive(Debug)]
pub struct Drain<'q> {
    /// The signals raised since the last drain.
    pub signals: HostSignal,
    queue: &'q HostQueue,
    _not_send: PhantomData<*const ()>,
}

impl Drain<'_> {
    /// Whether `signal` was raised.
    #[must_use]
    pub fn has(&self, signal: HostSignal) -> bool {
        self.signals.contains(signal)
    }
}

impl Iterator for Drain<'_> {
    type Item = HostMessage;

    fn next(&mut self) -> Option<HostMessage> {
        self.queue.messages.pop()
    }
}

impl Drop for Drain<'_> {
    fn drop(&mut self) {
        if !self.queue.messages.is_empty() {
            // Never let a panicking wake abort an in-progress unwind.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.queue.wake_once();
            }));
        }
    }
}

/// Format-side receiver for a drained [`HostQueue`].
///
/// Every [`HostSignal`] bit and [`HostMessage`] variant maps to a dedicated
/// method. [`dispatch`] delivers all raised signals in deterministic order,
/// calls [`Self::flush_signals`] so wrappers can coalesce format-specific
/// notifications (for example VST3's `restartComponent` bitmask or CLAP's
/// single `request_restart`), and then delivers queued messages in FIFO order.
///
/// All methods default to no-ops so wrappers that lack a concept (such as
/// floating windows on VST3/AU) only implement what applies to them.
pub trait HostSink {
    /// [`HostSignal::PERSIST`]: a `#[persist]` field changed off-host.
    fn persist_changed(&mut self) {}
    /// [`HostSignal::MARK_DIRTY`]: project state changed outside parameters.
    fn mark_dirty(&mut self) {}
    /// [`HostSignal::FLOATING_CLOSED`]: user closed a floating editor window.
    fn floating_closed(&mut self) {}
    /// [`HostSignal::PARAM_RESCAN`]: parameter values changed internally.
    fn param_values_changed(&mut self) {}
    /// [`HostSignal::PRESENTATION`]: parameter metadata or catalog changed.
    fn presentation_changed(&mut self) {}
    /// [`HostSignal::NOTE_NAMES`]: note-name table changed.
    fn note_names_changed(&mut self) {}
    /// [`HostSignal::VOICE_INFO`]: voice count or polyphony info changed.
    fn voice_info_changed(&mut self) {}
    /// [`HostSignal::AUDIO_PORTS`]: audio port layout or configuration changed.
    fn audio_ports_changed(&mut self) {}
    /// [`HostSignal::KEYSWITCHES`]: keyswitch table changed (VST3 `kKeyswitchChanged`).
    fn keyswitches_changed(&mut self) {}
    /// [`HostSignal::LATENCY`]: processor latency moved off the reported value.
    fn latency_changed(&mut self) {}
    /// [`HostSignal::TAIL`]: processor tail moved off the reported value.
    fn tail_changed(&mut self) {}
    /// [`HostSignal::RESTART`]: plugin requested host deactivate/reactivate.
    fn request_restart(&mut self) {}
    /// [`HostSignal::UNDO_GROUP`]: pending edit-group / undo transitions are ready.
    fn undo_groups_ready(&mut self) {}
    /// [`HostSignal::EDITOR_WAKE`]: coalesced wakeup for the open editor's UI loop.
    fn editor_wake(&mut self) {}
    /// Called once after all raised signals have been delivered and before
    /// queued [`HostMessage`]s are processed.
    fn flush_signals(&mut self) {}
    /// [`HostMessage::ContextMenu`]: show the host's context menu at logical `(x, y)`.
    fn context_menu(&mut self, _target: MenuTarget, _x: i32, _y: i32) {}
    /// [`HostMessage::ParamHovered`]: pointer hover moved onto `Some(id)` or left (`None`).
    fn param_hovered(&mut self, _id: Option<u32>) {}
    /// [`HostMessage::EditGroup`]: grouped undo edit boundary.
    fn edit_group(&mut self, _edge: EditGroup) {}
    /// [`HostMessage::RequestUndo`]: ask the host to undo.
    fn request_undo(&mut self) {}
    /// [`HostMessage::RequestRedo`]: ask the host to redo.
    fn request_redo(&mut self) {}
    /// [`HostMessage::ForwardXkbKey`]: forward an unhandled XKB key event to the host.
    fn forward_xkb_key(&mut self, _keycode: u32, _keysym: u32, _modifiers: u32, _pressed: bool) {}
}

/// Deliver every raised signal and queued message in `drain` to `sink`.
///
/// Signals are visited in fixed order, followed by [`HostSink::flush_signals`],
/// followed by each queued [`HostMessage`] in FIFO order.
pub fn dispatch(drain: Drain<'_>, sink: &mut dyn HostSink) {
    let signals = drain.signals;
    if signals.contains(HostSignal::PERSIST) {
        sink.persist_changed();
    }
    if signals.contains(HostSignal::MARK_DIRTY) {
        sink.mark_dirty();
    }
    if signals.contains(HostSignal::FLOATING_CLOSED) {
        sink.floating_closed();
    }
    if signals.contains(HostSignal::PARAM_RESCAN) {
        sink.param_values_changed();
    }
    if signals.contains(HostSignal::PRESENTATION) {
        sink.presentation_changed();
    }
    if signals.contains(HostSignal::NOTE_NAMES) {
        sink.note_names_changed();
    }
    if signals.contains(HostSignal::VOICE_INFO) {
        sink.voice_info_changed();
    }
    if signals.contains(HostSignal::AUDIO_PORTS) {
        sink.audio_ports_changed();
    }
    if signals.contains(HostSignal::KEYSWITCHES) {
        sink.keyswitches_changed();
    }
    if signals.contains(HostSignal::LATENCY) {
        sink.latency_changed();
    }
    if signals.contains(HostSignal::TAIL) {
        sink.tail_changed();
    }
    if signals.contains(HostSignal::RESTART) {
        sink.request_restart();
    }
    if signals.contains(HostSignal::UNDO_GROUP) {
        sink.undo_groups_ready();
    }
    if signals.contains(HostSignal::EDITOR_WAKE) {
        sink.editor_wake();
    }
    sink.flush_signals();
    for message in drain {
        match message {
            HostMessage::ContextMenu { target, x, y } => sink.context_menu(target, x, y),
            HostMessage::ParamHovered(id) => sink.param_hovered(id),
            HostMessage::EditGroup(edge) => sink.edit_group(edge),
            HostMessage::RequestUndo => sink.request_undo(),
            HostMessage::RequestRedo => sink.request_redo(),
            HostMessage::ForwardXkbKey {
                keycode,
                keysym,
                modifiers,
                pressed,
            } => sink.forward_xkb_key(keycode, keysym, modifiers, pressed),
        }
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn counting() -> (HostQueue, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let queue = HostQueue::with_capacity(2, {
            let wakes = Arc::clone(&wakes);
            move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }
        });
        (queue, wakes)
    }

    fn main() -> MainThread {
        // SAFETY: tests stand in for the host's main thread.
        unsafe { MainThread::assume() }
    }

    #[test]
    fn signals_coalesce_into_one_wake_per_drain() {
        let (queue, wakes) = counting();
        queue.signal(HostSignal::MARK_DIRTY);
        queue.signal(HostSignal::MARK_DIRTY | HostSignal::LATENCY);
        queue.signal(HostNotice::VOICE_INFO);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);

        let drain = queue.drain(&main());
        assert_eq!(
            drain.signals,
            HostSignal::MARK_DIRTY | HostSignal::LATENCY | HostSignal::VOICE_INFO
        );
        assert_eq!(drain.signals.notices(), HostNotice::VOICE_INFO);
        assert_eq!(drain.count(), 0);

        queue.signal(HostSignal::RESTART);
        assert_eq!(wakes.load(Ordering::Relaxed), 2, "a drain re-arms the wake");
    }

    #[test]
    fn messages_keep_order_and_a_full_ring_hands_them_back() {
        let (queue, _) = counting();
        let menu = |x| HostMessage::ContextMenu {
            target: MenuTarget::Param(7),
            x,
            y: 0,
        };
        assert_eq!(queue.post(menu(1)), Ok(()));
        assert_eq!(queue.post(menu(2)), Ok(()));
        assert_eq!(queue.post(menu(3)), Err(menu(3)));
        let got: Vec<_> = queue.drain(&main()).collect();
        assert_eq!(got, [menu(1), menu(2)]);
    }

    #[test]
    fn messages_left_in_a_dropped_drain_wake_the_host_again() {
        let (queue, wakes) = counting();
        let menu = HostMessage::ContextMenu {
            target: MenuTarget::Global,
            x: 0,
            y: 0,
        };
        queue.post(menu).expect("room");
        queue.post(menu).expect("room");
        let mut drain = queue.drain(&main());
        assert_eq!(drain.next(), Some(menu));
        drop(drain);
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        assert_eq!(queue.drain(&main()).count(), 1);
    }

    #[test]
    fn take_leaves_other_signals_pending() {
        let (queue, wakes) = counting();
        queue.signal(HostSignal::NOTE_NAMES | HostSignal::PERSIST);
        assert_eq!(
            queue.take(HostSignal::NOTE_NAMES | HostSignal::VOICE_INFO),
            HostSignal::NOTE_NAMES
        );
        assert_eq!(queue.pending(), HostSignal::PERSIST);
        assert_eq!(wakes.load(Ordering::Relaxed), 2, "what is left wakes again");
        assert_eq!(queue.take(HostSignal::PERSIST), HostSignal::PERSIST);
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        queue.signal(HostSignal::RESTART);
        assert_eq!(wakes.load(Ordering::Relaxed), 3, "take re-arms the wake");
    }

    #[test]
    fn a_closed_queue_keeps_signals_but_never_wakes() {
        let (queue, wakes) = counting();
        queue.close();
        queue.signal(HostSignal::RESTART);
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        assert_eq!(queue.drain(&main()).signals, HostSignal::RESTART);
    }

    #[test]
    fn a_deferred_producer_leaves_the_immediate_wake_available() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

        let defer = Arc::new(AtomicBool::new(true));
        let wakes = Arc::new(AtomicU32::new(0));
        let queue = HostQueue::with_deferred_wake(
            {
                let wakes = Arc::clone(&wakes);
                move || {
                    wakes.fetch_add(1, Ordering::Relaxed);
                }
            },
            {
                let defer = Arc::clone(&defer);
                move || defer.load(Ordering::Relaxed)
            },
        );
        queue.signal(HostSignal::LATENCY);
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        defer.store(false, Ordering::Relaxed);
        queue.signal(HostSignal::PRESENTATION);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(
            queue.pending(),
            HostSignal::LATENCY | HostSignal::PRESENTATION
        );
    }

    #[test]
    fn signal_no_wake_records_without_waking_or_blocking_later_wakes() {
        let (queue, wakes) = counting();
        queue.signal_no_wake(HostSignal::LATENCY | HostSignal::TAIL);
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        assert_eq!(queue.pending(), HostSignal::LATENCY | HostSignal::TAIL);
        // Because `armed` stayed false, a non-RT `signal` still wakes once
        // and the drain picks up all pending bits together.
        queue.signal(HostSignal::MARK_DIRTY);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(
            queue.drain(&main()).signals,
            HostSignal::LATENCY | HostSignal::TAIL | HostSignal::MARK_DIRTY
        );
    }

    #[test]
    fn a_panicking_wake_still_lets_close_return_and_rearms() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let queue = HostQueue::new({
            let attempts = Arc::clone(&attempts);
            move || {
                assert!(
                    attempts.fetch_add(1, Ordering::Relaxed) != 0,
                    "transient wake panic"
                );
            }
        });
        let raised = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            queue.signal(HostSignal::RESTART);
        }));
        assert!(raised.is_err());
        // A later signal must still attempt to wake rather than staying stuck armed.
        queue.signal(HostSignal::MARK_DIRTY);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        assert_eq!(queue.with_open(|| 42), Some(42));
        queue.close();
        assert!(queue.is_closed());
        assert_eq!(queue.with_open(|| 42), None);
    }

    #[test]
    fn failed_wake_cannot_clear_a_newer_claim() {
        let armed = AtomicUsize::new(1);
        let guard = DisarmOnPanic {
            armed: &armed,
            claim: 1,
            completed: false,
        };
        // A drain advances the generation, then another producer arms it.
        armed.store(3, Ordering::Release);
        drop(guard);
        assert_eq!(armed.load(Ordering::Acquire), 3);
    }

    #[test]
    fn raises_from_many_threads_are_all_delivered() {
        let (queue, wakes) = counting();
        let queue = Arc::new(queue);
        let bits = [
            HostSignal::MARK_DIRTY,
            HostSignal::RESTART,
            HostSignal::LATENCY,
            HostSignal::PRESENTATION,
        ];
        std::thread::scope(|s| {
            for bit in bits {
                let queue = Arc::clone(&queue);
                s.spawn(move || {
                    for _ in 0..1000 {
                        queue.signal(bit);
                    }
                });
            }
        });
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(
            queue.drain(&main()).signals,
            bits.into_iter().fold(HostSignal::empty(), |a, b| a | b)
        );
    }

    #[test]
    fn dispatch_maps_every_host_signal_bit_distinctly_and_orders_flush_before_messages() {
        #[derive(Default)]
        struct Recorder {
            calls: Vec<&'static str>,
            menus: Vec<(MenuTarget, i32, i32)>,
        }
        impl HostSink for Recorder {
            fn persist_changed(&mut self) {
                self.calls.push("persist_changed");
            }
            fn mark_dirty(&mut self) {
                self.calls.push("mark_dirty");
            }
            fn floating_closed(&mut self) {
                self.calls.push("floating_closed");
            }
            fn param_values_changed(&mut self) {
                self.calls.push("param_values_changed");
            }
            fn presentation_changed(&mut self) {
                self.calls.push("presentation_changed");
            }
            fn note_names_changed(&mut self) {
                self.calls.push("note_names_changed");
            }
            fn voice_info_changed(&mut self) {
                self.calls.push("voice_info_changed");
            }
            fn audio_ports_changed(&mut self) {
                self.calls.push("audio_ports_changed");
            }
            fn keyswitches_changed(&mut self) {
                self.calls.push("keyswitches_changed");
            }
            fn latency_changed(&mut self) {
                self.calls.push("latency_changed");
            }
            fn tail_changed(&mut self) {
                self.calls.push("tail_changed");
            }
            fn request_restart(&mut self) {
                self.calls.push("request_restart");
            }
            fn undo_groups_ready(&mut self) {
                self.calls.push("undo_groups_ready");
            }
            fn editor_wake(&mut self) {
                self.calls.push("editor_wake");
            }
            fn flush_signals(&mut self) {
                self.calls.push("flush_signals");
            }
            fn context_menu(&mut self, target: MenuTarget, x: i32, y: i32) {
                self.calls.push("context_menu");
                self.menus.push((target, x, y));
            }
        }

        let mut seen = std::collections::HashSet::new();
        for bit in HostSignal::all().iter() {
            let (queue, _) = counting();
            queue.signal(bit);
            let mut rec = Recorder::default();
            dispatch(queue.drain(&main()), &mut rec);
            assert_eq!(
                rec.calls.len(),
                2,
                "signal bit {bit:?} must invoke exactly one HostSink signal method plus flush_signals, got {:?}",
                rec.calls
            );
            assert_eq!(rec.calls[1], "flush_signals");
            assert!(
                seen.insert(rec.calls[0]),
                "signal bit {bit:?} collided on HostSink method {}",
                rec.calls[0]
            );
        }

        // Verify ordering: signals -> flush_signals -> FIFO messages.
        let (queue, _) = counting();
        queue.signal(HostSignal::MARK_DIRTY | HostSignal::LATENCY);
        queue
            .post(HostMessage::ContextMenu {
                target: MenuTarget::Param(4),
                x: 10,
                y: 20,
            })
            .unwrap();
        queue
            .post(HostMessage::ContextMenu {
                target: MenuTarget::Global,
                x: 30,
                y: 40,
            })
            .unwrap();
        let mut rec = Recorder::default();
        dispatch(queue.drain(&main()), &mut rec);
        assert_eq!(
            rec.calls,
            [
                "mark_dirty",
                "latency_changed",
                "flush_signals",
                "context_menu",
                "context_menu",
            ]
        );
        assert_eq!(
            rec.menus,
            [(MenuTarget::Param(4), 10, 20), (MenuTarget::Global, 30, 40)]
        );
    }
}

/// The message ring under `loom`. loom can't see into crossbeam's
/// `ArrayQueue`, whose internal `SeqCst` operations could also mask a
/// weak handshake in [`HostQueue`]. This multi-producer, single-consumer
/// ring publishes a slot with `Release` and observes it with `Acquire`,
/// the weakest orderings a correct ring can use, so the `loom` tests prove
/// the queue's own handshake rather than the ring's.
#[cfg(all(test, feature = "loom"))]
mod loom_ring {
    use super::HostMessage;
    use loom::cell::UnsafeCell;
    use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    pub(super) struct Ring {
        /// `(ready, message)`; `ready` publishes the message.
        slots: Box<[(AtomicBool, UnsafeCell<Option<HostMessage>>)]>,
        /// Next slot to pop. Written by the one consumer.
        head: AtomicUsize,
        /// Next slot to reserve.
        tail: AtomicUsize,
    }

    // SAFETY: a slot's cell is written only by the producer that reserved
    // it and read only by the one consumer after `ready` published it.
    unsafe impl Sync for Ring {}

    impl Ring {
        pub(super) fn new(capacity: usize) -> Self {
            Self {
                slots: (0..capacity)
                    .map(|_| (AtomicBool::new(false), UnsafeCell::new(None)))
                    .collect(),
                head: AtomicUsize::new(0),
                tail: AtomicUsize::new(0),
            }
        }

        pub(super) fn capacity(&self) -> usize {
            self.slots.len()
        }

        pub(super) fn push(&self, message: HostMessage) -> Result<(), HostMessage> {
            let mut tail = self.tail.load(Ordering::Relaxed);
            loop {
                // `Acquire` pairs with the consumer's `head` store, so the
                // slot it emptied is free to write. If `tail` is stale behind
                // `head` (another producer pushed and the consumer popped),
                // refresh `tail` instead of underflowing.
                let head = self.head.load(Ordering::Acquire);
                if tail < head {
                    tail = self.tail.load(Ordering::Relaxed);
                    continue;
                }
                if tail - head >= self.capacity() {
                    return Err(message);
                }
                match self.tail.compare_exchange(
                    tail,
                    tail + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(now) => tail = now,
                }
            }
            let (ready, cell) = &self.slots[tail % self.capacity()];
            // SAFETY: the reservation above makes this slot ours alone.
            cell.with_mut(|slot| unsafe { *slot = Some(message) });
            ready.store(true, Ordering::Release);
            Ok(())
        }

        /// Single consumer.
        pub(super) fn pop(&self) -> Option<HostMessage> {
            let head = self.head.load(Ordering::Relaxed);
            let (ready, cell) = &self.slots[head % self.capacity()];
            if !ready.load(Ordering::Acquire) {
                return None;
            }
            // SAFETY: `ready` published the slot and only the consumer pops.
            let message = cell.with_mut(|slot| unsafe { (*slot).take() });
            ready.store(false, Ordering::Relaxed);
            self.head.store(head + 1, Ordering::Release);
            message
        }

        /// Single consumer.
        pub(super) fn is_empty(&self) -> bool {
            let head = self.head.load(Ordering::Relaxed);
            !self.slots[head % self.capacity()].0.load(Ordering::Acquire)
        }

        pub(super) fn len(&self) -> usize {
            self.tail
                .load(Ordering::Acquire)
                .saturating_sub(self.head.load(Ordering::Acquire))
                .min(self.capacity())
        }
    }
}

// Model-checked proof of the two handshakes in the [`HostQueue`] docs. Run
// with `cargo test -p moose-core --features loom --release --lib loom`.
// Going back to a plain `store(false)` in `disarm` or to a separate
// `closed` flag makes loom find the failing interleaving.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::cell::UnsafeCell;
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicUsize, Ordering};
    use loom::thread;

    use super::*;

    const MENU: HostMessage = HostMessage::ContextMenu {
        target: MenuTarget::Global,
        x: 0,
        y: 0,
    };

    /// The state a host keeps for its wake.
    struct Host(UnsafeCell<u32>);

    // SAFETY: loom checks every access to the cell.
    unsafe impl Sync for Host {}

    fn main() -> MainThread {
        // SAFETY: the model's main thread stands in for the host's.
        unsafe { MainThread::assume() }
    }

    /// A queue whose wake counts, with one wake already sent: the drain
    /// under test answers it.
    fn armed() -> (Arc<HostQueue>, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let queue = Arc::new(HostQueue::with_capacity(2, {
            let wakes = Arc::clone(&wakes);
            move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }
        }));
        queue.signal(HostSignal::MARK_DIRTY);
        (queue, wakes)
    }

    #[test]
    fn loom_close_returns_only_after_a_running_wake() {
        loom::model(|| {
            // The host the wake calls into. Writing it after `close` stands
            // for the wrapper freeing it; loom reports a data race if a wake
            // can still be touching it.
            let host = Arc::new(Host(UnsafeCell::new(0)));
            let queue = Arc::new(HostQueue::new({
                let host = Arc::clone(&host);
                // SAFETY: loom checks this access against the free below.
                move || host.0.with_mut(|count| unsafe { *count += 1 })
            }));
            let raiser = thread::spawn({
                let queue = Arc::clone(&queue);
                move || queue.signal(HostSignal::RESTART)
            });
            queue.close();
            // SAFETY: as above.
            host.0.with_mut(|count| unsafe { *count = u32::MAX });
            raiser.join().unwrap();
            assert!(queue.is_closed());
        });
    }

    #[test]
    fn loom_a_post_racing_a_drain_is_taken_or_wakes_again() {
        loom::model(|| {
            let (queue, wakes) = armed();
            let poster = thread::spawn({
                let queue = Arc::clone(&queue);
                move || queue.post(MENU).unwrap()
            });
            let taken = queue.drain(&main()).count();
            poster.join().unwrap();
            assert!(
                taken == 1 || wakes.load(Ordering::Relaxed) == 2,
                "a message was stranded with no wake pending"
            );
        });
    }

    #[test]
    fn loom_a_post_racing_a_take_wakes_again() {
        loom::model(|| {
            let (queue, wakes) = armed();
            let poster = thread::spawn({
                let queue = Arc::clone(&queue);
                move || queue.post(MENU).unwrap()
            });
            assert_eq!(queue.take(HostSignal::MARK_DIRTY), HostSignal::MARK_DIRTY);
            poster.join().unwrap();
            assert_eq!(queue.messages.len(), 1);
            assert_eq!(
                wakes.load(Ordering::Relaxed),
                2,
                "a message was stranded with no wake pending"
            );
        });
    }

    #[test]
    fn loom_a_signal_racing_a_drain_is_taken_or_wakes_again() {
        loom::model(|| {
            let (queue, wakes) = armed();
            let raiser = thread::spawn({
                let queue = Arc::clone(&queue);
                move || queue.signal(HostSignal::RESTART)
            });
            let taken = queue.drain(&main()).signals;
            raiser.join().unwrap();
            assert!(
                taken.contains(HostSignal::RESTART) || wakes.load(Ordering::Relaxed) == 2,
                "a signal was stranded with no wake pending"
            );
        });
    }
}
