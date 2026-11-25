//! The single-component VST3 object: `IComponent` + `IAudioProcessor` +
//! `IEditController` and friends on one COM identity.
//!
//! [`Host`] is the per-instance state, shared with plug views, editor
//! calls (through the registry) and run-loop handlers. [`Component`] is
//! the COM shell the host holds.

use std::cell::{Cell, UnsafeCell};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use moose_core::editor::EditGroup;
use moose_core::main_thread::{MenuItem, MenuTarget};
use moose_params::CatalogChange;
use vst3::Steinberg::Vst::ChannelContext::{
    IInfoListener, IInfoListenerTrait, kChannelColorKey, kChannelNameKey,
};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper};

use super::view::View;
use super::{Registration, cstr, retain, utf16_from, utf16_into};
use crate::backend::{
    ProcessArgs, Vst3Backend, Vst3LearnSource, Vst3NativeEvent, Vst3TrackInfo, Vst3Transport,
};

/// A wake's drain makes at most this many host calls; the rest wait for the
/// next wake, so a burst never holds the UI thread.
pub(super) const DRAIN_EDITS: usize = 64;
/// A detach, handler change or teardown drains up to this many before it
/// goes on, so a finite burst finishes on the handler that saw it begin.
pub(super) const TRANSITION_EDITS: usize = 4096;

const OUTPUT_END: u32 = 0;
const OUTPUT_EMITTED: u32 = 1;
const OUTPUT_UNSUPPORTED: u32 = 2;
const OUTPUT_INVALID: u32 = 3;
const OUTPUT_QUEUE_FULL: u32 = 4;

const NOTE_ON: u32 = Event_::EventTypes_::kNoteOnEvent as u32;
const NOTE_OFF: u32 = Event_::EventTypes_::kNoteOffEvent as u32;
const DATA: u32 = Event_::EventTypes_::kDataEvent as u32;
const POLY_PRESSURE: u32 = Event_::EventTypes_::kPolyPressureEvent as u32;
const NOTE_EXPRESSION: u32 = Event_::EventTypes_::kNoteExpressionValueEvent as u32;
const LEGACY_CC_OUT: u32 = Event_::EventTypes_::kLegacyMIDICCOutEvent as u32;

/// An editor call to the host, queued when made off the UI thread.
#[derive(Clone, Copy, Debug)]
pub(crate) enum EditOp {
    Begin(u32),
    Perform(u32, f64),
    End(u32),
    Resize(u32, u32),
    /// `IComponentHandler2::setDirty(true)`: custom state changed.
    Dirty,
    /// `IComponentHandler2::startGroupEdit` / `finishGroupEdit`.
    Group(EditGroup),
}

/// How a host takes a changed parameter list. Param ids never change in
/// either mode, and the processor accepts every id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DynamicSupport {
    /// The host re-reads `getParameterCount` after `kReloadComponent`, so
    /// only available parameters are listed.
    Reload,
    /// The full list stays; unavailable slots are hidden.
    SlotPool,
}

impl DynamicSupport {
    /// Hosts that reload the parameter list on `kReloadComponent`.
    /// Unverified: Steinberg's own hosts, assumed from the SDK docs, not
    /// tested one by one.
    const RELOAD_HOSTS: [&str; 2] = ["Cubase", "Nuendo"];

    /// The mode for the host named `name` (`IHostApplication::getName`).
    pub(super) fn for_host(name: &str) -> Self {
        if Self::RELOAD_HOSTS.iter().any(|host| name.contains(host)) {
            Self::Reload
        } else {
            Self::SlotPool
        }
    }

    /// The `restartComponent` flags for a taken catalog change.
    pub(super) fn restart_flags(self, changes: CatalogChange) -> i32 {
        use RestartFlags_::*;
        let mut flags = 0;
        if changes.contains(CatalogChange::VALUES) {
            flags |= kParamValuesChanged as i32;
        }
        if changes.contains(CatalogChange::INFO) {
            flags |= kParamTitlesChanged as i32;
        }
        if changes.contains(CatalogChange::STRUCTURE) {
            flags |= match self {
                Self::Reload => kReloadComponent as i32,
                Self::SlotPool => (kParamTitlesChanged | kParamValuesChanged) as i32,
            };
        }
        flags
    }
}

/// What `getParameterCount` / `getParameterInfo` list.
enum ParamList {
    /// [`DynamicSupport::SlotPool`]: every registered parameter.
    All,
    /// [`DynamicSupport::Reload`]: indices into `Registration::params` of
    /// the available ones.
    Available(Vec<usize>),
}

/// What [`Host::close_open_gestures`] closed on the old handler.
struct OpenEdits {
    gestures: Vec<u32>,
    group: bool,
}

#[derive(Default)]
struct Edits {
    pending: VecDeque<EditOp>,
    /// Gestures begun on the current handler and not yet ended.
    open_gestures: Vec<u32>,
    /// A group edit started on the current handler and not yet finished.
    group_open: bool,
    /// Teardown: no more queueing.
    closed: bool,
    /// A drain is making calls with the lock released; callbacks may
    /// reenter and must queue behind it.
    draining: bool,
    /// Linux: the eventfd on the plug frame's run loop, and the thread
    /// that run loop belongs to.
    #[cfg(target_os = "linux")]
    wake: Option<super::wake::linux::FdWake>,
    #[cfg(target_os = "linux")]
    ui_thread: Option<std::thread::ThreadId>,
}

/// Processing configuration. Written by `setupProcessing` and
/// `setBusArrangements`, which VST3 only allows while inactive; read by
/// `process` and the bus queries.
struct Config {
    sample_rate: f64,
    max_frames: u32,
    sample_size: i32,
    process_mode: i32,
    layout: u32,
    total_in: u32,
    total_out: u32,
    in_bus_channels: Vec<u32>,
    out_bus_channels: Vec<u32>,
}

/// Audio-thread scratch, sized from the widest declared layout and the
/// host's maximum block size.
struct Scratch {
    in_ptrs: Vec<*const c_void>,
    out_ptrs: Vec<*mut c_void>,
    /// Shared silence for missing input channels (one block of `f64`, so
    /// it covers both sample widths).
    silence: Vec<f64>,
    /// One discard block per output channel for missing outputs, so no two
    /// output slices alias.
    trash: Vec<f64>,
    /// Per-queue state for merging the host's parameter queues with its
    /// note events. Capacity is fixed at construction; queues past it are
    /// ingested after the merge (correct, but the list then needs a sort).
    param_cursors: Vec<ParamCursor>,
    /// Min-heap of `(next point offset, cursor index)`: at most one entry
    /// per cursor, so it never grows past its construction capacity.
    param_heap: BinaryHeap<Reverse<(i32, u32)>>,
}

/// Room for this many host queues beyond the declared parameter count
/// (hosts may split one parameter across several queues).
const PARAM_CURSOR_SLACK: usize = 32;

/// One host parameter queue being walked point by point.
struct ParamCursor {
    queue: *mut IParamValueQueue,
    id: u32,
    count: i32,
    /// Index of the next point to read.
    next: i32,
    /// The parameter's value at block start.
    prev: f64,
    /// The previous point (offset, plain), or `(0, prev)`; the start of the
    /// segment the next point ends.
    from: (i32, f64),
    /// The read-but-not-yet-delivered point (offset, normalized value).
    pending: (i32, f64),
    /// The last delivered plain value.
    last: Option<f64>,
    /// Every point so far reached the plugin's event list.
    pushed_all: bool,
}

impl ParamCursor {
    /// A cursor over `queue`, or `None` when the host gave a null queue.
    unsafe fn new(host: &Host, queue: *mut IParamValueQueue, will_process: bool) -> Option<Self> {
        // SAFETY: The caller passes `queue` from the host's `IParameterChanges` for this `process`
        // call: null (handled by `from_raw`) or a live queue for the call's duration.
        let q = unsafe { ComRef::from_raw(queue) }?;
        // SAFETY: `q` is the live host queue checked just above.
        let id = unsafe { q.getParameterId() };
        let prev = host.backend().param_get_value(id);
        Some(Self {
            queue,
            id,
            // SAFETY: `q` is the live host queue checked just above.
            count: unsafe { q.getPointCount() },
            next: 0,
            prev,
            from: (0, prev),
            pending: (0, 0.0),
            last: None,
            pushed_all: will_process,
        })
    }

    /// Read the next valid point into `pending`; false when exhausted.
    /// Points the host refuses to return are skipped.
    unsafe fn advance(&mut self) -> bool {
        // SAFETY: `self.queue` was non-null at construction and the host keeps it alive for the
        // rest of this `process` call, which is the cursor's whole lifetime.
        let Some(q) = (unsafe { ComRef::from_raw(self.queue) }) else {
            return false;
        };
        while self.next < self.count {
            let j = self.next;
            self.next += 1;
            let (mut offset, mut value) = (0, 0.0);
            // SAFETY: `q` is the live host queue; `j < count` and both out-pointers are locals.
            if unsafe { q.getPoint(j, &raw mut offset, &raw mut value) } == kResultOk {
                self.pending = (offset, value);
                return true;
            }
        }
        false
    }

    /// Deliver `pending` to the plugin. Returns true when the event list
    /// refused it (the queue's final value is then committed directly).
    fn emit(&mut self, host: &Host) -> bool {
        let backend = host.backend();
        let (offset, value) = self.pending;
        let plain = backend.param_denormalize(self.id, value);
        // The first point ramps from an implicit point at sample -1 holding
        // the last value (VST3 SDK), so sample 0 is already one step along.
        if self.last.is_none() {
            self.from.1 = self.prev + (plain - self.prev) / (f64::from(offset) + 1.0);
        }
        self.last = Some(plain);
        let mut dropped = false;
        if self.pushed_all
            && !backend.push_param_change(self.id, offset, plain, self.from.0, self.from.1)
        {
            self.pushed_all = false;
            dropped = true;
        }
        self.from = (offset, plain);
        dropped
    }

    /// Commit the final value when no sub-block apply will, and tell the
    /// edit controller the parameter moved.
    fn finish(&self, host: &Host) {
        let backend = host.backend();
        if let Some(plain) = self.last
            && !(self.pushed_all && backend.param_is_chunked(self.id))
        {
            backend.param_set_value(self.id, plain);
        }
        if self.last.is_some() {
            host.note_param_change(self.id);
        }
    }

    /// Walk the whole queue at once (no merge). Returns true on overflow.
    unsafe fn drain(&mut self, host: &Host) -> bool {
        let mut dropped = false;
        // SAFETY: Same contract as `advance`: the queue outlives this `process` call.
        while unsafe { self.advance() } {
            dropped |= self.emit(host);
        }
        self.finish(host);
        dropped
    }
}

/// The host's input event list, read one event ahead for the merge.
struct NoteCursor<'a> {
    list: Option<ComRef<'a, IEventList>>,
    count: i32,
    next: i32,
    /// The next event, converted, plus any borrowed `SysEx` payload slice valid
    /// for the duration of this `process` call (`'a`). `None` once the list is
    /// exhausted, the host refused an event, or the plugin's queue filled.
    pending: Option<(Vst3NativeEvent, Option<&'a [u8]>)>,
}

impl<'a> NoteCursor<'a> {
    unsafe fn new(list: *mut IEventList) -> Self {
        // SAFETY: The caller passes the host's `IEventList` for this `process` call: null (handled
        // by `from_raw`) or live for the call's duration (`'a`).
        let list = unsafe { ComRef::from_raw(list) };
        let mut cursor = Self {
            // SAFETY: `l` is the live host event list.
            count: list.map_or(0, |l| unsafe { l.getEventCount() }),
            list,
            next: 0,
            pending: None,
        };
        // SAFETY: `list` is null or live for `'a`, which is `advance`'s contract.
        unsafe { cursor.advance() };
        cursor
    }

