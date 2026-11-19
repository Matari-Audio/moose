//! The single-component VST3 object: `IComponent` + `IAudioProcessor` +
//! `IEditController` and friends on one COM identity.
//!
//! [`Host`] is the per-instance state, shared with plug views, editor
//! calls (through the registry) and run-loop handlers. [`Component`] is
//! the COM shell the host holds.

use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use moose_core::editor::EditGroup;
use moose_params::CatalogChange;
use vst3::Steinberg::Vst::ChannelContext::{
    IInfoListener, IInfoListenerTrait, kChannelColorKey, kChannelNameKey,
};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef};

use super::view::View;
use super::{Registration, cstr, retain, utf16_from, utf16_into};
use crate::ffi::{
    PRESENTATION_HIDDEN, PRESENTATION_UNAVAILABLE, Vst3LearnSource, Vst3NativeEvent, Vst3TrackInfo,
    Vst3Transport,
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

pub(crate) struct Host {
    pub ctx: *mut c_void,
    pub reg: &'static Registration,
    config: UnsafeCell<Config>,
    scratch: UnsafeCell<Scratch>,
    in_active: AtomicU32,
    out_active: AtomicU32,
    handler: Mutex<Option<ComPtr<IComponentHandler>>>,
    pending_restart: AtomicI32,
    presentation_revision: AtomicU64,
    /// A change that may move `presentation_revision` happened; the UI
    /// thread compares it once.
    presentation_check: AtomicBool,
    /// The catalog signal fired; the UI thread takes the changes.
    catalog_pending: AtomicBool,
    param_list: Mutex<ParamList>,
    /// Suppresses the host's `setParamNormalized` echo of our own edit.
    in_perform_edit: AtomicBool,
    persist_wanted: bool,
    persist_bound: AtomicBool,
    persist_pending: AtomicBool,
    edits: Mutex<Edits>,
    view: Mutex<ViewSlot>,
    #[cfg(target_os = "linux")]
    persist_wake: Mutex<Option<super::wake::linux::FdWake>>,
    #[cfg(target_os = "linux")]
    restart_fd: AtomicI32,
    #[cfg(target_os = "linux")]
    timer: Mutex<Option<super::wake::linux::TimerWake>>,
    /// The host context (factory `setHostContext`, else `initialize`), for
    /// `IWaylandHost`.
    #[cfg(target_os = "linux")]
    context: std::sync::OnceLock<ComPtr<FUnknown>>,
    #[cfg(target_os = "macos")]
    main_queue: std::sync::OnceLock<super::wake::macos::MainQueueWake>,
    #[cfg(target_os = "windows")]
    thread_timer: std::sync::OnceLock<super::wake::windows::ThreadTimer>,
}

// SAFETY: `ctx` is the Rust plugin instance, built for cross-thread use by
// the callback table. `config` follows the VST3 threading contract (written
// only while inactive). `scratch` is touched only by `process` and
// `setupProcessing`, which the host never overlaps. The view slot's raw
// pointer is only dereferenced on the UI thread.
unsafe impl Send for Host {}
unsafe impl Sync for Host {}

thread_local! {
    /// The instance whose `process` is on this thread's stack, so the audio
    /// thread can flag a restart with no lookup or lock.
    static PROCESSING: Cell<*const Host> = const { Cell::new(std::ptr::null()) };
}

/// Audio-thread restart request: sets bits for the UI thread to send.
/// Flag a `restartComponent` for the UI thread. Lock-free on the audio
/// thread (inside `process`); other threads find the instance through the
/// registry.
pub(super) fn mark_restart(ctx: *mut c_void, flags: i32) {
    let flag = |host: &Host| {
        host.pending_restart.fetch_or(flags, Ordering::Release);
        // A state load can rename parameters too.
        host.presentation_check.store(true, Ordering::Release);
        host.wake_ui();
    };
    // SAFETY: set only for the duration of `Host::process`.
    let on_audio = PROCESSING.with(|current| {
        unsafe { current.get().as_ref() }
            .filter(|h| h.ctx == ctx)
            .map(flag)
    });
    if on_audio.is_none()
        && let Some(host) = super::pin(ctx)
    {
        flag(&host);
        // A UI-thread caller (setState, the editor) delivers right away.
        if host.may_deliver(&lock(&host.edits)) {
            host.flush_pending_restart();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn default_active_mask(count: u32, kinds: *const u8) -> u32 {
    (0..count.min(32))
        .filter(|&i| kinds.is_null() || unsafe { *kinds.add(i as usize) } == 0)
        .fold(0, |mask, i| mask | (1 << i))
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
        *w = unsafe { (reg.cb.layout_bus_channels)(cfg.layout, 0, b as u32) };
    }
    for (b, w) in cfg.out_bus_channels.iter_mut().enumerate() {
        *w = unsafe { (reg.cb.layout_bus_channels)(cfg.layout, 1, b as u32) };
    }
}

impl Host {
    /// A new instance, or `None` when the plugin's constructor failed.
    pub(super) fn new(
        reg: &'static Registration,
        #[cfg_attr(
            not(target_os = "linux"),
            allow(unused_variables, clippy::needless_pass_by_value)
        )]
        factory_run_loop: Option<ComPtr<vst3::Steinberg::Linux::IRunLoop>>,
    ) -> Option<Arc<Self>> {
        let desc = reg.desc;
        let cb = reg.cb;
        let ctx = unsafe { (cb.create)() };
        if ctx.is_null() {
            return None;
        }
        let mut config = Config {
            sample_rate: 44100.0,
            max_frames: 1024,
            sample_size: SymbolicSampleSizes_::kSample32 as i32,
            process_mode: 0,
            layout: 0,
            total_in: desc.num_inputs,
            total_out: desc.num_outputs,
            in_bus_channels: vec![0; desc.num_input_buses as usize],
            out_bus_channels: vec![0; desc.num_output_buses as usize],
        };
        refresh_bus_channels(reg, &mut config);
        let host = Arc::new(Self {
            ctx,
            reg,
            config: UnsafeCell::new(config),
            scratch: UnsafeCell::new(Scratch {
                in_ptrs: vec![std::ptr::null(); desc.max_inputs as usize],
                out_ptrs: vec![std::ptr::null_mut(); desc.max_outputs as usize],
                silence: Vec::new(),
                trash: Vec::new(),
            }),
            in_active: AtomicU32::new(default_active_mask(
                desc.num_input_buses,
                desc.input_bus_kinds,
            )),
            out_active: AtomicU32::new(default_active_mask(
                desc.num_output_buses,
                desc.output_bus_kinds,
            )),
            handler: Mutex::new(None),
            pending_restart: AtomicI32::new(0),
            presentation_revision: AtomicU64::new(unsafe { (cb.param_presentation_revision)(ctx) }),
            presentation_check: AtomicBool::new(false),
            catalog_pending: AtomicBool::new(false),
            param_list: Mutex::new(ParamList::All),
            in_perform_edit: AtomicBool::new(false),
            persist_wanted: unsafe { (cb.wants_persist_notifications)(ctx) },
            persist_bound: AtomicBool::new(false),
            persist_pending: AtomicBool::new(false),
            edits: Mutex::new(Edits {
                #[cfg(target_os = "linux")]
                ui_thread: Some(std::thread::current().id()),
                ..Edits::default()
            }),
            view: Mutex::new(ViewSlot::default()),
            #[cfg(target_os = "linux")]
            persist_wake: Mutex::new(None),
            #[cfg(target_os = "linux")]
            restart_fd: AtomicI32::new(-1),
            #[cfg(target_os = "linux")]
            timer: Mutex::new(None),
            #[cfg(target_os = "linux")]
            context: std::sync::OnceLock::new(),
            #[cfg(target_os = "macos")]
            main_queue: std::sync::OnceLock::new(),
            #[cfg(target_os = "windows")]
            thread_timer: std::sync::OnceLock::new(),
        });
        super::register_instance(&host);
        // Instances are created on the host UI thread, so these wakes bind
        // to it.
        #[cfg(target_os = "macos")]
        if let Some(q) = super::wake::macos::MainQueueWake::new(ctx, main_queue_event) {
            let _ = host.main_queue.set(q);
        }
        #[cfg(target_os = "windows")]
        if let Some(t) = super::wake::windows::ThreadTimer::new(thread_timer_proc) {
            let _ = host.thread_timer.set(t);
        }
        #[cfg(target_os = "linux")]
        if let Some(rl) = factory_run_loop {
            host.bind_persist_run_loop(rl);
        }
        host.bind_persist_signal();
        // The host hasn't read the list yet: changes from construction are
        // already in it.
        let _ = unsafe { (cb.take_catalog_changes)(ctx) };
        let me = Arc::as_ptr(&host).cast_mut().cast();
        unsafe { (cb.set_catalog_waker)(ctx, Some(Self::wake_catalog), me) };
        Some(host)
    }

    /// Schedule a UI-thread flush. Lock-free, so the audio thread may call it.
    // `self` holds the wake on Linux and macOS.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos")),
        allow(clippy::unused_self)
    )]
    fn wake_ui(&self) {
        #[cfg(target_os = "linux")]
        {
            let fd = self.restart_fd.load(Ordering::Acquire);
            if fd >= 0 {
                super::wake::linux::FdWake::notify_fd(fd);
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(q) = self.main_queue.get() {
            q.signal();
        }
        // Windows: the thread timer flushes on every tick.
    }

    /// The parameter set's `CatalogSignal` wake. Any non-audio thread.
    unsafe extern "C" fn wake_catalog(context: *mut c_void) {
        // SAFETY: `context` is this `Host`, bound in `new`; `drop` unbinds
        // it first, which waits for a running wake.
        let host = unsafe { &*context.cast::<Self>() };
        host.catalog_pending.store(true, Ordering::Release);
        host.wake_ui();
    }

    /// A processed or host-set change to `id`: if it can rename parameters,
    /// have the UI thread compare the presentation revision. Audio-safe.
    fn note_param_change(&self, id: u32) {
        if unsafe { (self.reg.cb.param_affects_presentation)(self.ctx, id) }
            && !self.presentation_check.swap(true, Ordering::AcqRel)
        {
            self.wake_ui();
        }
    }

    // --- Parameter list ---------------------------------------------------------

    /// Indices of the parameters a dynamic-list host should see. UI thread.
    fn available_params(&self) -> Vec<usize> {
        let mut title = [0 as c_char; 512];
        (0..self.reg.params.len())
            .filter(|&i| {
                let bits = unsafe {
                    (self.reg.cb.param_presentation)(
                        self.ctx,
                        self.reg.params[i].id,
                        title.as_mut_ptr(),
                        512,
                    )
                };
                bits < 0 || bits & PRESENTATION_UNAVAILABLE == 0
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
        unsafe { &*self.config.get() }
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn config_mut(&self) -> &mut Config {
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
            self.main_queue.get().is_none() || super::wake::macos::on_main_thread()
        }
        #[cfg(target_os = "windows")]
        {
            self.thread_timer
                .get()
                .is_none_or(|t| t.thread == std::thread::current().id())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            true
        }
    }

    // `self` holds the wake on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(clippy::unused_self))]
    fn wake_edits(&self, #[allow(unused_variables)] edits: &Edits) {
        #[cfg(target_os = "linux")]
        if let Some(w) = &edits.wake {
            w.notify();
        } else {
            let fd = self.restart_fd.load(Ordering::Acquire);
            if fd >= 0 {
                super::wake::linux::FdWake::notify_fd(fd);
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(q) = self.main_queue.get() {
            q.signal();
        }
        // Windows: the thread timer drains on every tick.
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
                    unsafe { h.beginEdit(id) };
                    lock(&self.edits).open_gestures.push(id);
                }
                1
            }
            EditOp::Perform(id, value) => {
                if let Some(h) = self.handler() {
                    self.in_perform_edit.store(true, Ordering::Relaxed);
                    unsafe { h.performEdit(id, value) };
                    self.in_perform_edit.store(false, Ordering::Relaxed);
                    self.note_param_change(id);
                    self.flush_pending_restart();
                }
                1
            }
            EditOp::End(id) => {
                if let Some(h) = self.handler() {
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
                if let Some(h) = self.handler().and_then(|h| h.cast::<IComponentHandler2>()) {
                    unsafe { h.setDirty(1) };
                }
                1
            }
            EditOp::Group(edge) => {
                if let Some(h) = self.handler().and_then(|h| h.cast::<IComponentHandler2>()) {
                    match edge {
                        EditGroup::Begin => unsafe { h.startGroupEdit() },
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
        i32::from(unsafe { frame.resizeView(view, &raw mut rect) } == kResultOk)
    }

    /// Timer / dispatch tick on the UI thread.
    pub(super) fn on_ui_tick(&self) {
        self.flush_pending_restart();
        self.drain_edits();
    }

    // --- Restart and persist notifications -----------------------------------

    /// Send any restart flags the audio thread, a catalog change or a
    /// presentation change left pending. UI thread only.
    pub(super) fn flush_pending_restart(&self) {
        let Some(handler) = self.handler() else {
            return;
        };
        self.flush_persist_change();
        let cb = self.reg.cb;
        let mut check = self.presentation_check.swap(false, Ordering::AcqRel);
        if self.catalog_pending.swap(false, Ordering::AcqRel) {
            let changes =
                CatalogChange::from_bits_truncate(unsafe { (cb.take_catalog_changes)(self.ctx) });
            let support = self.dynamic_support();
            if changes.contains(CatalogChange::STRUCTURE) {
                self.relist_params();
            }
            self.pending_restart
                .fetch_or(support.restart_flags(changes), Ordering::Release);
            check = true;
        }
        if check {
            let revision = unsafe { (cb.param_presentation_revision)(self.ctx) };
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
            unsafe { handler.restartComponent(flags) };
        }
    }

    unsafe extern "C" fn wake_persist(context: *mut c_void) {
        let host = unsafe { &*context.cast::<Self>() };
        // Already pending: a wake is on its way. Windows: the thread timer
        // drains on its next tick.
        let first = !host.persist_pending.swap(true, Ordering::AcqRel);
        #[cfg(target_os = "linux")]
        if first && let Some(w) = lock(&host.persist_wake).as_ref() {
            w.notify();
        }
        #[cfg(target_os = "macos")]
        if first && let Some(q) = host.main_queue.get() {
            q.signal();
        }
        #[cfg(target_os = "windows")]
        let _ = first;
    }

    fn bind_persist_signal(&self) {
        if !self.persist_wanted || self.persist_bound.load(Ordering::Acquire) {
            return;
        }
        #[cfg(target_os = "linux")]
        let ready = lock(&self.persist_wake).is_some();
        #[cfg(target_os = "macos")]
        let ready = self.main_queue.get().is_some();
        #[cfg(target_os = "windows")]
        let ready = self.thread_timer.get().is_some();
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        let ready = false;
        if !ready || self.persist_bound.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = std::ptr::from_ref(self).cast_mut().cast();
        unsafe { (self.reg.cb.set_persist_waker)(self.ctx, Some(Self::wake_persist), me) };
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
            && !self.persist_pending.swap(false, Ordering::AcqRel)
        {
            return;
        }
        let (mut id, mut value) = (0u32, 0f64);
        if !unsafe { (self.reg.cb.take_persist_change)(self.ctx, &raw mut id, &raw mut value) } {
            return;
        }
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
            self.restart_fd.store(wake.fd(), Ordering::Release);
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
    #[cfg(target_os = "linux")]
    pub(super) fn update_run_loop_registration(self: &Arc<Self>, frame: Option<*mut FUnknown>) {
        use super::wake::linux::{FdWake, TimerWake, run_loop_of};
        let run_loop = frame.and_then(run_loop_of);
        if self.persist_wanted
            && let Some(rl) = &run_loop
        {
            self.bind_persist_run_loop(rl.clone());
        }
        // Deliver everything queued before unbinding, so no worker steals
        // the detach batch.
        let old = {
            let mut e = self.drain_locked(lock(&self.edits), true);
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
        if let Some(wake) = FdWake::bind(rl.clone(), Arc::downgrade(self), Host::drain_edits) {
            let mut e = lock(&self.edits);
            e.wake = Some(wake);
        }
        *lock(&self.timer) = TimerWake::bind(rl, Arc::downgrade(self));
    }

    /// Keep the first host context offered; later ones are ignored.
    #[cfg(target_os = "linux")]
    pub(super) fn set_host_context(&self, context: *mut FUnknown) {
        // SAFETY: `context` is null or a live host object for this call.
        if self.context.get().is_none()
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

    /// Walk the host's parameter queues. Every point goes to the plugin's
    /// event list when the block renders; a queue's final value is also
    /// committed here when no sub-block apply will (a flush, a non-chunked
    /// parameter, or an event list that ran out of room). Returns true on
    /// an overflow: the caller then commits final values again after the
    /// block, so earlier queued points can't win.
    unsafe fn read_param_changes(
        &self,
        changes: *mut IParameterChanges,
        will_process: bool,
    ) -> bool {
        let Some(changes) = (unsafe { ComRef::from_raw(changes) }) else {
            return false;
        };
        let mut dropped = false;
        let cb = self.reg.cb;
        for i in 0..unsafe { changes.getParameterCount() } {
            let Some(queue) = (unsafe { ComRef::from_raw(changes.getParameterData(i)) }) else {
                continue;
            };
            let id = unsafe { queue.getParameterId() };
            let mut pushed_all = will_process;
            let mut last = None;
            for j in 0..unsafe { queue.getPointCount() } {
                let (mut offset, mut value) = (0, 0.0);
                if unsafe { queue.getPoint(j, &raw mut offset, &raw mut value) } != kResultOk {
                    continue;
                }
                let plain = unsafe { (cb.param_denormalize)(self.ctx, id, value) };
                last = Some(plain);
                if pushed_all && unsafe { (cb.push_param_change)(self.ctx, id, offset, plain) } == 0
                {
                    pushed_all = false;
                    dropped = true;
                }
            }
            if let Some(plain) = last
                && !(pushed_all && unsafe { (cb.param_is_chunked)(self.ctx, id) } != 0)
            {
                unsafe { (cb.param_set_value)(self.ctx, id, plain) };
            }
            if last.is_some() {
                self.note_param_change(id);
            }
        }
        dropped
    }

    /// The last point of every queue, committed after an overflowed block.
    unsafe fn commit_final_values(&self, changes: *mut IParameterChanges) {
        let Some(changes) = (unsafe { ComRef::from_raw(changes) }) else {
            return;
        };
        let cb = self.reg.cb;
        for i in 0..unsafe { changes.getParameterCount() } {
            let Some(queue) = (unsafe { ComRef::from_raw(changes.getParameterData(i)) }) else {
                continue;
            };
            let count = unsafe { queue.getPointCount() };
            let (mut offset, mut value) = (0, 0.0);
            if count > 0
                && unsafe { queue.getPoint(count - 1, &raw mut offset, &raw mut value) }
                    == kResultOk
            {
                let id = unsafe { queue.getParameterId() };
                unsafe {
                    (cb.param_set_value)(self.ctx, id, (cb.param_denormalize)(self.ctx, id, value));
                };
            }
        }
    }

    fn transport(ctx: &ProcessContext) -> Vst3Transport {
        use ProcessContext_::StatesAndFlags_::*;
        let has = |flag: ProcessContext_::StatesAndFlags| ctx.state & flag as u32 != 0;
        let sig = has(kTimeSigValid);
        let cycle = has(kCycleValid);
        Vst3Transport {
            playing: i32::from(has(kPlaying)),
            recording: i32::from(has(kRecording)),
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
            // kCycleActive is "loop engaged"; kCycleValid only says the
            // markers are readable.
            cycle_active: i32::from(has(kCycleActive)),
        }
    }

    /// Pass the host's input events on in their original order.
    unsafe fn push_input_events(&self, list: *mut IEventList) {
        let Some(list) = (unsafe { ComRef::from_raw(list) }) else {
            return;
        };
        let cb = self.reg.cb;
        for i in 0..unsafe { list.getEventCount() } {
            let mut ev: Event = unsafe { std::mem::zeroed() };
            if unsafe { list.getEvent(i, &raw mut ev) } != kResultOk {
                break;
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
                        n.bytes = e.bytes;
                        n.len = e.size;
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
            // Queue full: keep one ordered prefix.
            if unsafe { (cb.push_input_event)(self.ctx, &raw const n) } == OUTPUT_QUEUE_FULL {
                break;
            }
        }
    }

    /// Hand the plugin's output events to the host list in global order.
    /// Note-id bookkeeping is committed only after the host accepted each.
    unsafe fn drain_output_events(&self, list: *mut IEventList) -> u32 {
        let cb = self.reg.cb;
        let list = unsafe { ComRef::from_raw(list) };
        let mut result = OUTPUT_END;
        unsafe { (cb.begin_output_events)(self.ctx) };
        loop {
            let mut n = Vst3NativeEvent::default();
            match unsafe { (cb.next_output_event)(self.ctx, &raw mut n) } {
                OUTPUT_END => return result,
                OUTPUT_EMITTED => {}
                code @ (OUTPUT_UNSUPPORTED | OUTPUT_INVALID | OUTPUT_QUEUE_FULL) => return code,
                _ => return OUTPUT_INVALID,
            }
            let Some(list) = list else {
                return OUTPUT_UNSUPPORTED;
            };
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
            if unsafe { list.addEvent(&raw mut ev) } != kResultOk {
                return OUTPUT_QUEUE_FULL;
            }
            unsafe { (cb.commit_output_event)(self.ctx) };
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
        let Some(changes) = (unsafe { ComRef::from_raw(changes) }) else {
            return result;
        };
        let cb = self.reg.cb;
        for i in 0..count {
            let (mut id, mut offset, mut value) = (0u32, 0i32, 0f64);
            unsafe {
                (cb.get_output_param)(self.ctx, i, &raw mut id, &raw mut offset, &raw mut value);
            };
            let mut index = 0;
            let queue = unsafe { changes.addParameterData(&raw const id, &raw mut index) };
            let Some(queue) = (unsafe { ComRef::from_raw(queue) }) else {
                return OUTPUT_QUEUE_FULL;
            };
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
        let cb = self.reg.cb;
        let desc = self.reg.desc;
        let cfg = self.config();
        // Never reinterpret buffers at a width other than the negotiated one.
        if data.symbolicSampleSize != cfg.sample_size {
            return kResultFalse;
        }
        // A parameter flush: no audio buses at all on a plugin that has
        // them (Live sends these with nonzero numSamples for suspended
        // devices). A pure-MIDI plugin legitimately processes this shape.
        let suspended_flush = (desc.num_inputs > 0 || desc.num_outputs > 0)
            && data.numInputs == 0
            && data.numOutputs == 0;
        let will_process = data.numSamples > 0 && !suspended_flush;
        let frames = data.numSamples.max(0) as u32;

        // Notes first: automation can only take what capacity they leave.
        if will_process {
            unsafe { (cb.begin_input_events)(self.ctx, frames) };
            unsafe { self.push_input_events(data.inputEvents) };
        }
        let dropped = unsafe { self.read_param_changes(data.inputParameterChanges, will_process) };
        if !will_process {
            return kResultOk;
        }
        let transport = unsafe { data.processContext.as_ref() }.map(Self::transport);

        let scratch = unsafe { &mut *self.scratch.get() };
        let use64 = cfg.sample_size == SymbolicSampleSizes_::kSample64 as i32;
        let in_active = self.in_active.load(Ordering::Acquire);
        let out_active = self.out_active.load(Ordering::Acquire);

        // Flatten every declared bus at its negotiated width, in bus order.
        // Inactive or missing channels keep their position as null and are
        // filled from scratch below.
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

        // Missing inputs read shared silence; missing outputs write their own
        // discard block. Skipped if the host overruns its declared block size.
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
                unsafe { std::ptr::copy(src.cast::<u8>(), dst.cast::<u8>(), bytes) };
            }
        }

        let transport_ptr = transport
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref);
        let process = if use64 { cb.process_f64 } else { cb.process };
        PROCESSING.with(|p| p.set(self));
        let ok = unsafe {
            process(
                self.ctx,
                scratch.in_ptrs.as_ptr(),
                scratch.out_ptrs.as_mut_ptr(),
                num_in as u32,
                num_out as u32,
                cfg.in_bus_channels.as_ptr(),
                desc.num_input_buses,
                in_active,
                cfg.out_bus_channels.as_ptr(),
                desc.num_output_buses,
                out_active,
                frames,
                transport_ptr,
                data.processMode,
            )
        };
        PROCESSING.with(|p| p.set(std::ptr::null()));
        if dropped {
            unsafe { self.commit_final_values(data.inputParameterChanges) };
        }

        if ok == 0 {
            unsafe { (cb.finish_output_events)(self.ctx, OUTPUT_INVALID) };
            return kResultOk;
        }

        // Rendered audio is not silent; clear the host's previous flags.
        if !data.outputs.is_null() {
            let buses =
                (desc.num_output_buses.min(32) as usize).min(data.numOutputs.max(0) as usize);
            for b in 0..buses {
                if out_active & (1 << b) != 0 {
                    unsafe { (*data.outputs.add(b)).silenceFlags = 0 };
                }
            }
        }

        let out_params = unsafe { (cb.get_output_param_count)(self.ctx) };
        let mut result = unsafe { self.drain_output_events(data.outputEvents) };
        if out_params > 0 && matches!(result, OUTPUT_END | OUTPUT_EMITTED) {
            result = if data.outputParameterChanges.is_null() {
                OUTPUT_UNSUPPORTED
            } else {
                unsafe { self.write_output_params(data.outputParameterChanges, out_params, result) }
            };
        }
        unsafe { (cb.finish_output_events)(self.ctx, result) };
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
            (!buses.is_null() && b < count.max(0) as usize).then(|| unsafe { &*buses.add(b) });
        let on = b < 32 && active & (1 << b) != 0;
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
        super::unregister_instance(self.ctx);
        let cb = self.reg.cb;
        // Joins a running catalog wake, which points at `self`.
        unsafe { (cb.set_catalog_waker)(self.ctx, None, std::ptr::null_mut()) };
        if self.persist_bound.load(Ordering::Acquire) {
            // Joins any worker wake before the wakes go away.
            unsafe { (cb.set_persist_waker)(self.ctx, None, std::ptr::null_mut()) };
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
            drop(e.wake.take());
        }
        #[cfg(target_os = "linux")]
        {
            lock(&self.persist_wake).take();
            lock(&self.timer).take();
        }
        #[cfg(target_os = "macos")]
        drop(self.main_queue.take());
        #[cfg(target_os = "windows")]
        drop(self.thread_timer.take());
        lock(&self.handler).take();
        unsafe { (cb.destroy)(self.ctx) };
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn main_queue_event(ctx: *mut c_void) {
    if let Some(host) = super::pin(ctx) {
        host.on_ui_tick();
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn thread_timer_proc(_: *mut c_void, _: u32, id: usize, _: u32) {
    if let Some(host) = super::find_instance(|h| h.thread_timer.get().is_some_and(|t| t.id == id)) {
        host.on_ui_tick();
    }
}

// ---------------------------------------------------------------------------
// COM class
// ---------------------------------------------------------------------------

pub(super) struct Component {
    pub host: Arc<Host>,
}

impl Class for Component {
    type Interfaces = (
        IComponent,
        IAudioProcessor,
        IEditController,
        IEditControllerHostEditing,
        IMidiMapping,
        IMidiMapping2,
        IProcessContextRequirements,
        IUnitInfo,
        INoteExpressionController,
        IParameterFunctionName,
        IInfoListener,
        IMidiLearn,
        IMidiLearn2,
    );
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
        if let Some(rl) = super::wake::linux::run_loop_of(context) {
            self.host.bind_persist_run_loop(rl);
        }
        #[cfg(target_os = "linux")]
        self.host.set_host_context(context);
        if let Some(app) =
            unsafe { ComRef::from_raw(context) }.and_then(|c| c.cast::<IHostApplication>())
        {
            let mut name: String128 = [0; 128];
            if unsafe { app.getName(&raw mut name) } == kResultOk {
                let name = unsafe { utf16_from(name.as_ptr(), name.len()) };
                self.host
                    .set_dynamic_support(DynamicSupport::for_host(&name));
            }
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
        let d = self.host.reg.desc;
        let Some(input) = is_dir(dir) else {
            return 0;
        };
        // An audio bus only where that side has channels: a phantom
        // zero-channel bus crashes strict hosts (Bitwig).
        match kind {
            k if k == MediaTypes_::kAudio as i32 => {
                (if input {
                    d.num_input_buses
                } else {
                    d.num_output_buses
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
        let (Some(bus), Some(input)) = (unsafe { bus.as_mut() }, is_dir(dir)) else {
            return kInvalidArgument;
        };
        if index < 0 {
            return kInvalidArgument;
        }
        let d = self.host.reg.desc;
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
            // moose carries all 16 MIDI channels end to end.
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
        let (count, kinds) = if input {
            (d.num_input_buses, d.input_bus_kinds)
        } else {
            (d.num_output_buses, d.output_bus_kinds)
        };
        if index as u32 >= count {
            return kInvalidArgument;
        }
        let aux = !kinds.is_null() && unsafe { *kinds.add(index as usize) } != 0;
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
        // Main buses are live on a fresh instance; aux buses wait for
        // activateBus.
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
        let d = self.host.reg.desc;
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
            d.num_input_buses
        } else {
            d.num_output_buses
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
        let cb = h.reg.cb;
        let cfg = h.config();
        unsafe {
            if state != 0 {
                if (cb.prepare)(h.ctx, cfg.sample_rate, cfg.max_frames, cfg.process_mode) == 0 {
                    return kResultFalse;
                }
            } else {
                (cb.reset)(h.ctx, cfg.sample_rate, cfg.max_frames, cfg.process_mode);
            }
            // A state load while suspended must not wait for the audio thread.
            (cb.set_active)(h.ctx, i32::from(state != 0));
        }
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let h = &self.host;
        if !state.is_null()
            && unsafe { (h.reg.cb.state_load_stream)(h.ctx, state.cast(), super::read_state_chunk) }
                != 0
        {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        let h = &self.host;
        if !state.is_null()
            && unsafe {
                (h.reg.cb.state_save_stream)(h.ctx, state.cast(), super::write_state_chunk)
            } != 0
        {
            kResultOk
        } else {
            kResultFalse
        }
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
        let d = h.reg.desc;
        // One arrangement per declared bus, matched per bus (a sidechain on
        // its own width) against the declared layouts. kResultFalse makes
        // the host ask getBusArrangement for what we do support.
        if num_ins != d.num_input_buses as i32 || num_outs != d.num_output_buses as i32 {
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
                        unsafe { *arr.add(i) }.count_ones()
                    }
                })
                .collect()
        };
        let ins = widths(inputs, num_ins);
        let outs = widths(outputs, num_outs);
        if ins.iter().enumerate().any(|(i, &width)| {
            width == 0 || unsafe { *inputs.add(i) } != speaker_arrangement(width)
        }) || outs.iter().enumerate().any(|(i, &width)| {
            width == 0 || unsafe { *outputs.add(i) } != speaker_arrangement(width)
        }) {
            return kResultFalse;
        }
        let layout = unsafe {
            (h.reg.cb.match_bus_layout_perbus)(
                ins.as_ptr(),
                ins.len() as u32,
                outs.as_ptr(),
                outs.len() as u32,
            )
        };
        if layout < 0 {
            return kResultFalse;
        }
        let cfg = unsafe { h.config_mut() };
        cfg.layout = layout as u32;
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
        let d = self.host.reg.desc;
        let count = if dir == BusDirections_::kInput as i32 {
            d.num_input_buses
        } else {
            d.num_output_buses
        };
        if arr.is_null() || index < 0 || index as u32 >= count {
            return kInvalidArgument;
        }
        let ch = self.host.bus_channels(dir, index as u32);
        if ch == 0 || ch > 64 {
            return kInvalidArgument;
        }
        unsafe { *arr = speaker_arrangement(ch) };
        kResultOk
    }

    unsafe fn canProcessSampleSize(&self, size: int32) -> tresult {
        let ok = size == SymbolicSampleSizes_::kSample32 as i32
            || (size == SymbolicSampleSizes_::kSample64 as i32
                && self.host.reg.desc.supports_f64 != 0);
        if ok { kResultOk } else { kResultFalse }
    }

    unsafe fn getLatencySamples(&self) -> uint32 {
        unsafe { (self.host.reg.cb.get_latency)(self.host.ctx) }
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        let Some(setup) = (unsafe { setup.as_ref() }) else {
            return kInvalidArgument;
        };
        // Refuse a sample size we never offered.
        if unsafe { self.canProcessSampleSize(setup.symbolicSampleSize) } != kResultOk {
            return kResultFalse;
        }
        let h = &self.host;
        let cfg = unsafe { h.config_mut() };
        cfg.sample_size = setup.symbolicSampleSize;
        cfg.sample_rate = setup.sampleRate;
        cfg.max_frames = setup.maxSamplesPerBlock.max(0) as u32;
        cfg.process_mode = setup.processMode;
        let frames = cfg.max_frames as usize;
        let d = h.reg.desc;
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
        unsafe {
            (h.reg.cb.set_processing)(
                h.ctx,
                i32::from(state != 0),
                cfg.sample_rate,
                cfg.max_frames,
                cfg.process_mode,
            );
        }
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        match unsafe { data.as_mut() } {
            Some(data) => unsafe { self.host.process(data) },
            None => kResultOk,
        }
    }

    unsafe fn getTailSamples(&self) -> uint32 {
        unsafe { (self.host.reg.cb.get_tail)(self.host.ctx) }
    }
}

impl IEditControllerTrait for Component {
    unsafe fn setComponentState(&self, _state: *mut IBStream) -> tresult {
        // Single component: setState already loaded everything.
        kResultOk
    }

    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> int32 {
        self.host.param_count() as i32
    }

    unsafe fn getParameterInfo(&self, index: int32, info: *mut ParameterInfo) -> tresult {
        let reg = self.host.reg;
        let (Some(info), Some(index)) = (unsafe { info.as_mut() }, self.host.param_index(index))
        else {
            return kInvalidArgument;
        };
        let p = &reg.params[index];
        info.id = p.id;
        utf16_into(&mut info.title, unsafe { cstr(p.name) });
        utf16_into(&mut info.shortTitle, unsafe { cstr(p.short_name) });
        utf16_into(&mut info.units, unsafe { cstr(p.units) });
        info.stepCount = p.step_count;
        info.defaultNormalizedValue = p.default_normalized;
        info.unitId = reg.param_unit[index];
        info.flags = p.flags;
        let mut title = [0 as c_char; 512];
        let bits =
            unsafe { (reg.cb.param_presentation)(self.host.ctx, p.id, title.as_mut_ptr(), 512) };
        if bits >= 0 {
            let title = unsafe { cstr(title.as_ptr()) };
            utf16_into(&mut info.title, title);
            utf16_into(&mut info.shortTitle, title);
            // Listed but unavailable only in slot-pool mode: a free slot.
            if bits & (PRESENTATION_HIDDEN | PRESENTATION_UNAVAILABLE) != 0 {
                // The SDK defines hidden as read-only and not automatable.
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
        let Some(string) = (unsafe { string.as_mut() }) else {
            return kInvalidArgument;
        };
        let h = &self.host;
        let plain = unsafe { (h.reg.cb.param_denormalize)(h.ctx, id, normalized) };
        let mut buf = [0 as c_char; 128];
        if unsafe { (h.reg.cb.param_format)(h.ctx, id, plain, buf.as_mut_ptr(), 128) } > 0 {
            utf16_into(string, unsafe { cstr(buf.as_ptr()) });
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
        let Ok(text) = std::ffi::CString::new(unsafe { utf16_from(string, 256) }) else {
            return kResultFalse;
        };
        let mut plain = 0.0;
        if unsafe { (h.reg.cb.param_parse)(h.ctx, id, text.as_ptr(), &raw mut plain) } == 0 {
            return kResultFalse;
        }
        unsafe { *normalized = (h.reg.cb.param_normalize)(h.ctx, id, plain) };
        kResultOk
    }

    unsafe fn normalizedParamToPlain(&self, id: ParamID, normalized: ParamValue) -> ParamValue {
        unsafe { (self.host.reg.cb.param_denormalize)(self.host.ctx, id, normalized) }
    }

    unsafe fn plainParamToNormalized(&self, id: ParamID, plain: ParamValue) -> ParamValue {
        unsafe { (self.host.reg.cb.param_normalize)(self.host.ctx, id, plain) }
    }

    unsafe fn getParamNormalized(&self, id: ParamID) -> ParamValue {
        // Hosts poll this on the UI thread: a good moment to send what the
        // audio thread flagged.
        let h = &self.host;
        if h.may_deliver(&lock(&h.edits)) {
            h.on_ui_tick();
        }
        unsafe {
            let plain = (h.reg.cb.param_get_value)(h.ctx, id);
            (h.reg.cb.param_normalize)(h.ctx, id, plain)
        }
    }

    unsafe fn setParamNormalized(&self, id: ParamID, value: ParamValue) -> tresult {
        let h = &self.host;
        if h.in_perform_edit.load(Ordering::Relaxed) {
            return kResultOk;
        }
        unsafe {
            let plain = (h.reg.cb.param_denormalize)(h.ctx, id, value);
            (h.reg.cb.param_set_value)(h.ctx, id, plain);
        }
        h.note_param_change(id);
        kResultOk
    }

    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        let h = &self.host;
        let handler = unsafe { retain(handler) };
        let has_handler = handler.is_some();
        // Hold the queue lock across the swap so concurrent editor calls land
        // on the right side of it.
        let e = h.drain_locked(lock(&h.edits), true);
        // A submitter that never stopped: its gestures end on the old handler
        // and continue in fresh ones on the new.
        // An open group edit finishes there too and restarts first.
        let (mut e, reopen) = h.close_open_gestures(e);
        let previous = std::mem::replace(&mut *lock(&h.handler), handler);
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
        let h = &self.host;
        if !id.is_null()
            && unsafe { (h.reg.cb.midi_mapping_get_param_id)(h.ctx, bus, channel, controller, id) }
                != 0
        {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

impl Component {
    /// Every MIDI 1 input assignment `getMidiControllerAssignment` would
    /// answer, in bus / channel / controller order. Controllers run to
    /// kCtrlProgramChange (130).
    fn midi1_assignments(&self, mut each: impl FnMut(Midi1ControllerParamIDAssignment)) {
        let h = &self.host;
        for bus in 0..h.reg.desc.midi_input_ports.max(0) {
            for channel in 0..16i16 {
                for controller in 0..=130i16 {
                    let mut id = 0;
                    if unsafe {
                        (h.reg.cb.midi_mapping_get_param_id)(
                            h.ctx,
                            bus,
                            channel,
                            controller,
                            &raw mut id,
                        )
                    } != 0
                    {
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

/// The SDK 3.8 list form of `IMidiMapping`, input direction only. Moose
/// has no MIDI 2 controller bindings.
impl IMidiMapping2Trait for Component {
    unsafe fn getNumMidi2ControllerAssignments(&self, _direction: BusDirections) -> uint32 {
        0
    }

    unsafe fn getMidi2ControllerAssignments(
        &self,
        _direction: BusDirections,
        _list: *const Midi2ControllerParamIDAssignmentList,
    ) -> tresult {
        kResultFalse
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
        let want = unsafe { CStr::from_ptr(name) };
        let found = h
            .reg
            .params
            .iter()
            .zip(&h.reg.param_unit)
            .filter(|&(_, &u)| unit == kRootUnitId || u == unit)
            .map(|(p, _)| p.id)
            .find(|&pid| {
                let f = unsafe { (h.reg.cb.param_function_name)(h.ctx, pid) };
                !f.is_null() && unsafe { CStr::from_ptr(f) } == want
            });
        unsafe { *id = found.unwrap_or(kNoParamId) };
        if found.is_some() {
            kResultTrue
        } else {
            kResultFalse
        }
    }
}

impl IInfoListenerTrait for Component {
    unsafe fn setChannelContextInfos(&self, list: *mut IAttributeList) -> tresult {
        let Some(list) = (unsafe { ComRef::from_raw(list) }) else {
            return kInvalidArgument;
        };
        let mut name = [0 as TChar; 256];
        let name = (unsafe {
            list.getString(
                kChannelNameKey,
                name.as_mut_ptr(),
                size_of_val(&name) as u32,
            )
        } == kResultOk)
            .then(|| CString::new(unsafe { utf16_from(name.as_ptr(), name.len()) }).ok())
            .flatten();
        let mut color = 0i64;
        let has_color = unsafe { list.getInt(kChannelColorKey, &raw mut color) } == kResultOk;
        let info = Vst3TrackInfo {
            name: name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            // `ColorSpec` is a 32-bit ARGB value carried in an int64.
            color_argb: color as u32,
            has_color,
        };
        unsafe { (self.host.reg.cb.track_info_changed)(self.host.ctx, &raw const info) };
        kResultTrue
    }
}

impl Component {
    fn midi_learn(&self, bus: int32, channel: i16, source: Vst3LearnSource) -> tresult {
        let (Ok(bus), Ok(channel @ 0..=15)) = (u32::try_from(bus), u8::try_from(channel)) else {
            return kInvalidArgument;
        };
        let h = &self.host;
        // SAFETY: `ctx` is this component's live plugin instance, and the
        // learn interfaces are UI-thread calls, as the callback requires.
        if unsafe { (h.reg.cb.midi_learn)(h.ctx, bus, channel, source) } != 0 {
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

impl IUnitInfoTrait for Component {
    unsafe fn getUnitCount(&self) -> int32 {
        self.host.reg.units.len() as i32
    }

    unsafe fn getUnitInfo(&self, index: int32, info: *mut UnitInfo) -> tresult {
        let unit = usize::try_from(index)
            .ok()
            .and_then(|i| self.host.reg.units.get(i));
        let (Some(info), Some(unit)) = (unsafe { info.as_mut() }, unit) else {
            return kInvalidArgument;
        };
        info.id = unit.id;
        info.parentUnitId = unit.parent;
        utf16_into(&mut info.name, &unit.name);
        info.programListId = kNoProgramListId;
        kResultOk
    }

    unsafe fn getProgramListCount(&self) -> int32 {
        0
    }

    unsafe fn getProgramListInfo(&self, _index: int32, _info: *mut ProgramListInfo) -> tresult {
        kNotImplemented
    }

    unsafe fn getProgramName(
        &self,
        _list: ProgramListID,
        _index: int32,
        _name: *mut String128,
    ) -> tresult {
        kNotImplemented
    }

    unsafe fn getProgramInfo(
        &self,
        _list: ProgramListID,
        _index: int32,
        _attribute: vst3::Steinberg::Vst::CString,
        _value: *mut String128,
    ) -> tresult {
        kNotImplemented
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

    unsafe fn setUnitProgramData(
        &self,
        _list: int32,
        _index: int32,
        _data: *mut IBStream,
    ) -> tresult {
        kNotImplemented
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
            (unsafe { info.as_mut() }, entry)
        else {
            return kInvalidArgument;
        };
        if unsafe { self.getNoteExpressionCount(bus, channel) } == 0 {
            return kInvalidArgument;
        }
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
        match note_expression_value(id, &unsafe { utf16_from(string, 64) }) {
            Some(v) => {
                unsafe { *value = v };
                kResultOk
            }
            None => kResultFalse,
        }
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
        assert_eq!(default_active_mask(3, kinds.as_ptr()), 0b101);
        assert_eq!(default_active_mask(2, std::ptr::null()), 0b11);
    }
}