    /// Read the next event into `pending`. A refused `getEvent` ends the
    /// stream, so the plugin sees one in-order prefix.
    unsafe fn advance(&mut self) {
        self.pending = None;
        let Some(list) = self.list else {
            return;
        };
        if self.next >= self.count {
            return;
        }
        let i = self.next;
        self.next += 1;
        // SAFETY: `Event` is a plain C struct/union for which all-zero bytes are a valid value.
        let mut ev: Event = unsafe { std::mem::zeroed() };
        // SAFETY: `list` is live for `'a`, `i < count`, and `ev` is a local out slot.
        if unsafe { list.getEvent(i, &raw mut ev) } != kResultOk {
            self.count = self.next;
            return;
        }
        let mut n = Vst3NativeEvent {
            kind: u32::from(ev.r#type),
            sample_offset: ev.sampleOffset,
            bus_index: ev.busIndex,
            flags: u32::from(ev.flags),
            ppq_position: ev.ppqPosition,
            note_id: -1,
            ..Vst3NativeEvent::default()
        };
        let mut data: Option<&'a [u8]> = None;
        // SAFETY: Each union arm read matches the event `type` the host set (`n.kind`), which is
        // the SDK's tagging rule; the `DataEvent` byte slice is justified inline.
        unsafe {
            match n.kind {
                NOTE_ON => {
                    let e = ev.__field0.noteOn;
                    n.channel = e.channel;
                    n.pitch = e.pitch;
                    n.note_id = e.noteId;
                    n.length = e.length;
                    n.velocity = e.velocity;
                    n.tuning = e.tuning;
                }
                NOTE_OFF => {
                    let e = ev.__field0.noteOff;
                    n.channel = e.channel;
                    n.pitch = e.pitch;
                    n.note_id = e.noteId;
                    n.velocity = e.velocity;
                    n.tuning = e.tuning;
                }
                DATA => {
                    let e = ev.__field0.data;
                    n.type_id = e.r#type;
                    data = if e.size == 0 {
                        Some(&[])
                    } else if e.bytes.is_null() {
                        None
                    } else {
                        // SAFETY: VST3 `IEventList` guarantees `e.bytes` points
                        // to `e.size` readable bytes for the duration of
                        // `process` (`'a`).
                        Some(std::slice::from_raw_parts(e.bytes, e.size as usize))
                    };
                }
                POLY_PRESSURE => {
                    let e = ev.__field0.polyPressure;
                    n.channel = e.channel;
                    n.pitch = e.pitch;
                    n.note_id = e.noteId;
                    n.velocity = e.pressure;
                }
                NOTE_EXPRESSION => {
                    let e = ev.__field0.noteExpressionValue;
                    n.type_id = e.typeId;
                    n.note_id = e.noteId;
                    n.value = e.value;
                }
                _ => {}
            }
        }
        self.pending = Some((n, data));
    }
}

#[derive(Default)]
struct ViewSlot {
    /// The most recently created view; resize requests go to it.
    current: usize,
    current_ptr: Option<*mut IPlugView>,
    frame: Option<ComPtr<IPlugFrame>>,
    /// The view whose native editor is open.
    attached: usize,
}

/// Monotonic, never-reused key for `INSTANCES` registry lookups.
static NEXT_HOST_ID: AtomicUsize = AtomicUsize::new(1);

/// State for the Windows `wake_persist` fallback retry worker, coordinated
/// under one mutex so a failed signal arriving while the worker is exiting
/// either keeps it looping or spawns a replacement.
#[cfg(target_os = "windows")]
#[derive(Default)]
struct PersistRetryState {
    handle: Option<std::thread::JoinHandle<()>>,
    active: bool,
    retry_needed: bool,
}

/// Shared lock-free wake hub referenced by `Host` and by background
/// `PersistChangeSignal` / `CatalogSignal` / `HostQueue` wake callbacks.
/// Keeping this state behind an `Arc` lets callbacks run without borrowing
/// `&Host`, avoiding any aliasing overlap with `&mut Host` inside `Host::drop`.
#[repr(align(8))]
struct HostWakes {
    /// The catalog signal fired; the UI thread takes the changes.
    catalog_pending: AtomicBool,
    persist_pending: AtomicBool,
    closing: AtomicBool,
    #[cfg(target_os = "linux")]
    restart_fd: AtomicI32,
    #[cfg(target_os = "linux")]
    edit_fd: AtomicI32,
    #[cfg(target_os = "macos")]
    main_queue: std::sync::OnceLock<super::wake::macos::MainQueueWake>,
    #[cfg(target_os = "windows")]
    message_wake: std::sync::OnceLock<super::wake::windows::MessageWake>,
    /// Tracked Windows retry worker for failed `PostMessageW` calls on
    /// `wake_persist`. Cancelled and joined in [`Self::stop_persist_retry`]
    /// before `MessageWake` drops and frees the DLL module reference.
    #[cfg(target_os = "windows")]
    persist_retry_worker: Mutex<PersistRetryState>,
    #[cfg(target_os = "windows")]
    persist_retry_cv: std::sync::Condvar,
}

impl HostWakes {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            catalog_pending: AtomicBool::new(false),
            persist_pending: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            restart_fd: AtomicI32::new(-1),
            #[cfg(target_os = "linux")]
            edit_fd: AtomicI32::new(-1),
            #[cfg(target_os = "macos")]
            main_queue: std::sync::OnceLock::new(),
            #[cfg(target_os = "windows")]
            message_wake: std::sync::OnceLock::new(),
            #[cfg(target_os = "windows")]
            persist_retry_worker: Mutex::new(PersistRetryState::default()),
            #[cfg(target_os = "windows")]
            persist_retry_cv: std::sync::Condvar::new(),
        })
    }

    /// Cancel and join any Windows persist-retry worker before `MessageWake`
    /// destroys its `HWND` and releases the module handle.
    #[cfg(target_os = "windows")]
    fn stop_persist_retry(&self) {
        let handle = {
            let mut state = lock(&self.persist_retry_worker);
            self.closing.store(true, Ordering::Release);
            state.retry_needed = false;
            self.persist_retry_cv.notify_all();
            state.handle.take()
        };
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    /// Schedule a UI-thread flush. Lock-free, so the audio thread may call it.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
        allow(clippy::unused_self)
    )]
    fn wake_ui(&self) {
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            let fd = self.restart_fd.load(Ordering::Acquire);
            let fd = if fd >= 0 {
                fd
            } else {
                self.edit_fd.load(Ordering::Acquire)
            };
            if fd >= 0 {
                super::wake::linux::FdWake::notify_fd(fd);
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(q) = self.main_queue.get() {
            q.signal();
        }
        #[cfg(target_os = "windows")]
        if let Some(w) = self.message_wake.get() {
            let _ = w.signal();
        }
    }

    fn wake_persist(self: &Arc<Self>) {
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        let first = !self.persist_pending.swap(true, Ordering::AcqRel);
        #[cfg(target_os = "linux")]
        if first {
            let fd = self.restart_fd.load(Ordering::Acquire);
            if fd >= 0 {
                super::wake::linux::FdWake::notify_fd(fd);
            }
        }
        #[cfg(target_os = "macos")]
        if first && let Some(q) = self.main_queue.get() {
            q.signal();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = first;
            // Fast path: immediate `PostMessageW` (coalesced inside `MessageWake`).
            let Some(w) = self.message_wake.get() else {
                return;
            };
            if w.signal() {
                return;
            }
            // Rare fallback when the UI message queue is momentarily full:
            // coordinate worker lifecycle and new retry requests under one
            // mutex so a request arriving while the worker is exiting either
            // keeps it looping or spawns a replacement.
            let mut state = lock(&self.persist_retry_worker);
            if self.closing.load(Ordering::Acquire) {
                return;
            }
            state.retry_needed = true;
            if state.active {
                self.persist_retry_cv.notify_one();
                return;
            }
            if let Some(old) = state.handle.take() {
                let _ = old.join();
            }
            state.active = true;
            let wakes = Arc::clone(self);
            match std::thread::Builder::new()
                .name("moose-vst3-persist-wake-retry".into())
                .spawn(move || {
                    loop {
                        {
                            let state = lock(&wakes.persist_retry_worker);
                            if wakes.closing.load(Ordering::Acquire)
                                || !wakes.persist_pending.load(Ordering::Acquire)
                                || !state.retry_needed
                            {
                                let mut state = state;
                                state.active = false;
                                return;
                            }
                            let (mut state, _) = wakes
                                .persist_retry_cv
                                .wait_timeout(state, std::time::Duration::from_millis(10))
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if wakes.closing.load(Ordering::Acquire)
                                || !wakes.persist_pending.load(Ordering::Acquire)
                                || !state.retry_needed
                            {
                                state.active = false;
                                return;
                            }
                            state.retry_needed = false;
                        }
                        let signaled = wakes.message_wake.get().is_none_or(|w| w.signal());
                        let mut state = lock(&wakes.persist_retry_worker);
                        if wakes.closing.load(Ordering::Acquire)
                            || !wakes.persist_pending.load(Ordering::Acquire)
                        {
                            state.active = false;
                            return;
                        }
                        if signaled && !state.retry_needed {
                            state.active = false;
                            return;
                        }
                        if !signaled {
                            state.retry_needed = true;
                        }
                    }
                }) {
                Ok(handle) => {
                    state.handle = Some(handle);
                }
                Err(_) => {
                    // Thread creation failed: clear `active` so a future
                    // signal can retry, and make a bounded inline attempt now.
                    state.active = false;
                    drop(state);
                    for _ in 0..3 {
                        std::thread::yield_now();
                        if w.signal() {
                            break;
                        }
                    }
                }
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        let _ = first;
    }
}

pub(crate) struct Host {
    pub id: usize,
    backend: Option<Box<dyn Vst3Backend>>,
    pub reg: &'static Registration,
    config: UnsafeCell<Config>,
    scratch: UnsafeCell<Scratch>,
    in_active: AtomicU32,
    out_active: AtomicU32,
    handler: Mutex<Option<ComPtr<IComponentHandler>>>,
    has_handler3: AtomicBool,
    in_drain_queue: AtomicBool,
    pending_restart: AtomicI32,
    presentation_revision: AtomicU64,
    /// A change that may move `presentation_revision` happened; the UI
    /// thread compares it once.
    presentation_check: AtomicBool,
    param_list: Mutex<ParamList>,
    /// Dedicated non-colliding parameter ID for the root unit's
    /// `kIsProgramChange` selector, exposed only when `program_list` is
    /// non-empty (VST3 SDK `IUnitInfo` contract).
    program_change_id: u32,
    /// Currently selected index in the root unit's program list.
    selected_program: AtomicI32,
    /// Suppresses the host's `setParamNormalized` echo of our own edit.
    in_perform_edit: AtomicBool,
    persist_wanted: bool,
    persist_bound: AtomicBool,
    /// A `MARK_DIRTY` raised before `setComponentHandler` installed a handler.
    dirty_pending: AtomicBool,
    edits: Mutex<Edits>,
    view: Mutex<ViewSlot>,
    #[cfg(target_os = "linux")]
    persist_wake: Mutex<Option<super::wake::linux::FdWake>>,
    #[cfg(target_os = "linux")]
    timer: Mutex<Option<super::wake::linux::TimerWake>>,
    /// The host context (factory `setHostContext`, else `initialize`), for
    /// `IWaylandHost`.
    #[cfg(target_os = "linux")]
    context: std::sync::OnceLock<ComPtr<FUnknown>>,
    wakes: Arc<HostWakes>,
}

// SAFETY: `backend` is `Send + Sync`. `config` follows the VST3 threading
// contract (written only while inactive). `scratch` is touched only by `process`
// and `setupProcessing`, which the host never overlaps. The view slot's raw
// pointer is only dereferenced on the UI thread.
unsafe impl Send for Host {}
// SAFETY: Same reasoning as `Send`: every shared-access path reaches `backend` (itself `Sync`),
// atomics, mutexes, or the `UnsafeCell`s whose access the VST3 threading contract serializes.
unsafe impl Sync for Host {}

thread_local! {
    /// The instance whose `process` is on this thread's stack, so the audio
    /// thread can flag a restart with no lookup or lock.
    static PROCESSING: Cell<*const Host> = const { Cell::new(std::ptr::null()) };
}

/// Flag a `restartComponent` for the UI thread. Lock-free on the audio
/// thread (inside `process`); other threads find the instance through the
/// registry.
pub(super) fn mark_restart(host_id: usize, flags: i32) {
    let flag = |host: &Host| {
        host.pending_restart.fetch_or(flags, Ordering::Release);
        // A state load can rename parameters too.
        host.presentation_check.store(true, Ordering::Release);
        host.wake_ui();
    };
    let on_audio = PROCESSING.with(|current| {
        // SAFETY: `PROCESSING` holds a non-null pointer only while `Host::process` runs on this
        // thread and borrows that `Host`, so it is valid here.
        unsafe { current.get().as_ref() }
            .filter(|h| h.id == host_id)
            .map(flag)
    });
    if on_audio.is_none()
        && let Some(host) = super::pin(host_id)
    {
        flag(&host);
        // A UI-thread caller (setState, the editor) delivers right away.
        if host.may_deliver(&lock(&host.edits)) {
            host.flush_pending_restart();
        }
    }
}

/// Schedule a UI-thread drain of `HostQueue` through the separately heap-owned
/// [`HostWakes`] pointer bound by `Host::new`. Never borrows `&Host` (avoiding
/// any aliasing overlap with `&mut Host` inside `Host::drop`), never pins
/// `Arc<Host>`, never drops `Host`, and is lock-free on the audio thread.
///
/// # Safety
/// `host` must be null or the opaque [`HostWakes`] pointer cast to
/// `*const Host` by `Host::new`; `Host::drop` closes `HostQueue` (joining
/// every in-flight call) before mutating or dropping `HostWakes` / OS wakes.
pub(super) unsafe fn wake_host_queue(host: *const Host) {
    // SAFETY: The caller guarantees `host` is null or the `Arc<HostWakes>` pointer bound in
    // `Host::new`; `Host::drop` closes the queue (joining this call) before the `Arc` can drop.
    let Some(wakes) = (unsafe { host.cast::<HostWakes>().as_ref() }) else {
        return;
    };
    wakes.wake_ui();
}

/// Drain the instance's `HostQueue` on the UI thread, or wake the UI thread
/// when called off it. Lock-free on the audio thread.
pub(super) fn flush_host_queue(host_id: usize) {
    let on_audio = PROCESSING.with(|current| {
        // SAFETY: `PROCESSING` holds a non-null pointer only while `Host::process` runs on this
        // thread and borrows that `Host`, so it is valid here.
        unsafe { current.get().as_ref() }
            .filter(|h| h.id == host_id)
            .map(Host::wake_ui)
    });
    if on_audio.is_none()
        && let Some(host) = super::pin(host_id)
    {
        let can_deliver = {
            let e = lock(&host.edits);
            if host.may_deliver(&e) {
                true
            } else {
                host.wake_edits(&e);
                false
            }
        };
        if can_deliver {
            host.drain_host_queue();
            host.flush_pending_restart();
        } else {
            host.wake_ui();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn default_active_mask(kinds: &[u8]) -> u32 {
    kinds
        .iter()
        .take(32)
        .enumerate()
        .filter(|&(_, &k)| k == 0)
        .fold(0, |mask, (i, _)| mask | (1 << i))
}

/// Canonical `SpeakerArrangement` per channel count; the popcount fallback
/// keeps the channel count right for other widths.
const fn speaker_arrangement(channels: u32) -> u64 {
    const L: u64 = 1 << 0;
    const R: u64 = 1 << 1;
    const C: u64 = 1 << 2;
    const LFE: u64 = 1 << 3;
    const LS: u64 = 1 << 4;
    const RS: u64 = 1 << 5;
    const SL: u64 = 1 << 9;
    const SR: u64 = 1 << 10;
    const M: u64 = 1 << 19;
    match channels {
        1 => M,
        2 => L | R,
        4 => L | R | LS | RS,
        6 => L | R | C | LFE | LS | RS,
        8 => L | R | C | LFE | LS | RS | SL | SR,
        64 => u64::MAX,
        n => (1u64 << n) - 1,
    }
}

fn refresh_bus_channels(reg: &Registration, cfg: &mut Config) {
    for (b, w) in cfg.in_bus_channels.iter_mut().enumerate() {
        *w = reg.factory.layout_bus_channels(cfg.layout, false, b as u32);
    }
    for (b, w) in cfg.out_bus_channels.iter_mut().enumerate() {
        *w = reg.factory.layout_bus_channels(cfg.layout, true, b as u32);
    }
}

impl Host {
    #[inline]
    pub(crate) fn backend(&self) -> &dyn Vst3Backend {
        self.backend
            .as_deref()
            .expect("Vst3Backend lives for the lifetime of Host")
    }

    /// A new instance, or `None` when the plugin's constructor failed.
    pub(super) fn new(
        reg: &'static Registration,
        #[cfg_attr(
            not(target_os = "linux"),
            allow(unused_variables, clippy::needless_pass_by_value)
        )]
        factory_run_loop: Option<ComPtr<vst3::Steinberg::Linux::IRunLoop>>,
        host_name: Option<&str>,
    ) -> Option<Arc<Self>> {
        let desc = &reg.desc;
        let mut identity = moose_core::info::HostIdentity::new(moose_core::info::HostFormat::Vst3);
        if let Some(name) = host_name {
            identity = identity.with_name(name);
        }
        let backend = reg.factory.create(&identity)?;
        let id = NEXT_HOST_ID.fetch_add(1, Ordering::Relaxed);
        let mut config = Config {
            sample_rate: 44100.0,
            max_frames: 1024,
            sample_size: SymbolicSampleSizes_::kSample32 as i32,
            process_mode: 0,
            layout: 0,
            total_in: desc.num_inputs,
            total_out: desc.num_outputs,
            in_bus_channels: vec![0; desc.input_bus_kinds.len()],
            out_bus_channels: vec![0; desc.output_bus_kinds.len()],
        };
        refresh_bus_channels(reg, &mut config);
        let presentation_revision = backend.param_presentation_revision();
        let persist_wanted = backend.wants_persist_notifications();
        let host = Arc::new(Self {
            id,
            backend: Some(backend),
            reg,
            config: UnsafeCell::new(config),
            scratch: UnsafeCell::new(Scratch {
                in_ptrs: vec![std::ptr::null(); desc.max_inputs as usize],
                out_ptrs: vec![std::ptr::null_mut(); desc.max_outputs as usize],
                silence: Vec::new(),
                trash: Vec::new(),
                param_cursors: Vec::with_capacity(reg.params.len() + PARAM_CURSOR_SLACK),
                param_heap: BinaryHeap::with_capacity(reg.params.len() + PARAM_CURSOR_SLACK),
            }),
            in_active: AtomicU32::new(default_active_mask(&desc.input_bus_kinds)),
            out_active: AtomicU32::new(default_active_mask(&desc.output_bus_kinds)),
            handler: Mutex::new(None),
            has_handler3: AtomicBool::new(false),
            in_drain_queue: AtomicBool::new(false),
            pending_restart: AtomicI32::new(0),
            presentation_revision: AtomicU64::new(presentation_revision),
            presentation_check: AtomicBool::new(false),
            param_list: Mutex::new(ParamList::All),
            program_change_id: allocate_program_change_id(reg),
            selected_program: AtomicI32::new(0),
            in_perform_edit: AtomicBool::new(false),
            persist_wanted,
            persist_bound: AtomicBool::new(false),
            dirty_pending: AtomicBool::new(false),
            edits: Mutex::new(Edits {
                #[cfg(target_os = "linux")]
                ui_thread: Some(std::thread::current().id()),
                ..Edits::default()
            }),
            view: Mutex::new(ViewSlot::default()),
            #[cfg(target_os = "linux")]
            persist_wake: Mutex::new(None),
            #[cfg(target_os = "linux")]
            timer: Mutex::new(None),
            #[cfg(target_os = "linux")]
            context: std::sync::OnceLock::new(),
            wakes: HostWakes::new(),
        });
        super::register_instance(&host);
        // Instances are created on the host UI thread, so these wakes bind to it.
        #[cfg(target_os = "macos")]
        if let Some(q) = super::wake::macos::MainQueueWake::new(id as *mut c_void, main_queue_event)
        {
            let _ = host.wakes.main_queue.set(q);
        }
        #[cfg(target_os = "windows")]
        if let Some(w) =
            super::wake::windows::MessageWake::new(id as *mut c_void, win_message_event)
        {
            let _ = host.wakes.message_wake.set(w);
        }
        #[cfg(target_os = "linux")]
        if let Some(rl) = factory_run_loop {
            host.bind_persist_run_loop(rl);
        }
        host.bind_persist_signal();
        // The host hasn't read the list yet: changes from construction are already in it.
        let _ = host.backend().take_catalog_changes();
        // SAFETY: pass the separately heap-owned `HostWakes` pointer (opaque
        // `*const Host` token); `Host::drop` closes and joins `host_queue`
        // before mutating or dropping `self.wakes`, and `wake_host_queue` never
        // constructs `&Host` (zero aliasing with `&mut Host` in `Host::drop`).
        unsafe {
            host.backend()
                .bind_host_queue(id, Some(Arc::as_ptr(&host.wakes).cast::<Host>()));
        };
        let catalog_wakes = Arc::clone(&host.wakes);
        host.backend().set_catalog_waker(Some(Box::new(move || {
            catalog_wakes.catalog_pending.store(true, Ordering::Release);
            catalog_wakes.wake_ui();
        })));
        // Drain any `HostNotice` raised synchronously during `hooks.connect`.
        host.drain_host_queue();
        host.flush_pending_restart();
        Some(host)
    }

    /// Schedule a UI-thread flush. Lock-free, so the audio thread may call it.
    fn wake_ui(&self) {
        self.wakes.wake_ui();
    }

    /// A processed or host-set change to `id`: if it can rename parameters,
    /// have the UI thread compare the presentation revision. Audio-safe.
    fn note_param_change(&self, id: u32) {
        if self.backend().param_affects_presentation(id) {
            self.check_presentation();
        }
    }

    pub(super) fn check_presentation(&self) {
        if !self.presentation_check.swap(true, Ordering::AcqRel) {
            self.wake_ui();
        }
    }

    pub(super) fn drain_host_queue(&self) {
        if !self.in_drain_queue.swap(true, Ordering::AcqRel) {
            self.backend().drain_host_queue();
            self.in_drain_queue.store(false, Ordering::Release);
        }
    }

    // --- Parameter list ---------------------------------------------------------

    /// Indices of the parameters a dynamic-list host should see. UI thread.
    fn available_params(&self) -> Vec<usize> {
        let backend = self.backend();
        (0..self.reg.params.len())
            .filter(|&i| {
                backend
                    .param_presentation(self.reg.params[i].id)
                    .is_none_or(|p| !p.unavailable)
            })
            .collect()
    }

    /// Choose the list mode for this host. UI thread (`initialize`).
    pub(super) fn set_dynamic_support(&self, support: DynamicSupport) {
        let list = match support {
            DynamicSupport::Reload => ParamList::Available(self.available_params()),
            DynamicSupport::SlotPool => ParamList::All,
        };
        *lock(&self.param_list) = list;
    }

    pub(super) fn dynamic_support(&self) -> DynamicSupport {
        match *lock(&self.param_list) {
            ParamList::All => DynamicSupport::SlotPool,
            ParamList::Available(_) => DynamicSupport::Reload,
        }
    }

    /// Rebuild the `Reload` list; true when it changed.
    fn relist_params(&self) -> bool {
        if self.dynamic_support() != DynamicSupport::Reload {
            return false;
        }
        let fresh = self.available_params();
        let mut list = lock(&self.param_list);
        match &mut *list {
            ParamList::Available(listed) if *listed != fresh => {
                *listed = fresh;
                true
            }
            _ => false,
        }
    }

    fn param_count(&self) -> usize {
        match &*lock(&self.param_list) {
            ParamList::All => self.reg.params.len(),
            ParamList::Available(listed) => listed.len(),
        }
    }

    /// The `Registration::params` index of host index `index`.
    fn param_index(&self, index: i32) -> Option<usize> {
        let index = usize::try_from(index).ok()?;
        match &*lock(&self.param_list) {
            ParamList::All => (index < self.reg.params.len()).then_some(index),
            ParamList::Available(listed) => listed.get(index).copied(),
        }
    }

    fn config(&self) -> &Config {
        // SAFETY: VST3 writes `Config` only from `setupProcessing`/`setBusArrangements`, which
        // hosts never overlap with any other call on this component, so no `&mut` exists while this
        // shared borrow is live.
        unsafe { &*self.config.get() }
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn config_mut(&self) -> &mut Config {
        // SAFETY: The caller guarantees the VST3 inactive-state exclusivity described on `config`,
        // so this is the only live borrow of `Config`.
        unsafe { &mut *self.config.get() }
    }

    fn handler(&self) -> Option<ComPtr<IComponentHandler>> {
        lock(&self.handler).clone()
    }

    // --- UI-thread delivery -------------------------------------------------

    /// Whether this thread may call the host now.
    // `self` holds the wake on macOS and Windows.
    #[cfg_attr(target_os = "linux", allow(clippy::unused_self))]
    fn may_deliver(&self, #[allow(unused_variables)] edits: &Edits) -> bool {
        #[cfg(target_os = "linux")]
        {
            edits.ui_thread == Some(std::thread::current().id())
        }
        #[cfg(target_os = "macos")]
        {
            self.wakes.main_queue.get().is_none() || super::wake::macos::on_main_thread()
        }
        #[cfg(target_os = "windows")]
        {
            self.wakes
                .message_wake
                .get()
                .is_none_or(|w| w.thread == std::thread::current().id())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            true
        }
    }

    // `self` holds the wake on macOS and Windows.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "windows")),
        allow(clippy::unused_self)
    )]
    fn wake_edits(&self, #[allow(unused_variables)] edits: &Edits) {
        #[cfg(target_os = "linux")]
        if let Some(w) = &edits.wake {
            w.notify();
        } else {
            let fd = self.wakes.restart_fd.load(Ordering::Acquire);
            if fd >= 0 {
                super::wake::linux::FdWake::notify_fd(fd);
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(q) = self.wakes.main_queue.get() {
            q.signal();
        }
        #[cfg(target_os = "windows")]
        if let Some(w) = self.wakes.message_wake.get() {
            let _ = w.signal();
        }
    }

    /// Make `op` now on the UI thread (after anything queued before it), or
    /// queue it for that thread. Returns the resize result (1 = made or
    /// queued; the host's `onSize` is what commits a size either way).
    pub(crate) fn submit(&self, op: EditOp) -> i32 {
        if self.try_queue(op) || self.queue_behind_remaining(op) {
            return 1;
        }
        self.run_op(op)
    }

    fn try_queue(&self, op: EditOp) -> bool {
        let mut e = lock(&self.edits);
        if e.closed {
            return true;
        }
        if e.draining {
            e.pending.push_back(op);
            return true;
        }
        if self.may_deliver(&e) {
            return false;
        }
        e.pending.push_back(op);
        self.wake_edits(&e);
        true
    }

    fn queue_behind_remaining(&self, op: EditOp) -> bool {
        let mut e = lock(&self.edits);
        if e.closed {
            return true;
        }
        if self.may_deliver(&e) {
            e = self.drain_locked(e, false);
        }
        if e.closed {
            return true;
        }
        if e.pending.is_empty() && self.may_deliver(&e) {
            return false;
        }
        e.pending.push_back(op);
        self.wake_edits(&e);
        true
    }

    /// Queued edits and open gestures, for the native tests.
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(super) fn edit_counts(&self) -> (usize, usize) {
        let e = lock(&self.edits);
        (e.pending.len(), e.open_gestures.len())
    }

    pub(super) fn drain_edits(&self) {
        drop(self.drain_locked(lock(&self.edits), false));
        if self.may_deliver(&lock(&self.edits)) {
            self.drain_host_queue();
            self.flush_pending_restart();
        }
    }

    /// Make queued calls oldest first. A transition drains wherever it runs;
    /// any other drain stops once this thread may no longer deliver.
    fn drain_locked<'a>(
        &'a self,
        mut e: MutexGuard<'a, Edits>,
        transition: bool,
    ) -> MutexGuard<'a, Edits> {
        if e.draining {
            return e;
        }
        e.draining = true;
        let mut left = if transition {
            TRANSITION_EDITS
        } else {
            DRAIN_EDITS
        };
        while left > 0 && !e.pending.is_empty() && (transition || self.may_deliver(&e)) {
            let take = left.min(e.pending.len());
            let ops: Vec<EditOp> = e.pending.drain(..take).collect();
            left -= take;
            drop(e);
            for op in ops {
                self.run_op(op);
            }
            e = lock(&self.edits);
        }
        e.draining = false;
        if !e.pending.is_empty() {
            self.wake_edits(&e);
        }
        e
    }

    /// End every open gesture, then an open group edit, on the current
    /// handler; returns what was open so a new handler can reopen it.
    fn close_open_gestures<'a>(
        &'a self,
        mut e: MutexGuard<'a, Edits>,
    ) -> (MutexGuard<'a, Edits>, OpenEdits) {
        let open = OpenEdits {
            gestures: std::mem::take(&mut e.open_gestures),
            group: std::mem::take(&mut e.group_open),
        };
        let Some(handler) = self.handler() else {
            return (e, open);
        };
        if open.gestures.is_empty() && !open.group {
            return (e, open);
        }
        e.draining = true;
        drop(e);
        for &id in &open.gestures {
            // SAFETY: `handler` is the host's live, retained handler, called
            // on the UI thread like every other handler call here.
            unsafe { handler.endEdit(id) };
        }
        if open.group
            && let Some(handler) = handler.cast::<IComponentHandler2>()
        {
            // SAFETY: as above; `cast` returned a retained interface.
            unsafe { handler.finishGroupEdit() };
        }
        let mut e = lock(&self.edits);
        e.draining = false;
        (e, open)
    }

    fn run_op(&self, op: EditOp) -> i32 {
        match op {
            EditOp::Begin(id) => {
                if let Some(h) = self.handler() {
                    // SAFETY: `h` is the host's retained `IComponentHandler`, called on the UI
                    // thread (`run_op` runs only where `may_deliver` holds).
                    unsafe { h.beginEdit(id) };
                    lock(&self.edits).open_gestures.push(id);
                }
                1
            }
            EditOp::Perform(id, value) => {
                if let Some(h) = self.handler() {
                    self.in_perform_edit.store(true, Ordering::Relaxed);
                    // SAFETY: `h` is the host's retained `IComponentHandler`, called on the UI
                    // thread.
                    unsafe { h.performEdit(id, value) };
                    self.in_perform_edit.store(false, Ordering::Relaxed);
                    self.note_param_change(id);
                    self.flush_pending_restart();
                }
                1
            }
            EditOp::End(id) => {
                if let Some(h) = self.handler() {
                    // SAFETY: `h` is the host's retained `IComponentHandler`, called on the UI
                    // thread.
                    unsafe { h.endEdit(id) };
                    let mut e = lock(&self.edits);
                    if let Some(at) = e.open_gestures.iter().position(|&g| g == id) {
                        e.open_gestures.remove(at);
                    }
                    drop(e);
                    self.flush_pending_restart();
                }
                1
            }
            EditOp::Resize(w, h) => self.resize_view(w, h),
            EditOp::Dirty => {
                if let Some(h2) = self.handler().and_then(|h| h.cast::<IComponentHandler2>()) {
                    self.dirty_pending.store(false, Ordering::Release);
                    // SAFETY: `h2` is a retained `IComponentHandler2` from `cast`, called on the UI
                    // thread.
                    unsafe { h2.setDirty(1) };
                } else {
                    self.dirty_pending.store(true, Ordering::Release);
                }
                1
            }
            EditOp::Group(edge) => {
                if let Some(h) = self.handler().and_then(|h| h.cast::<IComponentHandler2>()) {
                    match edge {
                        // SAFETY: `h` is a retained `IComponentHandler2` from `cast`, called on the
                        // UI thread.
                        EditGroup::Begin => unsafe { h.startGroupEdit() },
                        // SAFETY: `h` is a retained `IComponentHandler2` from `cast`, called on the
                        // UI thread.
                        EditGroup::End => unsafe { h.finishGroupEdit() },
                    };
                    lock(&self.edits).group_open = edge == EditGroup::Begin;
                }
                1
            }
        }
    }

    fn resize_view(&self, w: u32, h: u32) -> i32 {
        let (view, frame) = {
            let slot = lock(&self.view);
            match (slot.current_ptr, slot.frame.clone()) {
                (Some(v), Some(f)) => (v, f),
                _ => return 0,
            }
        };
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        // SAFETY: `frame` is a retained `IPlugFrame`; `view` is the current view, which unregisters
        // itself in `forget_view` (UI thread) before it is destroyed, and this also runs on the UI
        // thread. `rect` is a local.
        i32::from(unsafe { frame.resizeView(view, &raw mut rect) } == kResultOk)
    }

    /// Timer / dispatch tick on the UI thread.
    pub(super) fn on_ui_tick(&self) {
        self.drain_host_queue();
        self.flush_pending_restart();
        self.drain_edits();
    }

    pub(super) fn has_context_menu(&self) -> bool {
        self.has_handler3.load(Ordering::Acquire) && lock(&self.view).current_ptr.is_some()
    }

    /// Pop up the host's `IComponentHandler3` context menu on the UI thread.
    pub(super) fn popup_context_menu(
        &self,
        target: MenuTarget,
        x: i32,
        y: i32,
        items: &[MenuItem],
    ) {
        let Some(handler3) = self.handler().and_then(|h| h.cast::<IComponentHandler3>()) else {
            return;
        };
        let Some(view) = lock(&self.view).current_ptr else {
            return;
        };
        let param_id = match target {
            MenuTarget::Param(id) => Some(id),
            MenuTarget::Global => None,
        };
        let param_ptr = param_id
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref);
        // SAFETY: `handler3` is a retained `IComponentHandler3` and `view` is the current,
        // still-registered view (UI thread). `param_ptr` is null or points to the local `param_id`.
        let raw_menu = unsafe { handler3.createContextMenu(view, param_ptr) };
        // SAFETY: `createContextMenu` returns a new reference that we own, so `from_raw` takes it
        // without an extra `addRef`.
        let Some(menu) = (unsafe { ComPtr::from_raw(raw_menu) }) else {
            return;
        };
        let Some(host_arc) = super::pin(self.id) else {
            return;
        };
        let target_com = ComWrapper::new(ContextMenuTarget {
            host: std::sync::Arc::downgrade(&host_arc),
            target,
        })
        .to_com_ptr::<IContextMenuTarget>();
        for item in items {
            use IContextMenuItem_::Flags_::*;
            let mut raw_item = IContextMenuItem {
                name: [0; 128],
                tag: 0,
                flags: 0,
            };
            let mut with_target = false;
            match item {
                MenuItem::Entry {
                    label,
                    action,
                    enabled,
                } => {
                    utf16_into(&mut raw_item.name, label);
                    raw_item.tag = *action as i32;
                    if !enabled {
                        raw_item.flags = kIsDisabled as i32;
                    }
                    with_target = true;
                }
                MenuItem::Check {
                    label,
                    action,
                    enabled,
                    checked,
                } => {
                    utf16_into(&mut raw_item.name, label);
                    raw_item.tag = *action as i32;
                    if !enabled {
                        raw_item.flags |= kIsDisabled as i32;
                    }
                    if *checked {
                        raw_item.flags |= kIsChecked as i32;
                    }
                    with_target = true;
                }
                MenuItem::Separator => {
                    raw_item.flags = kIsSeparator as i32;
                }
                MenuItem::Submenu { label, enabled } => {
                    utf16_into(&mut raw_item.name, label);
                    raw_item.flags = kIsGroupStart as i32;
                    if !enabled {
                        raw_item.flags |= kIsDisabled as i32;
                    }
                }
                MenuItem::EndSubmenu => {
                    raw_item.flags = kIsGroupEnd as i32;
                }
                MenuItem::Title { label } => {
                    utf16_into(&mut raw_item.name, label);
                    raw_item.flags = kIsDisabled as i32;
                }
                _ => continue,
            }
            let target_ptr = if with_target {
                target_com
                    .as_ref()
                    .map_or(std::ptr::null_mut(), ComPtr::as_ptr)
            } else {
                std::ptr::null_mut()
            };
            // SAFETY: `menu` is live; `raw_item` is a local the host copies, and `target_ptr` is
            // null or our retained `IContextMenuTarget`, which the menu `addRef`s.
            unsafe { menu.addItem(&raw const raw_item, target_ptr) };
        }
        // SAFETY: `menu` is the live context menu created above.
        unsafe { menu.popup(x, y) };
    }

    // --- Restart and persist notifications -----------------------------------

    /// Send any restart flags the audio thread, a catalog change or a
    /// presentation change left pending. UI thread only.
    pub(super) fn flush_pending_restart(&self) {
        self.drain_host_queue();
        let Some(handler) = self.handler() else {
            return;
        };
        self.flush_dirty();
        self.flush_persist_change();
        let backend = self.backend();
        let mut check = self.presentation_check.swap(false, Ordering::AcqRel);
        if self.wakes.catalog_pending.swap(false, Ordering::AcqRel) {
            let changes = CatalogChange::from_bits_truncate(backend.take_catalog_changes());
            let support = self.dynamic_support();
            if changes.contains(CatalogChange::STRUCTURE) {
                self.relist_params();
            }
            self.pending_restart
                .fetch_or(support.restart_flags(changes), Ordering::Release);
            check = true;
        }
        if check {
            let revision = backend.param_presentation_revision();
            if self.presentation_revision.swap(revision, Ordering::AcqRel) != revision {
                let mut flags = RestartFlags_::kParamTitlesChanged as i32;
                if self.relist_params() {
                    flags |= RestartFlags_::kReloadComponent as i32;
                }
                self.pending_restart.fetch_or(flags, Ordering::Release);
            }
        }
        let flags = self.pending_restart.swap(0, Ordering::AcqRel);
        if flags != 0 {
            // SAFETY: `handler` is the host's retained `IComponentHandler`, called on the UI
            // thread.
            unsafe { handler.restartComponent(flags) };
        }
    }

    fn bind_persist_signal(&self) {
        if !self.persist_wanted || self.persist_bound.load(Ordering::Acquire) {
            return;
        }
        #[cfg(target_os = "linux")]
        let ready = lock(&self.persist_wake).is_some();
        #[cfg(target_os = "macos")]
        let ready = self.wakes.main_queue.get().is_some();
        #[cfg(target_os = "windows")]
        let ready = self.wakes.message_wake.get().is_some();
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        let ready = false;
        if !ready || self.persist_bound.swap(true, Ordering::AcqRel) {
            return;
        }
        let wakes = Arc::clone(&self.wakes);
        self.backend().set_persist_waker(Some(Box::new(move || {
            wakes.wake_persist();
        })));
    }

    /// Deliver a `MARK_DIRTY` that arrived before an `IComponentHandler2` existed.
    pub(super) fn flush_dirty(&self) {
        let Some(h2) = self.handler().and_then(|h| h.cast::<IComponentHandler2>()) else {
            return;
        };
        if !self.dirty_pending.swap(false, Ordering::AcqRel) {
            return;
        }
        // SAFETY: `h2` is a retained `IComponentHandler2` from `cast`, called on the UI thread.
        unsafe { h2.setDirty(1) };
    }

    /// Report a completed persistent-document change as one host edit.
    /// Without a bound wake this runs on the next host UI-thread callback.
    pub(super) fn flush_persist_change(&self) {
        if !self.persist_wanted {
            return;
        }
        let Some(handler) = self.handler() else {
            return;
        };
        if self.persist_bound.load(Ordering::Acquire)
            && !self.wakes.persist_pending.swap(false, Ordering::AcqRel)
        {
            return;
        }
        let Some((id, value)) = self.backend().take_persist_change() else {
            return;
        };
        // SAFETY: `handler` is the host's retained `IComponentHandler`, called on the UI thread;
        // begin/perform/end form one complete gesture.
        unsafe {
            handler.beginEdit(id);
            self.in_perform_edit.store(true, Ordering::Relaxed);
            handler.performEdit(id, value);
            self.in_perform_edit.store(false, Ordering::Relaxed);
            handler.endEdit(id);
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn bind_persist_run_loop(
        self: &Arc<Self>,
        run_loop: ComPtr<vst3::Steinberg::Linux::IRunLoop>,
    ) {
        let mut slot = lock(&self.persist_wake);
        if slot.is_some() {
            return;
        }
        *slot = super::wake::linux::FdWake::bind(
            run_loop,
            Arc::downgrade(self),
            Host::on_instance_wake,
        );
        if let Some(wake) = slot.as_ref() {
            self.wakes.restart_fd.store(wake.fd(), Ordering::Release);
        }
        drop(slot);
        self.bind_persist_signal();
    }

    #[cfg(target_os = "linux")]
    fn on_instance_wake(&self) {
        self.flush_pending_restart();
        self.flush_persist_change();
        self.drain_edits();
    }

    /// Bind (or with `None`, unbind) the edit wake and restart timer to a
    /// plug frame's run loop. UI thread.
    ///
    /// # Safety
    ///
    /// `frame`, when `Some`, is null or a live COM object for this call.
    #[cfg(target_os = "linux")]
    pub(super) unsafe fn update_run_loop_registration(
        self: &Arc<Self>,
        frame: Option<*mut FUnknown>,
    ) {
        use super::wake::linux::{FdWake, TimerWake, run_loop_of};
        // SAFETY: the caller guarantees `frame` is null or live for this call.
        let run_loop = frame.and_then(|f| unsafe { run_loop_of(f) });
        if self.persist_wanted
            && let Some(rl) = &run_loop
        {
            self.bind_persist_run_loop(rl.clone());
        }
        // Deliver everything queued before unbinding, so no worker steals
        // the detach batch.
        let old = {
            let mut e = self.drain_locked(lock(&self.edits), true);
            self.wakes.edit_fd.store(-1, Ordering::Release);
            e.wake.take()
        };
        drop(old);
        lock(&self.timer).take();
        let Some(rl) = run_loop else {
            return;
        };
        {
            let mut e = lock(&self.edits);
            e.ui_thread = Some(std::thread::current().id());
        }
        if let Some(wake) = FdWake::bind(rl.clone(), Arc::downgrade(self), Host::on_ui_tick) {
            self.wakes.edit_fd.store(wake.fd(), Ordering::Release);
            let mut e = lock(&self.edits);
            e.wake = Some(wake);
            if !e.pending.is_empty() {
                self.wake_edits(&e);
            }
        } else {
            // Only fall back to a run-loop timer if the host refused eventfd.
            *lock(&self.timer) = TimerWake::bind(rl, Arc::downgrade(self));
        }
    }

    /// Keep the first host context offered; later ones are ignored.
    ///
    /// # Safety
    ///
    /// `context` is null or a live COM object for this call.
    #[cfg(target_os = "linux")]
    pub(super) unsafe fn set_host_context(&self, context: *mut FUnknown) {
        if self.context.get().is_none()
            // SAFETY: the caller guarantees `context` is null or live; `retain`
            // takes our own reference before the call returns.
            && let Some(c) = unsafe { super::retain(context) }
        {
            let _ = self.context.set(c);
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn host_context(&self) -> Option<ComPtr<FUnknown>> {
        self.context.get().cloned()
    }

    // --- Views ----------------------------------------------------------------

    pub(super) fn set_current_view(&self, view: usize, ptr: *mut IPlugView) {
        let mut slot = lock(&self.view);
        slot.current = view;
        slot.current_ptr = Some(ptr);
        slot.frame = None;
    }

    pub(super) fn is_current_view(&self, view: usize) -> bool {
        lock(&self.view).current == view
    }

    pub(super) fn set_current_frame(&self, view: usize, frame: Option<ComPtr<IPlugFrame>>) {
        let mut slot = lock(&self.view);
        if slot.current == view {
            slot.frame = frame;
        }
    }

    pub(super) fn forget_view(&self, view: usize) {
        let mut slot = lock(&self.view);
        if slot.current == view {
            slot.current = 0;
            slot.current_ptr = None;
            slot.frame = None;
        }
    }

    pub(super) fn attach_view(&self, view: usize) {
        lock(&self.view).attached = view;
    }

    /// Clears the attachment if `view` holds it; true if it did.
    pub(super) fn detach_view(&self, view: usize) -> bool {
        let mut slot = lock(&self.view);
        if slot.attached != view {
            return false;
        }
        slot.attached = 0;
        true
    }

    // --- Processing -----------------------------------------------------------

    /// Ingest the host's note events and parameter queues as one stream in
    /// non-decreasing sample-offset order, so the plugin's event list stays
    /// on its already-sorted fast path. A k-way merge over the note list and
    /// one cursor per queue; ties go to notes first, then queues in host
    /// order. All merge state lives in preallocated `scratch`, so this never
    /// allocates. Merge keys are the host's point offsets: a `PerSample`
    /// plugin's linear `ParamRamp` starts at the previous point instead and
    /// may still need the list's sort pass.
    ///
    /// Every point goes to the plugin's event list when the block renders; a
    /// queue's final value is also committed here when no sub-block apply
    /// will (a flush, a non-chunked parameter, or an event list that ran out
    /// of room). Returns true on an overflow: the caller then commits final
    /// values again after the block, so earlier queued points can't win.
    unsafe fn ingest_inputs(
        &self,
        events: *mut IEventList,
        changes: *mut IParameterChanges,
        will_process: bool,
    ) -> bool {
        // SAFETY: The caller passes the host's `IParameterChanges` for this `process` call: null
        // (handled by `from_raw`) or live for the call's duration.
        let changes = unsafe { ComRef::from_raw(changes) };
        // SAFETY: `c` is the live host change list.
        let queue_count = changes.map_or(0, |c| unsafe { c.getParameterCount() });
        let mut dropped = false;
        if !will_process {
            // A flush: nothing is queued, every final value commits now.
            for i in 0..queue_count {
                let queue =
                    // SAFETY: `c` is the live host change list and `i` is below its count.
                    changes.map_or(std::ptr::null_mut(), |c| unsafe { c.getParameterData(i) });
                // SAFETY: `queue` comes from the live change list for this `process` call.
                if let Some(mut cursor) = unsafe { ParamCursor::new(self, queue, false) } {
                    // SAFETY: The queue outlives this `process` call.
                    dropped |= unsafe { cursor.drain(self) };
                }
            }
            return dropped;
        }

        // SAFETY: `scratch` is only touched by `process` and
        // `setupProcessing`, which the host never runs concurrently; this
        // borrow ends before `process` takes its own.
        let scratch = unsafe { &mut *self.scratch.get() };
        let (cursors, heap) = (&mut scratch.param_cursors, &mut scratch.param_heap);
        cursors.clear();
        heap.clear();
        let mut overflow_from = queue_count;
        for i in 0..queue_count {
            // SAFETY: `c` is the live host change list and `i` is below its count.
            let queue = changes.map_or(std::ptr::null_mut(), |c| unsafe { c.getParameterData(i) });
            if cursors.len() == cursors.capacity() {
                overflow_from = i;
                break;
            }
            // SAFETY: `queue` comes from the live change list for this `process` call.
            let Some(mut cursor) = (unsafe { ParamCursor::new(self, queue, true) }) else {
                continue;
            };
            // A queue without a readable point changes nothing.
            // SAFETY: The queue outlives this `process` call.
            if unsafe { cursor.advance() } {
                #[allow(clippy::cast_possible_truncation)]
                heap.push(Reverse((cursor.pending.0, cursors.len() as u32)));
                cursors.push(cursor);
            }
        }

        // SAFETY: The caller passes the host's `IEventList` for this `process` call (null or live
        // for its duration).
        let mut notes = unsafe { NoteCursor::new(events) };
        loop {
            let note_offset = notes.pending.as_ref().map(|(n, _)| n.sample_offset);
            let next_param = heap.peek().map(|&Reverse(key)| key);
            let take_note = match (note_offset, next_param) {
                (None, None) => break,
                (Some(n), Some((p, _))) => n <= p,
                (Some(_), None) => true,
                (None, Some(_)) => false,
            };
            if take_note {
                let Some((event, data)) = notes.pending.take() else {
                    break;
                };
                // Actual exhaustion ends the note prefix. A reserve refusal
                // drops the whole pair but continues to later exact-only events.
                // Parameter points continue using their reserved headroom.
                let status = self.backend().push_input_event(&event, data);
                if status != OUTPUT_QUEUE_FULL {
                    // SAFETY: The event list outlives this `process` call.
                    unsafe { notes.advance() };
                }
            } else if let Some(Reverse((_, index))) = heap.pop() {
                let cursor = &mut cursors[index as usize];
                dropped |= cursor.emit(self);
                // SAFETY: The cursor's queue outlives this `process` call.
                if unsafe { cursor.advance() } {
                    heap.push(Reverse((cursor.pending.0, index)));
                } else {
                    cursor.finish(self);
                }
            }
        }
        for i in overflow_from..queue_count {
            // SAFETY: `c` is the live host change list and `i` is below its count.
            let queue = changes.map_or(std::ptr::null_mut(), |c| unsafe { c.getParameterData(i) });
            // SAFETY: `queue` comes from the live change list for this `process` call.
            if let Some(mut cursor) = unsafe { ParamCursor::new(self, queue, true) } {
                // SAFETY: The queue outlives this `process` call.
                dropped |= unsafe { cursor.drain(self) };
            }
        }
        dropped
    }

    /// The last point of every queue, committed after an overflowed block.
    unsafe fn commit_final_values(&self, changes: *mut IParameterChanges) {
        // SAFETY: The caller passes the host's `IParameterChanges` for this `process` call (null or
        // live for its duration).
        let Some(changes) = (unsafe { ComRef::from_raw(changes) }) else {
            return;
        };
        let backend = self.backend();
        // SAFETY: `changes` is the live host change list.
        for i in 0..unsafe { changes.getParameterCount() } {
            // SAFETY: `i` is below the live list's count; a null queue is handled by `from_raw`.
            let Some(queue) = (unsafe { ComRef::from_raw(changes.getParameterData(i)) }) else {
                continue;
            };
            // SAFETY: `queue` is the live host queue.
            let count = unsafe { queue.getPointCount() };
            let (mut offset, mut value) = (0, 0.0);
            if count > 0
                // SAFETY: `queue` is live, `count - 1` is in range (`count > 0`), and both
                // out-pointers are locals.
                && unsafe { queue.getPoint(count - 1, &raw mut offset, &raw mut value) }
                    == kResultOk
            {
                // SAFETY: `queue` is the live host queue.
                let id = unsafe { queue.getParameterId() };
                let plain = backend.param_denormalize(id, value);
                backend.param_set_value(id, plain);
            }
        }
    }

    fn transport(ctx: &ProcessContext) -> Vst3Transport {
        use ProcessContext_::StatesAndFlags_::*;
        let has = |flag: ProcessContext_::StatesAndFlags| ctx.state & flag as u32 != 0;
        let sig = has(kTimeSigValid);
        let cycle = has(kCycleValid);
        Vst3Transport {
            playing: has(kPlaying),
            recording: has(kRecording),
            tempo: if has(kTempoValid) { ctx.tempo } else { 120.0 },
            time_sig_num: if sig { ctx.timeSigNumerator } else { 4 },
            time_sig_den: if sig { ctx.timeSigDenominator } else { 4 },
            position_samples: ctx.projectTimeSamples as f64,
            position_beats: if has(kProjectTimeMusicValid) {
                ctx.projectTimeMusic
            } else {
                0.0
            },
            bar_start_beats: if has(kBarPositionValid) {
                ctx.barPositionMusic
            } else {
                0.0
            },
            cycle_start_beats: if cycle { ctx.cycleStartMusic } else { 0.0 },
            cycle_end_beats: if cycle { ctx.cycleEndMusic } else { 0.0 },
            cycle_active: has(kCycleActive),
        }
    }

    /// Hand the plugin's output events to the host list in global order.
    /// Note-id bookkeeping is committed only after the host accepted each.
    unsafe fn drain_output_events(&self, list: *mut IEventList) -> u32 {
        let backend = self.backend();
        // SAFETY: The caller passes the host's output `IEventList` for this `process` call (null or
        // live for its duration).
        let list = unsafe { ComRef::from_raw(list) };
        let mut result = OUTPUT_END;
        backend.begin_output_events();
        loop {
            let mut n = Vst3NativeEvent::default();
            match backend.next_output_event(&mut n) {
                OUTPUT_END => return result,
                OUTPUT_EMITTED => {}
                code @ (OUTPUT_UNSUPPORTED | OUTPUT_INVALID | OUTPUT_QUEUE_FULL) => return code,
                _ => return OUTPUT_INVALID,
            }
            let Some(list) = list else {
                return OUTPUT_UNSUPPORTED;
            };
            // SAFETY: `Event` is a plain C struct/union for which all-zero bytes are a valid value.
            let mut ev: Event = unsafe { std::mem::zeroed() };
            ev.busIndex = n.bus_index;
            ev.sampleOffset = n.sample_offset;
            ev.ppqPosition = n.ppq_position;
            ev.flags = n.flags as u16;
            ev.r#type = n.kind as u16;
            ev.__field0 = match n.kind {
                NOTE_ON => Event__type0 {
                    noteOn: NoteOnEvent {
                        channel: n.channel,
                        pitch: n.pitch,
                        tuning: n.tuning,
                        velocity: n.velocity,
                        length: n.length,
                        noteId: n.note_id,
                    },
                },
                NOTE_OFF => Event__type0 {
                    noteOff: NoteOffEvent {
                        channel: n.channel,
                        pitch: n.pitch,
                        velocity: n.velocity,
                        noteId: n.note_id,
                        tuning: n.tuning,
                    },
                },
                DATA => Event__type0 {
                    data: DataEvent {
                        size: n.len,
                        r#type: n.type_id,
                        bytes: n.bytes,
                    },
                },
                POLY_PRESSURE => Event__type0 {
                    polyPressure: PolyPressureEvent {
                        channel: n.channel,
                        pitch: n.pitch,
                        pressure: n.velocity,
                        noteId: n.note_id,
                    },
                },
                NOTE_EXPRESSION => Event__type0 {
                    noteExpressionValue: NoteExpressionValueEvent {
                        typeId: n.type_id,
                        noteId: n.note_id,
                        value: n.value,
                    },
                },
                LEGACY_CC_OUT => Event__type0 {
                    midiCCOut: LegacyMIDICCOutEvent {
                        controlNumber: n.type_id as u8,
                        channel: n.channel as i8,
                        value: n.data1 as i8,
                        value2: n.data2 as i8,
                    },
                },
                _ => return OUTPUT_INVALID,
            };
            // SAFETY: `list` is the live host output list; `ev` is a local the host copies (a
            // `DataEvent`'s bytes stay owned by the backend until the next `begin_output_events`).
            if unsafe { list.addEvent(&raw mut ev) } != kResultOk {
                return OUTPUT_QUEUE_FULL;
            }
            backend.commit_output_event();
            result = OUTPUT_EMITTED;
        }
    }

    /// Forward parameter values the plugin changed while processing, so the
    /// host UI and automation see them.
    unsafe fn write_output_params(
        &self,
        changes: *mut IParameterChanges,
        count: u32,
        mut result: u32,
    ) -> u32 {
        // SAFETY: The caller passes the host's output `IParameterChanges` for this call (null or
        // live for its duration).
        let Some(changes) = (unsafe { ComRef::from_raw(changes) }) else {
            return result;
        };
        let backend = self.backend();
        for i in 0..count {
            let Some((id, offset, value)) = backend.output_param(i) else {
                continue;
            };
            let mut index = 0;
            // SAFETY: `changes` is the live output list; `id` and `index` are locals.
            let queue = unsafe { changes.addParameterData(&raw const id, &raw mut index) };
            // SAFETY: `queue` is null or a queue owned by the live output list.
            let Some(queue) = (unsafe { ComRef::from_raw(queue) }) else {
                return OUTPUT_QUEUE_FULL;
            };
            // SAFETY: `queue` is the live output queue; `index` is a local out slot.
            if unsafe { queue.addPoint(offset, value, &raw mut index) } != kResultOk {
                return OUTPUT_QUEUE_FULL;
            }
            if result == OUTPUT_END {
                result = OUTPUT_EMITTED;
            }
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    unsafe fn process(&self, data: &mut ProcessData) -> tresult {
        let backend = self.backend();
        let desc = &self.reg.desc;
        let cfg = self.config();
        // Never reinterpret buffers at a width other than the negotiated one.
        if data.symbolicSampleSize != cfg.sample_size {
            return kResultFalse;
        }
        let suspended_flush = (desc.num_inputs > 0 || desc.num_outputs > 0)
            && data.numInputs == 0
            && data.numOutputs == 0;
        let will_process = data.numSamples > 0 && !suspended_flush;
        let frames = data.numSamples.max(0) as u32;

        if will_process {
            backend.begin_input_events(frames);
        }
        // SAFETY: `data`'s event and parameter pointers are the host's for this `process` call,
        // which is `ingest_inputs`'s contract.
        let dropped = unsafe {
            self.ingest_inputs(data.inputEvents, data.inputParameterChanges, will_process)
        };
        if !will_process {
            return kResultOk;
        }
        // SAFETY: `processContext` is null or the host's context for this call.
        let transport = unsafe { data.processContext.as_ref() }.map(Self::transport);

        // SAFETY: Only `process` and `setupProcessing` touch `scratch`, and the host never overlaps
        // them; `ingest_inputs`'s borrow has ended.
        let scratch = unsafe { &mut *self.scratch.get() };
        let use64 = cfg.sample_size == SymbolicSampleSizes_::kSample64 as i32;
        let in_active = self.in_active.load(Ordering::Acquire);
        let out_active = self.out_active.load(Ordering::Acquire);

        let num_in = gather(
            data.inputs,
            data.numInputs,
            &cfg.in_bus_channels,
            in_active,
            use64,
            &mut scratch.in_ptrs,
            <*mut c_void>::cast_const,
        );
        let num_out = gather(
            data.outputs,
            data.numOutputs,
            &cfg.out_bus_channels,
            out_active,
            use64,
            &mut scratch.out_ptrs,
            |p| p,
        );
        let (mut num_in, mut num_out) = (num_in, num_out);

        if frames <= cfg.max_frames {
            let max_frames = cfg.max_frames as usize;
            if !scratch.silence.is_empty() {
                let silence = scratch.silence.as_ptr().cast::<c_void>();
                let want = (cfg.total_in as usize).min(scratch.in_ptrs.len());
                for slot in &mut scratch.in_ptrs[..num_in] {
                    if slot.is_null() {
                        *slot = silence;
                    }
                }
                while num_in < want {
                    scratch.in_ptrs[num_in] = silence;
                    num_in += 1;
                }
            }
            if !scratch.trash.is_empty() && max_frames > 0 {
                let trash = scratch.trash.as_mut_ptr();
                let blocks = scratch.trash.len() / max_frames;
                // SAFETY: `c.min(blocks - 1) * max_frames` stays inside `trash`, which holds
                // `blocks >= 1` (non-empty) blocks of `max_frames` samples.
                let block = |c: usize| unsafe {
                    trash.add(c.min(blocks - 1) * max_frames).cast::<c_void>()
                };
                let want = (cfg.total_out as usize).min(scratch.out_ptrs.len());
                for (c, slot) in scratch.out_ptrs[..num_out].iter_mut().enumerate() {
                    if slot.is_null() {
                        *slot = block(c);
                    }
                }
                while num_out < want {
                    scratch.out_ptrs[num_out] = block(num_out);
                    num_out += 1;
                }
            }
        }

        // In-place processing starts from the input.
        let bytes = frames as usize * if use64 { 8 } else { 4 };
        for c in 0..num_in.min(num_out) {
            let (src, dst) = (scratch.in_ptrs[c], scratch.out_ptrs[c]);
            if !src.is_null() && !dst.is_null() && src != dst.cast_const() {
                // SAFETY: `src` and `dst` are non-null channel buffers of at least `frames` samples
                // at the negotiated width (host buffers by contract, or our `silence`/`trash`,
                // sized to `max_frames >= frames`); `ptr::copy` tolerates overlap.
                unsafe { std::ptr::copy(src.cast::<u8>(), dst.cast::<u8>(), bytes) };
            }
        }

        let mut args = ProcessArgs {
            inputs: &scratch.in_ptrs[..num_in],
            outputs: &mut scratch.out_ptrs[..num_out],
            in_bus_channels: &cfg.in_bus_channels,
            in_bus_active: in_active,
            out_bus_channels: &cfg.out_bus_channels,
            out_bus_active: out_active,
            num_frames: frames,
            transport: transport.as_ref(),
            process_mode: data.processMode,
        };
        PROCESSING.with(|p| p.set(self));
        let ok = if use64 {
            // SAFETY: `args` holds `num_in`/`num_out` channel pointers, each null or `frames`
            // 64-bit samples long, which is `process_f64`'s contract.
            unsafe { backend.process_f64(&mut args) }
        } else {
            // SAFETY: `args` holds `num_in`/`num_out` channel pointers, each null or `frames`
            // 32-bit samples long, which is `process_f32`'s contract.
            unsafe { backend.process_f32(&mut args) }
        };
        PROCESSING.with(|p| p.set(std::ptr::null()));
        if dropped {
            // SAFETY: Same live host parameter list as above, still within this `process` call.
            unsafe { self.commit_final_values(data.inputParameterChanges) };
        }

        if !ok {
            backend.finish_output_events(OUTPUT_INVALID);
            return kResultOk;
        }

        if !data.outputs.is_null() {
            let buses =
                (desc.num_output_buses().min(32) as usize).min(data.numOutputs.max(0) as usize);
            for b in 0..buses {
                if out_active & (1 << b) != 0 {
                    // SAFETY: `data.outputs` is non-null and `b < numOutputs`, so this is one of
                    // the host's output bus structs.
                    unsafe { (*data.outputs.add(b)).silenceFlags = 0 };
                }
            }
        }

        let out_params = backend.output_param_count();
        // SAFETY: `outputEvents` is the host's list for this `process` call (null or live).
        let mut result = unsafe { self.drain_output_events(data.outputEvents) };
        if out_params > 0 && matches!(result, OUTPUT_END | OUTPUT_EMITTED) {
            result = if data.outputParameterChanges.is_null() {
                OUTPUT_UNSUPPORTED
            } else {
                // SAFETY: `outputParameterChanges` is non-null (checked) and live for this call.
                unsafe { self.write_output_params(data.outputParameterChanges, out_params, result) }
            };
        }
        backend.finish_output_events(result);
        kResultOk
    }

    fn bus_channels(&self, dir: i32, index: u32) -> u32 {
        let cfg = self.config();
        let widths = if dir == BusDirections_::kInput as i32 {
            &cfg.in_bus_channels
        } else {
            &cfg.out_bus_channels
        };
        widths.get(index as usize).copied().unwrap_or(0)
    }
}

/// Flatten host buses into `slots` (declared width per bus); returns the
/// channel count written.
fn gather<T>(
    buses: *mut AudioBusBuffers,
    count: i32,
    widths: &[u32],
    active: u32,
    use64: bool,
    slots: &mut [T],
    conv: impl Fn(*mut c_void) -> T,
) -> usize {
    let mut n = 0;
    for (b, &declared) in widths.iter().enumerate() {
        let bus =
            // SAFETY: `buses` is non-null and `b < count`, so this is one of the host's bus
            // structs.
            (!buses.is_null() && b < count.max(0) as usize).then(|| unsafe { &*buses.add(b) });
        let on = b < 32 && active & (1 << b) != 0;
        // SAFETY: The union arm read matches the negotiated sample size (`use64`), as the SDK
        // requires.
        let channels = bus.and_then(|bus| unsafe {
            let p = if use64 {
                bus.__field0.channelBuffers64.cast::<*mut c_void>()
            } else {
                bus.__field0.channelBuffers32.cast::<*mut c_void>()
            };
            (!p.is_null()).then_some((p, bus.numChannels.max(0) as u32))
        });
        for c in 0..declared {
            let Some(slot) = slots.get_mut(n) else {
                return n;
            };
            *slot = conv(match channels {
                // SAFETY: `p` is non-null and `c < supplied` (the bus's `numChannels`), so the read
                // stays inside the host's channel array.
                Some((p, supplied)) if on && c < supplied => unsafe { *p.add(c as usize) },
                _ => std::ptr::null_mut(),
            });
            n += 1;
        }
    }
    n
}

impl Drop for Host {
    fn drop(&mut self) {
        // No new editor call can find this instance; one already inside
        // holds an `Arc`, so none is running.
        super::unregister_instance(self.id);
        self.wakes.closing.store(true, Ordering::Release);
        if let Some(backend) = self.backend.as_deref() {
            // Unbinds and joins catalog/persist wakers (which share `Arc<HostWakes>`,
            // never `&Host`).
            backend.set_catalog_waker(None);
            if self.persist_bound.load(Ordering::Acquire) {
                backend.set_persist_waker(None);
            }
            // Closes `HostQueue` and joins any in-flight `wake_host_queue` while
            // `self` and its OS wakes are still alive.
            // SAFETY: Unbinding with `None` has no pointer precondition; it closes `HostQueue`
            // while `self` is still whole.
            unsafe { backend.bind_host_queue(0, None) };
        }
        // Queued editor calls go out while the handler is still held, if
        // this thread may make them (the last reference can be a worker's).
        {
            let e = lock(&self.edits);
            let mut e = if self.may_deliver(&e) {
                let e = self.drain_locked(e, true);
                if e.pending.is_empty() {
                    e
                } else {
                    self.close_open_gestures(e).0
                }
            } else {
                e
            };
            e.closed = true;
            e.pending.clear();
            #[cfg(target_os = "linux")]
            {
                self.wakes.edit_fd.store(-1, Ordering::Release);
                drop(e.wake.take());
            }
        }
        #[cfg(target_os = "linux")]
        {
            self.wakes.restart_fd.store(-1, Ordering::Release);
            lock(&self.persist_wake).take();
            lock(&self.timer).take();
        }
        #[cfg(target_os = "macos")]
        if let Some(wakes) = Arc::get_mut(&mut self.wakes) {
            drop(wakes.main_queue.take());
        }
        #[cfg(target_os = "windows")]
        {
            // Cancel and join any persist-retry worker BEFORE destroying
            // `MessageWake` and releasing the DLL module reference.
            self.wakes.stop_persist_retry();
            if let Some(wakes) = Arc::get_mut(&mut self.wakes) {
                drop(wakes.message_wake.take());
            }
        }
        lock(&self.handler).take();
        if let Some(backend) = self.backend.take() {
            // Firewall panics during plugin/editor teardown so they cannot
            // unwind across COM `IUnknown::release`.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(backend)));
        }
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn main_queue_event(ctx: *mut c_void) {
    if let Some(host) = super::pin(ctx as usize) {
        host.on_ui_tick();
    }
}

#[cfg(target_os = "windows")]
fn win_message_event(ctx: *mut c_void) {
    if let Some(host) = super::pin(ctx as usize) {
        if let Some(w) = host.wakes.message_wake.get() {
            w.clear_pending();
        }
        host.on_ui_tick();
    }
}

// ---------------------------------------------------------------------------
// COM classes
// ---------------------------------------------------------------------------

struct ContextMenuTarget {
    host: std::sync::Weak<Host>,
    target: MenuTarget,
}

impl Class for ContextMenuTarget {
    type Interfaces = (IContextMenuTarget,);
}

impl IContextMenuTargetTrait for ContextMenuTarget {
    unsafe fn executeMenuItem(&self, tag: int32) -> tresult {
        let Some(host) = self.host.upgrade() else {
            return kResultFalse;
        };
        #[allow(clippy::cast_sign_loss, reason = "tags are the u32 actions we set")]
        let action = tag as u32;
        if host.backend().perform_menu(self.target, action) {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

pub(super) struct Component {
    pub host: Arc<Host>,
}

/// `com-scrape-types` implements `InterfaceList` only for tuples up to arity
/// 15; this struct carries all 18 COM interfaces exposed by [`Component`].
pub(super) struct ComponentInterfaces;

macro_rules! component_interface_list {
    ($($field:ident : $iface:ty => $index:tt),+ $(,)?) => {
        #[repr(C)]
        pub(super) struct ComponentHeader {
            $($field: $iface),+
        }

        // SAFETY: `ComponentHeader` is `#[repr(C)]` of single-pointer COM
        // interface structs (`*const Vtbl`), so slot `$index` sits at byte
        // offset `$index * size_of::<*mut ()>()`.
        unsafe impl vst3::com_scrape_types::InterfaceList for ComponentInterfaces {
            type Header = ComponentHeader;

            fn query(iid: &vst3::com_scrape_types::Guid) -> Option<isize> {
                $(
                    if <$iface as vst3::com_scrape_types::Interface>::inherits(iid) {
                        return Some(($index as isize) * (std::mem::size_of::<*mut ()>() as isize));
                    }
                )+
                None
            }
        }

        // SAFETY: every slot is initialized with `<I as Construct<C, W, OFFSET>>::OBJ`
        // matching the exact offset returned by `InterfaceList::query`.
        unsafe impl<C, W> vst3::com_scrape_types::MakeHeader<C, W> for ComponentInterfaces
        where
            C: vst3::Class,
            W: vst3::com_scrape_types::Wrapper<C>,
            $(
                $iface: vst3::com_scrape_types::Construct<
                    C,
                    W,
                    { ($index as isize) * (std::mem::size_of::<*mut ()>() as isize) },
                >,
            )+
        {
            const HEADER: Self::Header = ComponentHeader {
                $($field: <$iface as vst3::com_scrape_types::Construct<
                    C,
                    W,
                    { ($index as isize) * (std::mem::size_of::<*mut ()>() as isize) },
                >>::OBJ),+
            };
        }
    };
}

component_interface_list!(
    i0: IComponent => 0,
    i1: IAudioProcessor => 1,
    i2: IAudioPresentationLatency => 2,
    i3: IEditController => 3,
    i4: IEditControllerHostEditing => 4,
    i5: IMidiMapping => 5,
    i6: IMidiMapping2 => 6,
    i7: IProcessContextRequirements => 7,
    i8: IUnitInfo => 8,
    i9: IProgramListData => 9,
    i10: INoteExpressionController => 10,
    i11: INoteExpressionPhysicalUIMapping => 11,
    i12: IKeyswitchController => 12,
    i13: IParameterFunctionName => 13,
    i14: IRemapParamID => 14,
    i15: IInfoListener => 15,
    i16: IMidiLearn => 16,
    i17: IMidiLearn2 => 17,
);

impl Class for Component {
    type Interfaces = ComponentInterfaces;
}

impl IAudioPresentationLatencyTrait for Component {
    unsafe fn setAudioPresentationLatencySamples(
        &self,
        dir: BusDirection,
        bus_index: int32,
        latency_in_samples: uint32,
    ) -> tresult {
        let Some(is_in) = is_dir(dir) else {
            return kInvalidArgument;
        };
        let d = &self.host.reg.desc;
        let bus_count = if is_in {
            d.num_input_buses()
        } else {
            d.num_output_buses()
        };
        let Ok(bus) = u32::try_from(bus_index) else {
            return kInvalidArgument;
        };
        if bus >= bus_count {
            return kInvalidArgument;
        }
        self.host
            .backend()
            .set_audio_presentation_latency(is_in, bus, latency_in_samples);
        kResultOk
    }
}

fn is_dir(dir: BusDirection) -> Option<bool> {
    match dir {
        d if d == BusDirections_::kInput as i32 => Some(true),
        d if d == BusDirections_::kOutput as i32 => Some(false),
        _ => None,
    }
}

impl IPluginBaseTrait for Component {
    unsafe fn initialize(&self, context: *mut FUnknown) -> tresult {
        #[cfg(target_os = "linux")]
        // SAFETY: `initialize`'s `context` is null or the host application
        // object, live for this call.
        if let Some(rl) = unsafe { super::wake::linux::run_loop_of(context) } {
            self.host.bind_persist_run_loop(rl);
        }
        // SAFETY: as above; `set_host_context` retains it before returning.
        #[cfg(target_os = "linux")]
        unsafe {
            self.host.set_host_context(context);
        }
        // SAFETY: `context` is null or the host's live context for this call.
        if let Some(name) = unsafe { super::host_app_name(context) } {
            self.host
                .set_dynamic_support(DynamicSupport::for_host(&name));
            self.host.backend().set_host_name(&name);
        }
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IComponentTrait for Component {
    unsafe fn getControllerClassId(&self, _class_id: *mut TUID) -> tresult {
        kNotImplemented
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, kind: MediaType, dir: BusDirection) -> int32 {
        let d = &self.host.reg.desc;
        let Some(input) = is_dir(dir) else {
            return 0;
        };
        match kind {
            k if k == MediaTypes_::kAudio as i32 => {
                (if input {
                    d.num_input_buses()
                } else {
                    d.num_output_buses()
                }) as i32
            }
            k if k == MediaTypes_::kEvent as i32 => {
                if input {
                    d.midi_input_ports
                } else {
                    d.midi_output_ports
                }
            }
            _ => 0,
        }
    }

    unsafe fn getBusInfo(
        &self,
        kind: MediaType,
        dir: BusDirection,
        index: int32,
        bus: *mut BusInfo,
    ) -> tresult {
        // SAFETY: `bus` is null (rejected) or the host's `BusInfo` out slot for this call.
        let (Some(bus), Some(input)) = (unsafe { bus.as_mut() }, is_dir(dir)) else {
            return kInvalidArgument;
        };
        if index < 0 {
            return kInvalidArgument;
        }
        let d = &self.host.reg.desc;
        let default_active = BusInfo_::BusFlags_::kDefaultActive as u32;
        if kind == MediaTypes_::kEvent as i32 {
            let ports = if input {
                d.midi_input_ports
            } else {
                d.midi_output_ports
            };
            if index >= ports {
                return kInvalidArgument;
            }
            bus.mediaType = kind;
            bus.direction = dir;
            bus.channelCount = 16;
            let base = if input { "Event In" } else { "Event Out" };
            let name = if ports > 1 {
                format!("{base} {}", index + 1)
            } else {
                base.into()
            };
            utf16_into(&mut bus.name, &name);
            bus.busType = BusTypes_::kMain as i32;
            bus.flags = default_active;
            return kResultOk;
        }
        if kind != MediaTypes_::kAudio as i32 {
            return kInvalidArgument;
        }
        let kinds = if input {
            &d.input_bus_kinds
        } else {
            &d.output_bus_kinds
        };
        let Some(&bus_kind) = kinds.get(index as usize) else {
            return kInvalidArgument;
        };
        let aux = bus_kind != 0;
        bus.mediaType = kind;
        bus.direction = dir;
        bus.channelCount = self.host.bus_channels(dir, index as u32) as i32;
        let name = match (input, aux) {
            (true, false) => "Input",
            (true, true) => "Sidechain",
            (false, false) => "Output",
            (false, true) => "Aux Output",
        };
        utf16_into(&mut bus.name, name);
        bus.busType = if aux {
            BusTypes_::kAux as i32
        } else {
            BusTypes_::kMain as i32
        };
        bus.flags = if aux { 0 } else { default_active };
        kResultOk
    }

    unsafe fn getRoutingInfo(
        &self,
        _in_info: *mut RoutingInfo,
        _out_info: *mut RoutingInfo,
    ) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(
        &self,
        kind: MediaType,
        dir: BusDirection,
        index: int32,
        state: TBool,
    ) -> tresult {
        let d = &self.host.reg.desc;
        let Some(input) = is_dir(dir) else {
            return kInvalidArgument;
        };
        if index < 0 {
            return kInvalidArgument;
        }
        if kind == MediaTypes_::kEvent as i32 {
            let count = if input {
                d.midi_input_ports
            } else {
                d.midi_output_ports
            };
            return if index < count {
                kResultOk
            } else {
                kInvalidArgument
            };
        }
        if kind != MediaTypes_::kAudio as i32 {
            return kInvalidArgument;
        }
        let count = if input {
            d.num_input_buses()
        } else {
            d.num_output_buses()
        };
        if index as u32 >= count || index >= 32 {
            return kInvalidArgument;
        }
        let mask = if input {
            &self.host.in_active
        } else {
            &self.host.out_active
        };
        if state != 0 {
            mask.fetch_or(1 << index, Ordering::Release);
        } else {
            mask.fetch_and(!(1 << index), Ordering::Release);
        }
        kResultOk
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        let h = &self.host;
        let backend = h.backend();
        let cfg = h.config();
        if state != 0 {
            if !backend.prepare(cfg.sample_rate, cfg.max_frames, cfg.process_mode) {
                return kResultFalse;
            }
        } else {
            backend.reset(cfg.sample_rate, cfg.max_frames, cfg.process_mode);
        }
        backend.set_active(state != 0);
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        // SAFETY: `state` is null (rejected) or the host's live stream for this call.
        let Some(stream) = (unsafe { ComRef::from_raw(state) }) else {
            return kResultFalse;
        };
        // SAFETY: `stream` is the host's live stream for this call.
        let context = unsafe { stream_state_context(stream) };
        if self
            .host
            .backend()
            .state_load_stream_with_context(context, &mut |dst| {
                super::read_state_chunk(stream, dst)
            })
        {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        // SAFETY: `state` is null (rejected) or the host's live stream for this call.
        let Some(stream) = (unsafe { ComRef::from_raw(state) }) else {
            return kResultFalse;
        };
        // SAFETY: `stream` is the host's live stream for this call.
        let context = unsafe { stream_state_context(stream) };
        if self
            .host
            .backend()
            .state_save_stream_with_context(context, &mut |src| {
                super::write_state_chunk(stream, src)
            })
        {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

/// Query `IStreamAttributes` on a VST3 `IBStream` and map
/// `PresetAttributes::kStateType` (`StateType::{kProject,kDefault,kTrackPreset}`)
/// into [`moose_core::state::StateContext`].
unsafe fn stream_state_context(stream: ComRef<'_, IBStream>) -> moose_core::state::StateContext {
    let Some(attrs) = stream.cast::<IStreamAttributes>() else {
        return moose_core::state::StateContext::Unknown;
    };
    // SAFETY: `IStreamAttributes::getAttributes()` returns a borrowed
    // `*mut IAttributeList` (no extra AddRef) valid while `attrs` is alive.
    let list_ptr = unsafe { attrs.getAttributes() };
    // SAFETY: `list_ptr` is null (rejected) or a borrowed list live while `attrs` is.
    let Some(list) = (unsafe { ComRef::from_raw(list_ptr) }) else {
        return moose_core::state::StateContext::Unknown;
    };
    let mut buf = [0 as TChar; 64];
    // SAFETY: `list` is live; `kStateType` is a static C string and `buf` is a local
    // buffer whose byte size is passed as the capacity.
    let res = unsafe {
        list.getString(
            PresetAttributes::kStateType,
            buf.as_mut_ptr(),
            size_of_val(&buf) as u32,
        )
    };
    if res != kResultOk {
        return moose_core::state::StateContext::Unknown;
    }
    // SAFETY: `buf` is a local, fully initialised array of `buf.len()` UTF-16 units.
    let state_type = unsafe { utf16_from(buf.as_ptr(), buf.len()) };
    parse_vst3_state_type(&state_type)
}

fn parse_vst3_state_type(state_type: &str) -> moose_core::state::StateContext {
    // SAFETY: `StateType::k*` constants in `vst3` are static NUL-terminated ASCII C strings.
    let k_project = unsafe { cstr(StateType::kProject) };
    // SAFETY: As above, a static NUL-terminated C string.
    let k_default = unsafe { cstr(StateType::kDefault) };
    // SAFETY: As above, a static NUL-terminated C string.
    let k_track_preset = unsafe { cstr(StateType::kTrackPreset) };
    if state_type == k_project {
        moose_core::state::StateContext::Project
    } else if state_type == k_default {
        moose_core::state::StateContext::Default
    } else if state_type == k_track_preset || state_type == "Preset" {
        moose_core::state::StateContext::Preset
    } else {
        moose_core::state::StateContext::Unknown
    }
}

impl IAudioProcessorTrait for Component {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut SpeakerArrangement,
        num_ins: int32,
        outputs: *mut SpeakerArrangement,
        num_outs: int32,
    ) -> tresult {
        let h = &self.host;
        let d = &h.reg.desc;
        if num_ins != d.num_input_buses() as i32 || num_outs != d.num_output_buses() as i32 {
            return kResultFalse;
        }
        if (num_ins > 0 && inputs.is_null()) || (num_outs > 0 && outputs.is_null()) {
            return kResultFalse;
        }
        let widths = |arr: *mut SpeakerArrangement, n: i32| -> Vec<u32> {
            (0..n.max(0) as usize)
                .map(|i| {
                    if arr.is_null() {
                        0
                    } else {
                        // SAFETY: `arr` is non-null and `i < n`, the array length the host passed.
                        unsafe { *arr.add(i) }.count_ones()
                    }
                })
                .collect()
        };
        let ins = widths(inputs, num_ins);
        let outs = widths(outputs, num_outs);
        if ins.iter().enumerate().any(|(i, &width)| {
            // SAFETY: `inputs` is non-null (checked above when `num_ins > 0`) and `i < num_ins`.
            width == 0 || unsafe { *inputs.add(i) } != speaker_arrangement(width)
        }) || outs.iter().enumerate().any(|(i, &width)| {
            // SAFETY: `outputs` is non-null (checked above when `num_outs > 0`) and `i < num_outs`.
            width == 0 || unsafe { *outputs.add(i) } != speaker_arrangement(width)
        }) {
            return kResultFalse;
        }
        let Some(layout) = h.reg.factory.match_bus_layout_perbus(&ins, &outs) else {
            return kResultFalse;
        };
        // SAFETY: `setBusArrangements` is only legal while inactive, when no other call touches
        // `Config`.
        let cfg = unsafe { h.config_mut() };
        cfg.layout = layout;
        refresh_bus_channels(h.reg, cfg);
        cfg.total_in = ins.iter().sum();
        cfg.total_out = outs.iter().sum();
        kResultOk
    }

    unsafe fn getBusArrangement(
        &self,
        dir: BusDirection,
        index: int32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        let d = &self.host.reg.desc;
        let count = if dir == BusDirections_::kInput as i32 {
            d.num_input_buses()
        } else {
            d.num_output_buses()
        };
        if arr.is_null() || index < 0 || index as u32 >= count {
            return kInvalidArgument;
        }
        let ch = self.host.bus_channels(dir, index as u32);
        if ch == 0 || ch > 64 {
            return kInvalidArgument;
        }
        // SAFETY: `arr` is non-null (checked above) and is the host's out slot.
        unsafe { *arr = speaker_arrangement(ch) };
        kResultOk
    }

    unsafe fn canProcessSampleSize(&self, size: int32) -> tresult {
        let ok = size == SymbolicSampleSizes_::kSample32 as i32
            || (size == SymbolicSampleSizes_::kSample64 as i32 && self.host.reg.desc.supports_f64);
        if ok { kResultOk } else { kResultFalse }
    }

    unsafe fn getLatencySamples(&self) -> uint32 {
        self.host.backend().get_latency()
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        // SAFETY: `setup` is null (rejected) or the host's `ProcessSetup` for this call.
        let Some(setup) = (unsafe { setup.as_ref() }) else {
            return kInvalidArgument;
        };
        // SAFETY: `canProcessSampleSize` has no preconditions.
        if unsafe { self.canProcessSampleSize(setup.symbolicSampleSize) } != kResultOk {
            return kResultFalse;
        }
        let h = &self.host;
        // SAFETY: `setupProcessing` is only legal while inactive, when no other call touches
        // `Config`.
        let cfg = unsafe { h.config_mut() };
        cfg.sample_size = setup.symbolicSampleSize;
        cfg.sample_rate = setup.sampleRate;
        cfg.max_frames = setup.maxSamplesPerBlock.max(0) as u32;
        cfg.process_mode = setup.processMode;
        let frames = cfg.max_frames as usize;
        let d = &h.reg.desc;
        // SAFETY: `setupProcessing` never overlaps `process`, the only other user of `scratch`.
        let scratch = unsafe { &mut *h.scratch.get() };
        scratch.silence = if d.max_inputs > 0 {
            vec![0.0; frames]
        } else {
            Vec::new()
        };
        scratch.trash = vec![0.0; frames * d.max_outputs as usize];
        kResultOk
    }

    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        let h = &self.host;
        let cfg = h.config();
        h.backend().set_processing(
            state != 0,
            cfg.sample_rate,
            cfg.max_frames,
            cfg.process_mode,
        );
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        // SAFETY: `data` is null (handled) or the host's `ProcessData`, exclusively ours for this
        // call.
        match unsafe { data.as_mut() } {
            // SAFETY: `data` holds the host's buffers, event lists and parameter changes for this
            // call, which is `Host::process`'s contract.
            Some(data) => unsafe { self.host.process(data) },
            None => kResultOk,
        }
    }

    unsafe fn getTailSamples(&self) -> uint32 {
        self.host.backend().get_tail()
    }
}

impl IEditControllerTrait for Component {
    unsafe fn setComponentState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> int32 {
        let base = self.host.param_count() as i32;
        let has_programs = !self.host.backend().program_list().is_empty();
        base + i32::from(has_programs)
    }

    unsafe fn getParameterInfo(&self, index: int32, info: *mut ParameterInfo) -> tresult {
        let reg = self.host.reg;
        let backend = self.host.backend();
        // SAFETY: `info` is null (rejected) or the host's out slot for this call.
        let Some(info) = (unsafe { info.as_mut() }) else {
            return kInvalidArgument;
        };
        let base_count = self.host.param_count() as i32;
        if index == base_count {
            use ParameterInfo_::ParameterFlags_::*;
            let programs = backend.program_list();
            if programs.is_empty() {
                return kInvalidArgument;
            }
            info.id = self.host.program_change_id;
            utf16_into(&mut info.title, "Program");
            utf16_into(&mut info.shortTitle, "Prog");
            utf16_into(&mut info.units, "");
            info.stepCount = programs.len().saturating_sub(1) as i32;
            info.defaultNormalizedValue = 0.0;
            info.unitId = 0;
            info.flags = (kIsProgramChange | kIsList) as i32;
            return kResultOk;
        }
        let Some(index) = self.host.param_index(index) else {
            return kInvalidArgument;
        };
        let p = &reg.params[index];
        info.id = p.id;
        utf16_into(&mut info.title, &p.name);
        utf16_into(&mut info.shortTitle, &p.short_name);
        utf16_into(&mut info.units, &p.units);
        info.stepCount = p.step_count;
        info.defaultNormalizedValue = p.default_normalized;
        if let Some((live_step, live_default)) = backend.param_step_count(p.id) {
            info.stepCount = live_step;
            if let Some(d) = live_default {
                info.defaultNormalizedValue = d;
            }
        }
        info.unitId = reg.param_unit[index];
        info.flags = p.flags;
        if let Some(presentation) = backend.param_presentation(p.id) {
            utf16_into(&mut info.title, &presentation.name);
            utf16_into(&mut info.shortTitle, &presentation.name);
            if presentation.hidden || presentation.unavailable {
                use ParameterInfo_::ParameterFlags_::*;
                info.flags =
                    (info.flags | kIsReadOnly as i32 | kIsHidden as i32) & !(kCanAutomate as i32);
            }
        }
        kResultOk
    }

    unsafe fn getParamStringByValue(
        &self,
        id: ParamID,
        normalized: ParamValue,
        string: *mut String128,
    ) -> tresult {
        // SAFETY: `string` is null (rejected) or the host's `String128` out slot.
        let Some(string) = (unsafe { string.as_mut() }) else {
            return kInvalidArgument;
        };
        let h = &self.host;
        let backend = h.backend();
        if id == h.program_change_id {
            let programs = backend.program_list();
            let Some(preset) = program_index_from_normalized(normalized, programs.len())
                .and_then(|i| programs.get(i))
            else {
                return kInvalidArgument;
            };
            utf16_into(string, &preset.name);
            return kResultOk;
        }
        let plain = backend.param_denormalize(id, normalized);
        if let Some(text) = backend.param_format(id, plain) {
            utf16_into(string, &text);
        } else {
            utf16_into(string, &format!("{plain:.2}"));
        }
        kResultOk
    }

    unsafe fn getParamValueByString(
        &self,
        id: ParamID,
        string: *mut TChar,
        normalized: *mut ParamValue,
    ) -> tresult {
        if string.is_null() || normalized.is_null() {
            return kInvalidArgument;
        }
        let h = &self.host;
        let backend = h.backend();
        // SAFETY: `string` is non-null (checked) and NUL-terminated within the host's `String128`
        // (256 bytes covers it).
        let raw_text = unsafe { utf16_from(string, 256) };
        if id == h.program_change_id {
            let programs = backend.program_list();
            let Some(idx) = programs.iter().position(|p| p.name == raw_text) else {
                return kResultFalse;
            };
            // SAFETY: `normalized` is non-null (checked) and is the host's out slot.
            unsafe { *normalized = program_normalized_from_index(idx, programs.len()) };
            return kResultOk;
        }
        let Some(plain) = backend.param_parse(id, &raw_text) else {
            return kResultFalse;
        };
        // SAFETY: `normalized` is non-null (checked) and is the host's out slot.
        unsafe { *normalized = backend.param_normalize(id, plain) };
        kResultOk
    }

    unsafe fn normalizedParamToPlain(&self, id: ParamID, normalized: ParamValue) -> ParamValue {
        let h = &self.host;
        let backend = h.backend();
        if id == h.program_change_id {
            let programs = backend.program_list();
            return program_index_from_normalized(normalized, programs.len()).unwrap_or(0) as f64;
        }
        backend.param_denormalize(id, normalized)
    }

    unsafe fn plainParamToNormalized(&self, id: ParamID, plain: ParamValue) -> ParamValue {
        let h = &self.host;
        let backend = h.backend();
        if id == h.program_change_id {
            let programs = backend.program_list();
            if programs.len() <= 1 {
                return 0.0;
            }
            let idx = (plain.round().clamp(0.0, (programs.len() - 1) as f64)) as usize;
            return program_normalized_from_index(idx, programs.len());
        }
        backend.param_normalize(id, plain)
    }

    unsafe fn getParamNormalized(&self, id: ParamID) -> ParamValue {
        let h = &self.host;
        if h.may_deliver(&lock(&h.edits)) {
            h.on_ui_tick();
        }
        let backend = h.backend();
        if id == h.program_change_id {
            let programs = backend.program_list();
            let idx = usize::try_from(h.selected_program.load(Ordering::Acquire))
                .unwrap_or(0)
                .min(programs.len().saturating_sub(1));
            return program_normalized_from_index(idx, programs.len());
        }
        let plain = backend.param_get_value(id);
        backend.param_normalize(id, plain)
    }

    unsafe fn setParamNormalized(&self, id: ParamID, value: ParamValue) -> tresult {
        let h = &self.host;
        if h.in_perform_edit.load(Ordering::Relaxed) {
            return kResultOk;
        }
        let backend = h.backend();
        if id == h.program_change_id {
            let programs = backend.program_list();
            let Some(idx) = program_index_from_normalized(value, programs.len()) else {
                return kInvalidArgument;
            };
            if !programs[idx].state.is_empty() {
                let mut slice = programs[idx].state.as_slice();
                if !backend.state_load_stream(&mut |dst| {
                    let take = slice.len().min(dst.len());
                    dst[..take].copy_from_slice(&slice[..take]);
                    slice = &slice[take..];
                    take as i64
                }) {
                    return kResultFalse;
                }
            }
            h.selected_program.store(idx as i32, Ordering::Release);
            return kResultOk;
        }
        let plain = backend.param_denormalize(id, value);
        backend.param_set_value(id, plain);
        h.note_param_change(id);
        kResultOk
    }

    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        let h = &self.host;
        // SAFETY: `handler` is null or the host's live handler; `retain` adds the reference we
        // keep.
        let handler = unsafe { retain(handler) };
        let has_handler = handler.is_some();
        let has_handler3 = handler
            .as_ref()
            .and_then(ComPtr::cast::<IComponentHandler3>)
            .is_some();
        // Hold the queue lock across the swap so concurrent editor calls land
        // on the right side of it.
        let e = h.drain_locked(lock(&h.edits), true);
        // A submitter that never stopped: its gestures end on the old handler
        // and continue in fresh ones on the new.
        // An open group edit finishes there too and restarts first.
        let (mut e, reopen) = h.close_open_gestures(e);
        let previous = std::mem::replace(&mut *lock(&h.handler), handler);
        h.has_handler3.store(has_handler3, Ordering::Release);
        if has_handler {
            for &id in reopen.gestures.iter().rev() {
                e.pending.push_front(EditOp::Begin(id));
            }
            if reopen.group {
                e.pending.push_front(EditOp::Group(EditGroup::Begin));
            }
        }
        if !e.pending.is_empty() {
            h.wake_edits(&e);
        }
        drop(e);
        drop(previous);
        if has_handler {
            h.flush_dirty();
            h.flush_persist_change();
        }
        kResultOk
    }

    unsafe fn createView(&self, _name: FIDString) -> *mut IPlugView {
        View::create(&self.host)
    }
}

impl IEditControllerHostEditingTrait for Component {
    unsafe fn beginEditFromHost(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn endEditFromHost(&self, _id: ParamID) -> tresult {
        kResultOk
    }
}

impl IMidiMappingTrait for Component {
    unsafe fn getMidiControllerAssignment(
        &self,
        bus: int32,
        channel: int16,
        controller: CtrlNumber,
        id: *mut ParamID,
    ) -> tresult {
        // SAFETY: `id` is null (rejected) or the host's out slot.
        let Some(out) = (unsafe { id.as_mut() }) else {
            return kResultFalse;
        };
        if let Some(pid) = self
            .host
            .backend()
            .midi_mapping_get_param_id(bus, channel, controller)
        {
            *out = pid;
            kResultOk
        } else {
            kResultFalse
        }
    }
}

impl Component {
    fn midi1_assignments(&self, mut each: impl FnMut(Midi1ControllerParamIDAssignment)) {
        let h = &self.host;
        let backend = h.backend();
        for bus in 0..h.reg.desc.midi_input_ports.max(0) {
            for channel in 0..16i16 {
                for controller in 0..=130i16 {
                    if let Some(id) = backend.midi_mapping_get_param_id(bus, channel, controller) {
                        each(Midi1ControllerParamIDAssignment {
                            pId: id,
                            busIndex: bus,
                            channel: channel as MidiChannel,
                            controller,
                        });
                    }
                }
            }
        }
    }
}

/// Exact 12-byte C++ VST3 SDK 3.8 ABI layout for
/// `Steinberg::Vst::Midi2ControllerParamIDAssignment`.
///
/// The `vst3 0.3.0` crate expands the bitfields of `Midi2Controller` into
/// four `u8` fields (`sizeof == 16`), whereas C++ hosts allocate 12-byte
/// entries (`ParamID` + `BusIndex` + `MidiChannel` + 2-byte bitfield
/// `Midi2Controller` + 1 pad byte). Writing `AbiMidi2Assignment` records
/// directly preserves exact C++ ABI fidelity.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AbiMidi2Assignment {
    p_id: ParamID,
    bus_index: BusIndex,
    channel: MidiChannel,
    byte1: u8,
    byte2: u8,
    _pad: u8,
}

const _: () = {
    assert!(size_of::<AbiMidi2Assignment>() == 12);
    assert!(align_of::<AbiMidi2Assignment>() == 4);
};

fn vst3_bus_direction(direction: BusDirections) -> Option<moose_core::main_thread::BusDirection> {
    if direction == BusDirections_::kInput as BusDirections {
        Some(moose_core::main_thread::BusDirection::Input)
    } else if direction == BusDirections_::kOutput as BusDirections {
        Some(moose_core::main_thread::BusDirection::Output)
    } else {
        None
    }
}

fn is_valid_midi2_assignment(a: &moose_core::main_thread::Midi2ControllerAssignment) -> bool {
    a.bus_index >= 0 && a.channel < 16 && a.controller.bank <= 0x7F && a.controller.index <= 0x7F
}

/// SDK 3.8 list form of `IMidiMapping`: serves both MIDI 2.0 Registered/Assignable
/// controller assignments and MIDI 1.0 CC assignments.
impl IMidiMapping2Trait for Component {
    unsafe fn getNumMidi2ControllerAssignments(&self, direction: BusDirections) -> uint32 {
        let Some(dir) = vst3_bus_direction(direction) else {
            return 0;
        };
        let count = self
            .host
            .backend()
            .midi2_controller_assignments(dir)
            .into_iter()
            .filter(is_valid_midi2_assignment)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    unsafe fn getMidi2ControllerAssignments(
        &self,
        direction: BusDirections,
        list: *const Midi2ControllerParamIDAssignmentList,
    ) -> tresult {
        // SAFETY: `list` is null (rejected) or the host's assignment list for this call.
        let Some(list) = (unsafe { list.as_ref() }) else {
            return kInvalidArgument;
        };
        if list.map.is_null() && list.count > 0 {
            return kInvalidArgument;
        }
        let Some(dir) = vst3_bus_direction(direction) else {
            return kResultFalse;
        };
        let assignments = self.host.backend().midi2_controller_assignments(dir);
        let out = list.map.cast::<AbiMidi2Assignment>();
        let requested = list.count as usize;
        let mut written = 0usize;
        for a in assignments
            .into_iter()
            .filter(is_valid_midi2_assignment)
            .take(requested)
        {
            let registered_bit =
                if a.controller.kind == moose_core::main_thread::Midi2ControllerKind::Registered {
                    0x80
                } else {
                    0
                };
            let record = AbiMidi2Assignment {
                p_id: a.param_id,
                bus_index: a.bus_index,
                channel: a.channel,
                byte1: (a.controller.bank & 0x7F) | registered_bit,
                byte2: a.controller.index & 0x7F,
                _pad: 0,
            };
            // SAFETY: Host allocated `list.map` for `list.count` C++ 12-byte
            // `Midi2ControllerParamIDAssignment` records; `.take(requested)`
            // guarantees `written < requested`. `write_unaligned` avoids UB if
            // the host supplies a packed or under-aligned buffer.
            unsafe { out.add(written).write_unaligned(record) };
            written += 1;
        }
        if written < requested {
            return kResultFalse;
        }
        kResultOk
    }

    unsafe fn getNumMidi1ControllerAssignments(&self, direction: BusDirections) -> uint32 {
        if direction != BusDirections_::kInput as BusDirections {
            return 0;
        }
        let mut n = 0;
        self.midi1_assignments(|_| n += 1);
        n
    }

    unsafe fn getMidi1ControllerAssignments(
        &self,
        direction: BusDirections,
        list: *const Midi1ControllerParamIDAssignmentList,
    ) -> tresult {
        // SAFETY: `list` is null (rejected) or the host's assignment list for this call.
        let Some(list) = (unsafe { list.as_ref() }) else {
            return kInvalidArgument;
        };
        if direction != BusDirections_::kInput as BusDirections
            || (list.map.is_null() && list.count > 0)
        {
            return kResultFalse;
        }
        // The host sized `map` from getNum...; never write past it.
        let mut n = 0;
        self.midi1_assignments(|a| {
            if n < list.count {
                // SAFETY: `map` is non-null whenever `count > 0` (checked above) and `n < count`.
                unsafe { list.map.add(n as usize).write(a) };
                n += 1;
            }
        });
        kResultOk
    }
}

impl IParameterFunctionNameTrait for Component {
    unsafe fn getParameterIDFromFunctionName(
        &self,
        unit: UnitID,
        name: FIDString,
        id: *mut ParamID,
    ) -> tresult {
        if name.is_null() || id.is_null() {
            return kInvalidArgument;
        }
        let h = &self.host;
        let backend = h.backend();
        // SAFETY: `name` is non-null (checked) and is the host's NUL-terminated function name.
        let want = unsafe { CStr::from_ptr(name) };
        let found = h
            .reg
            .params
            .iter()
            .zip(&h.reg.param_unit)
            .filter(|&(_, &u)| unit == kRootUnitId || u == unit)
            .map(|(p, _)| p.id)
            .find(|&pid| backend.param_function_name(pid) == Some(want));
        // SAFETY: `id` is non-null (checked) and is the host's out slot.
        unsafe { *id = found.unwrap_or(kNoParamId) };
        if found.is_some() {
            kResultTrue
        } else {
            kResultFalse
        }
    }
}

impl IRemapParamIDTrait for Component {
    unsafe fn getCompatibleParamID(
        &self,
        plugin_to_replace_uid: *const TUID,
        old_param_id: ParamID,
        new_param_id: *mut ParamID,
    ) -> tresult {
        if plugin_to_replace_uid.is_null() || new_param_id.is_null() {
            return kInvalidArgument;
        }
        let mut cid = [0u8; 16];
        // SAFETY: `plugin_to_replace_uid` is non-null (checked) and is a 16-byte `TUID`; `cid` is a
        // local of the same size.
        unsafe {
            std::ptr::copy_nonoverlapping(plugin_to_replace_uid.cast::<u8>(), cid.as_mut_ptr(), 16);
        }
        if let Some(out_id) = self.host.backend().remap_param_id(cid, old_param_id) {
            // SAFETY: `new_param_id` is non-null (checked) and is the host's out slot.
            unsafe { *new_param_id = out_id };
            kResultOk
        } else {
            // SAFETY: `new_param_id` is non-null (checked) and is the host's out slot.
            unsafe { *new_param_id = kNoParamId };
            kResultFalse
        }
    }
}

impl IInfoListenerTrait for Component {
    unsafe fn setChannelContextInfos(&self, list: *mut IAttributeList) -> tresult {
        // SAFETY: `list` is null (rejected) or the host's live attribute list for this call.
        let Some(list) = (unsafe { ComRef::from_raw(list) }) else {
            return kInvalidArgument;
        };
        let mut name = [0 as TChar; 256];
        // SAFETY: `list` is live; `name` is a local buffer whose byte size is passed.
        let name = (unsafe {
            list.getString(
                kChannelNameKey,
                name.as_mut_ptr(),
                size_of_val(&name) as u32,
            )
        } == kResultOk)
            // SAFETY: `name` is a local array of `name.len()` units; the read stops at its NUL or
            // end.
            .then(|| unsafe { utf16_from(name.as_ptr(), name.len()) });
        let mut color = 0i64;
        // SAFETY: `list` is live and `color` is a local.
        let color_argb = (unsafe { list.getInt(kChannelColorKey, &raw mut color) } == kResultOk)
            .then_some(color as u32);
        let info = Vst3TrackInfo { name, color_argb };
        self.host.backend().track_info_changed(&info);
        kResultTrue
    }
}

impl Component {
    fn midi_learn(&self, bus: int32, channel: i16, source: Vst3LearnSource) -> tresult {
        let (Ok(bus), Ok(channel @ 0..=15)) = (u32::try_from(bus), u8::try_from(channel)) else {
            return kInvalidArgument;
        };
        if self.host.backend().midi_learn(bus, channel, source) {
            kResultTrue
        } else {
            kResultFalse
        }
    }
}

impl IMidiLearnTrait for Component {
    unsafe fn onLiveMIDIControllerInput(
        &self,
        bus: int32,
        channel: int16,
        controller: CtrlNumber,
    ) -> tresult {
        self.midi_learn(bus, channel, Vst3LearnSource::Midi1(controller))
    }
}

/// Decode a VST3 `Midi2Controller`. The SDK struct is two bytes: `byte1`
/// holds the bank in bits 0-6 and "registered" (RPN) in bit 7, `byte2` the
/// index in bits 0-6 (`ivstmidimapping2.h`). The `vst3` crate binds it as
/// four bytes named `bank, registered, index, reserved`, so the host's
/// `byte1` lands in `bank` and its `byte2` in `registered`; the other two
/// fields are padding. `None` when the index's reserved bit 7 is set.
fn midi2_source(controller: Midi2Controller) -> Option<Vst3LearnSource> {
    let (byte1, byte2) = (controller.bank, controller.registered);
    (byte2 <= 0x7F).then_some(Vst3LearnSource::Midi2 {
        registered: byte1 & 0x80 != 0,
        bank: byte1 & 0x7F,
        index: byte2,
    })
}

impl IMidiLearn2Trait for Component {
    unsafe fn onLiveMidi2ControllerInput(
        &self,
        bus: BusIndex,
        channel: MidiChannel,
        controller: Midi2Controller,
    ) -> tresult {
        let Some(source) = midi2_source(controller) else {
            return kInvalidArgument;
        };
        self.midi_learn(bus, channel.into(), source)
    }

    unsafe fn onLiveMidi1ControllerInput(
        &self,
        bus: BusIndex,
        channel: MidiChannel,
        controller: CtrlNumber,
    ) -> tresult {
        self.midi_learn(bus, channel.into(), Vst3LearnSource::Midi1(controller))
    }
}

impl IProcessContextRequirementsTrait for Component {
    unsafe fn getProcessContextRequirements(&self) -> uint32 {
        // Every field a plugin may ask for (kNeedSystemTime ..
        // kNeedTransportState).
        (1..=10).fold(0, |m, bit| m | (1 << bit))
    }
}

/// Single program list ID exposed when `MainThreadHooks::program_list` is non-empty.
const PROGRAM_LIST_ID: ProgramListID = 0;

impl IUnitInfoTrait for Component {
    unsafe fn getUnitCount(&self) -> int32 {
        self.host.reg.units.len() as i32
    }

    unsafe fn getUnitInfo(&self, index: int32, info: *mut UnitInfo) -> tresult {
        let unit = usize::try_from(index)
            .ok()
            .and_then(|i| self.host.reg.units.get(i));
        // SAFETY: `info` is null (rejected) or the host's out slot.
        let (Some(info), Some(unit)) = (unsafe { info.as_mut() }, unit) else {
            return kInvalidArgument;
        };
        let has_programs = unit.id == 0 && !self.host.backend().program_list().is_empty();
        info.id = unit.id;
        info.parentUnitId = unit.parent;
        utf16_into(&mut info.name, &unit.name);
        info.programListId = if has_programs {
            PROGRAM_LIST_ID
        } else {
            kNoProgramListId
        };
        kResultOk
    }

    unsafe fn getProgramListCount(&self) -> int32 {
        i32::from(!self.host.backend().program_list().is_empty())
    }

    unsafe fn getProgramListInfo(&self, index: int32, info: *mut ProgramListInfo) -> tresult {
        // SAFETY: `info` is null (rejected) or the host's out slot.
        let Some(info) = (unsafe { info.as_mut() }) else {
            return kInvalidArgument;
        };
        if index != 0 {
            return kInvalidArgument;
        }
        let programs = self.host.backend().program_list();
        if programs.is_empty() {
            return kInvalidArgument;
        }
        info.id = PROGRAM_LIST_ID;
        utf16_into(&mut info.name, "Factory Presets");
        info.programCount = programs.len() as i32;
        kResultOk
    }

    unsafe fn getProgramName(
        &self,
        list: ProgramListID,
        index: int32,
        name: *mut String128,
    ) -> tresult {
        // SAFETY: `name` is null (rejected) or the host's `String128` out slot.
        let Some(name) = (unsafe { name.as_mut() }) else {
            return kInvalidArgument;
        };
        if list != PROGRAM_LIST_ID {
            return kInvalidArgument;
        }
        let programs = self.host.backend().program_list();
        let Some(preset) = usize::try_from(index).ok().and_then(|i| programs.get(i)) else {
            return kInvalidArgument;
        };
        utf16_into(name, &preset.name);
        kResultOk
    }

    unsafe fn getProgramInfo(
        &self,
        list: ProgramListID,
        index: int32,
        attribute: vst3::Steinberg::Vst::CString,
        value: *mut String128,
    ) -> tresult {
        // SAFETY: `value` is null (rejected) or the host's `String128` out slot.
        let Some(value) = (unsafe { value.as_mut() }) else {
            return kInvalidArgument;
        };
        if list != PROGRAM_LIST_ID || attribute.is_null() {
            return kInvalidArgument;
        }
        let programs = self.host.backend().program_list();
        let Some(preset) = usize::try_from(index).ok().and_then(|i| programs.get(i)) else {
            return kInvalidArgument;
        };
        // SAFETY: `attribute` is null or the host's NUL-terminated attribute name for this call.
        let attr = unsafe { cstr(attribute) };
        if matches!(attr, "Style" | "Category" | "Instrument") && !preset.category.is_empty() {
            utf16_into(value, &preset.category);
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn hasProgramPitchNames(&self, _list: ProgramListID, _index: int32) -> tresult {
        kNotImplemented
    }

    unsafe fn getProgramPitchName(
        &self,
        _list: ProgramListID,
        _index: int32,
        _pitch: int16,
        _name: *mut String128,
    ) -> tresult {
        kNotImplemented
    }

    unsafe fn getSelectedUnit(&self) -> UnitID {
        0
    }

    unsafe fn selectUnit(&self, _unit: UnitID) -> tresult {
        kResultOk
    }

    unsafe fn getUnitByBus(
        &self,
        _kind: MediaType,
        _dir: BusDirection,
        _bus: int32,
        _channel: int32,
        _unit: *mut UnitID,
    ) -> tresult {
        kNotImplemented
    }

    unsafe fn setUnitProgramData(&self, list: int32, index: int32, data: *mut IBStream) -> tresult {
        // SAFETY: Forwarded unchanged: `data` is null or the host's live stream, `setProgramData`'s
        // contract.
        unsafe { self.setProgramData(list, index, data) }
    }
}

impl IProgramListDataTrait for Component {
    unsafe fn programDataSupported(&self, list_id: ProgramListID) -> tresult {
        if list_id == PROGRAM_LIST_ID && !self.host.backend().program_list().is_empty() {
            kResultTrue
        } else {
            kResultFalse
        }
    }

    unsafe fn getProgramData(
        &self,
        list_id: ProgramListID,
        program_index: int32,
        data: *mut IBStream,
    ) -> tresult {
        // SAFETY: `data` is null (rejected) or the host's live stream for this call.
        let Some(stream) = (unsafe { ComRef::from_raw(data) }) else {
            return kInvalidArgument;
        };
        if list_id != PROGRAM_LIST_ID {
            return kInvalidArgument;
        }
        let programs = self.host.backend().program_list();
        let Some(preset) = usize::try_from(program_index)
            .ok()
            .and_then(|i| programs.get(i))
        else {
            return kInvalidArgument;
        };
        let bytes = &preset.state;
        let mut written = 0usize;
        while written < bytes.len() {
            let chunk = (bytes.len() - written).min(i32::MAX as usize);
            let mut wrote: int32 = 0;
            // SAFETY: `stream` is live; the source range is `chunk` readable bytes of `bytes` (the
            // SDK takes `*mut`, but `write` only reads) and `wrote` is a local.
            let rc = unsafe {
                stream.write(
                    bytes[written..].as_ptr().cast_mut().cast(),
                    chunk as int32,
                    &raw mut wrote,
                )
            };
            if rc != kResultOk || wrote <= 0 {
                return kResultFalse;
            }
            written += wrote as usize;
        }
        kResultOk
    }

    unsafe fn setProgramData(
        &self,
        list_id: ProgramListID,
        program_index: int32,
        data: *mut IBStream,
    ) -> tresult {
        // SAFETY: `data` is null (rejected) or the host's live stream for this call.
        let Some(stream) = (unsafe { ComRef::from_raw(data) }) else {
            return kInvalidArgument;
        };
        if list_id != PROGRAM_LIST_ID {
            return kInvalidArgument;
        }
        let backend = self.host.backend();
        let programs = backend.program_list();
        let Some(index) = usize::try_from(program_index)
            .ok()
            .filter(|&i| i < programs.len())
        else {
            return kInvalidArgument;
        };
        let Some(bytes) = read_program_stream(stream) else {
            return kResultFalse;
        };
        if backend.set_program_data(index, &bytes) {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

/// Bounded read cap for `IProgramListData::setProgramData` streams (32 MiB).
const MAX_PROGRAM_DATA_BYTES: usize = 32 * 1024 * 1024;

fn read_program_stream(stream: ComRef<'_, IBStream>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = super::read_state_chunk(stream, &mut chunk);
        if n < 0 {
            return None;
        }
        if n == 0 {
            return Some(out);
        }
        let n = n as usize;
        if out.len().saturating_add(n) > MAX_PROGRAM_DATA_BYTES {
            return None;
        }
        out.extend_from_slice(&chunk[..n]);
    }
}

fn allocate_program_change_id(reg: &Registration) -> u32 {
    let mut candidate = 0x7FFF_FFFEu32;
    while reg.params.iter().any(|p| p.id == candidate) {
        candidate = candidate.saturating_sub(1);
    }
    candidate
}

fn program_index_from_normalized(normalized: f64, count: usize) -> Option<usize> {
    if count == 0 || !normalized.is_finite() {
        return None;
    }
    if count == 1 {
        return Some(0);
    }
    let idx = (normalized.clamp(0.0, 1.0) * (count - 1) as f64).round() as usize;
    Some(idx.min(count - 1))
}

fn program_normalized_from_index(index: usize, count: usize) -> f64 {
    if count <= 1 {
        0.0
    } else {
        index.min(count - 1) as f64 / (count - 1) as f64
    }
}

/// The predefined note-expression types moose bridges to per-note MIDI 2.0
/// events, in the order hosts enumerate them: (id, title, short, default,
/// bipolar). Volume's no-change default is unity gain, 0.25 in the SDK's
/// `20 * log10(4 * x)` domain.
const NOTE_EXPRESSIONS: [(u32, &str, &str, f64, bool); 6] = [
    (0, "Volume", "Vol", 0.25, false),
    (1, "Pan", "Pan", 0.5, true),
    (2, "Tuning", "Tun", 0.5, true),
    (3, "Vibrato", "Vibr", 0.0, false),
    (4, "Expression", "Expr", 1.0, false),
    (5, "Brightness", "Bri", 0.5, false),
];

impl INoteExpressionControllerTrait for Component {
    unsafe fn getNoteExpressionCount(&self, bus: int32, _channel: int16) -> int32 {
        if bus < 0 || bus >= self.host.reg.desc.midi_input_ports {
            0
        } else {
            NOTE_EXPRESSIONS.len() as i32
        }
    }

    unsafe fn getNoteExpressionInfo(
        &self,
        bus: int32,
        channel: int16,
        index: int32,
        info: *mut NoteExpressionTypeInfo,
    ) -> tresult {
        let entry = usize::try_from(index)
            .ok()
            .and_then(|i| NOTE_EXPRESSIONS.get(i));
        let (Some(info), Some(&(id, title, short, default, bipolar))) =
            // SAFETY: `info` is null (rejected) or the host's out slot.
            (unsafe { info.as_mut() }, entry)
        else {
            return kInvalidArgument;
        };
        // SAFETY: `getNoteExpressionCount` only reads its integer arguments.
        if unsafe { self.getNoteExpressionCount(bus, channel) } == 0 {
            return kInvalidArgument;
        }
        // SAFETY: `NoteExpressionTypeInfo` is a plain C struct for which all-zero bytes are valid.
        *info = unsafe { std::mem::zeroed() };
        info.typeId = id;
        utf16_into(&mut info.title, title);
        utf16_into(&mut info.shortTitle, short);
        info.unitId = -1;
        info.valueDesc = NoteExpressionValueDescription {
            defaultValue: default,
            minimum: 0.0,
            maximum: 1.0,
            stepCount: 0,
        };
        info.flags = if bipolar {
            NoteExpressionTypeInfo_::NoteExpressionTypeFlags_::kIsBipolar as i32
        } else {
            0
        };
        kResultOk
    }

    unsafe fn getNoteExpressionStringByValue(
        &self,
        _bus: int32,
        _channel: int16,
        id: NoteExpressionTypeID,
        value: NoteExpressionValue,
        string: *mut String128,
    ) -> tresult {
        // SAFETY: `string` is null (rejected) or the host's `String128` out slot.
        let Some(string) = (unsafe { string.as_mut() }) else {
            return kInvalidArgument;
        };
        utf16_into(string, &note_expression_text(id, value));
        kResultOk
    }

    unsafe fn getNoteExpressionValueByString(
        &self,
        _bus: int32,
        _channel: int16,
        id: NoteExpressionTypeID,
        string: *const TChar,
        value: *mut NoteExpressionValue,
    ) -> tresult {
        if string.is_null() || value.is_null() {
            return kResultFalse;
        }
        // SAFETY: `string` is non-null (checked) and NUL-terminated within the host's `String128`.
        match note_expression_value(id, &unsafe { utf16_from(string, 64) }) {
            Some(v) => {
                // SAFETY: `value` is non-null (checked) and is the host's out slot.
                unsafe { *value = v };
                kResultOk
            }
            None => kResultFalse,
        }
    }
}

impl INoteExpressionPhysicalUIMappingTrait for Component {
    unsafe fn getPhysicalUIMapping(
        &self,
        bus: int32,
        _channel: int16,
        list: *mut PhysicalUIMapList,
    ) -> tresult {
        if bus < 0 || bus >= self.host.reg.desc.midi_input_ports {
            return kInvalidArgument;
        }
        // SAFETY: `list` is null (rejected) or the host's mapping list for this call.
        let Some(list) = (unsafe { list.as_mut() }) else {
            return kInvalidArgument;
        };
        if list.count > 0 && list.map.is_null() {
            return kInvalidArgument;
        }
        let entries = if list.count == 0 {
            &mut [][..]
        } else if list.map.is_null() {
            return kInvalidArgument;
        } else {
            // SAFETY: `map` is non-null and the caller supplies `list.count`
            // writable `PhysicalUIMap` items for this call.
            unsafe { std::slice::from_raw_parts_mut(list.map, list.count as usize) }
        };
        for entry in entries {
            entry.noteExpressionTypeID = match entry.physicalUITypeID {
                x if x == PhysicalUITypeIDs_::kPUIXMovement as u32 => {
                    NoteExpressionTypeIDs_::kTuningTypeID as u32
                }
                x if x == PhysicalUITypeIDs_::kPUIYMovement as u32 => {
                    NoteExpressionTypeIDs_::kBrightnessTypeID as u32
                }
                x if x == PhysicalUITypeIDs_::kPUIPressure as u32 => {
                    NoteExpressionTypeIDs_::kExpressionTypeID as u32
                }
                _ => NoteExpressionTypeIDs_::kInvalidTypeID as u32,
            };
        }
        kResultOk
    }
}

impl IKeyswitchControllerTrait for Component {
    unsafe fn getKeyswitchCount(&self, bus: int32, channel: int16) -> int32 {
        if bus < 0 || bus >= self.host.reg.desc.midi_input_ports || !(0..16).contains(&channel) {
            return 0;
        }
        self.host
            .backend()
            .keyswitches(bus as u32, channel as u8)
            .len() as i32
    }

    unsafe fn getKeyswitchInfo(
        &self,
        bus: int32,
        channel: int16,
        key_switch_index: int32,
        info: *mut KeyswitchInfo,
    ) -> tresult {
        if bus < 0 || bus >= self.host.reg.desc.midi_input_ports || !(0..16).contains(&channel) {
            return kInvalidArgument;
        }
        // SAFETY: `info` is null (rejected) or the host's out slot.
        let Some(info) = (unsafe { info.as_mut() }) else {
            return kInvalidArgument;
        };
        let list = self.host.backend().keyswitches(bus as u32, channel as u8);
        let Some(ks) = usize::try_from(key_switch_index)
            .ok()
            .and_then(|i| list.get(i))
        else {
            return kInvalidArgument;
        };
        // SAFETY: `KeyswitchInfo` is a plain C struct for which all-zero bytes are valid.
        *info = unsafe { std::mem::zeroed() };
        info.typeId = match ks.kind {
            moose_core::main_thread::KeyswitchKind::NoteOn => {
                KeyswitchTypeIDs_::kNoteOnKeyswitchTypeID as u32
            }
            moose_core::main_thread::KeyswitchKind::OnTheFly => {
                KeyswitchTypeIDs_::kOnTheFlyKeyswitchTypeID as u32
            }
            moose_core::main_thread::KeyswitchKind::OnRelease => {
                KeyswitchTypeIDs_::kOnReleaseKeyswitchTypeID as u32
            }
            moose_core::main_thread::KeyswitchKind::KeyRange => {
                KeyswitchTypeIDs_::kKeyRangeTypeID as u32
            }
        };
        utf16_into(&mut info.title, &ks.title);
        utf16_into(&mut info.shortTitle, &ks.short_title);
        info.keyswitchMin = i32::from(ks.min_key);
        info.keyswitchMax = i32::from(ks.max_key);
        info.keyRemapped = ks.remapped_key;
        info.unitId = ks.unit_id;
        info.flags = 0;
        kResultOk
    }
}

/// SDK domains: tuning spans -120..+120 semitones, volume is
/// `20 * log10(4 * x)` dB, the rest read as percent.
fn note_expression_text(id: u32, value: f64) -> String {
    match id {
        2 => format!("{:.2} st", (value - 0.5) * 240.0),
        0 if value <= 0.0 => "-inf dB".into(),
        0 => format!("{:.1} dB", 20.0 * (4.0 * value).log10()),
        _ => format!("{:.1} %", value * 100.0),
    }
}

/// Parses the leading number, like `strtod`.
fn note_expression_value(id: u32, text: &str) -> Option<f64> {
    let text = text.trim_start();
    let parsed: f64 = (1..=text.len())
        .rev()
        .filter(|&e| text.is_char_boundary(e))
        .find_map(|e| text[..e].parse().ok())?;
    let v = match id {
        2 => parsed / 240.0 + 0.5,
        0 => 10f64.powf(parsed / 20.0) / 4.0,
        _ => parsed / 100.0,
    };
    Some(v.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_arrangements_are_canonical() {
        assert_eq!(speaker_arrangement(1), 1 << 19);
        assert_eq!(speaker_arrangement(2), 0x3);
        assert_eq!(speaker_arrangement(4), 0x33);
        assert_eq!(speaker_arrangement(6), 0x3F);
        assert_eq!(speaker_arrangement(8), 0x63F);
        assert_eq!(speaker_arrangement(3).count_ones(), 3);
        assert_eq!(speaker_arrangement(64), u64::MAX);
    }

    #[test]
    fn note_expression_text_round_trips() {
        assert_eq!(note_expression_text(2, 0.5), "0.00 st");
        assert_eq!(note_expression_value(2, "12 st"), Some(0.55));
        assert_eq!(note_expression_value(0, "0.0 dB"), Some(0.25));
        assert_eq!(note_expression_value(4, "50 %"), Some(0.5));
        assert_eq!(note_expression_value(4, "loud"), None);
    }

    #[test]
    fn main_buses_start_active() {
        let kinds = [0u8, 1, 0];
        assert_eq!(default_active_mask(&kinds), 0b101);
        assert_eq!(default_active_mask(&[0, 0]), 0b11);
    }
}
