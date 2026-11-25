//! Editor callbacks, `HostContext`, and main-thread `HostQueue` drain.

use crate::instance::{AuCtx, AuInstance, EditorGesture, StateLoadQueue};

use moose_core::Float;
use moose_core::TransportSlot;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::editor::RawWindowHandle;
use moose_core::editor::{Editor, EditorBridge, HostContext, PluginContext, fit_logical_size};
use moose_core::events::TransportInfo;
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostQueue, HostSignal, HostSink, dispatch};
use moose_core::main_thread::HostNotice;
use moose_core::meters::MeterStore;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::thread::MainThread;
use moose_core::wrapper::{custom_state_view, run_extern_callback_with};
use moose_params::Params;
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

thread_local! {
    /// Target `(ctx, param_id, value bits)` strictly while `drain_host_queue` invokes
    /// `HostSink::set_param` so only the synchronous re-entrant echo for that
    /// exact instance + parameter skips bumping `param_edit_seqs`.
    static IN_AU_FEEDBACK_DRAIN: Cell<Option<(*mut std::ffi::c_void, u32, u64)>> = const { Cell::new(None) };
    /// Active host registrations and their deferred destruction flags, so
    /// a synchronous re-entrant `cb_destroy` defers `HostQueue::close` + free
    /// until `with_open` returns instead of deadlocking on its own registration.
    static ACTIVE_DRAIN_CTX: RefCell<Vec<(*mut std::ffi::c_void, bool)>> = const { RefCell::new(Vec::new()) };
}

#[inline]
pub(crate) fn take_feedback_echo(ctx: *mut std::ffi::c_void, param_id: u32, value: f64) -> bool {
    IN_AU_FEEDBACK_DRAIN.with(|cell| {
        if cell.get() == Some((ctx, param_id, value.to_bits())) {
            cell.set(None);
            true
        } else {
            false
        }
    })
}

#[inline]
pub(crate) fn defer_host_destroy(ctx: *mut std::ffi::c_void) -> bool {
    ACTIVE_DRAIN_CTX.with_borrow_mut(|stack| {
        if let Some((_, deferred)) = stack.iter_mut().find(|(active, _)| *active == ctx) {
            *deferred = true;
            true
        } else {
            false
        }
    })
}

struct DrainCtxGuard;

impl Drop for DrainCtxGuard {
    fn drop(&mut self) {
        ACTIVE_DRAIN_CTX.with_borrow_mut(Vec::pop);
    }
}

/// Run a host callback under the queue lifetime gate. Only the outermost
/// registration for this instance can take ownership of deferred teardown.
///
/// # Safety
/// `ctx` is the `AuInstance<P>` that owns `queue`, live until `queue` closes.
pub(crate) unsafe fn with_host_open<P: PluginExport, R>(
    queue: &HostQueue,
    ctx: *mut std::ffi::c_void,
    f: impl FnOnce() -> R,
) -> Option<R> {
    let mut finalize = false;
    let result = queue.with_open(|| {
        ACTIVE_DRAIN_CTX.with_borrow_mut(|stack| stack.push((ctx, false)));
        let _guard = DrainCtxGuard;
        let result = f();
        finalize = ACTIVE_DRAIN_CTX.with_borrow(|stack| stack.last().unwrap().1);
        result
    });
    if finalize {
        // SAFETY: `finalize` is set only when a re-entrant `cb_destroy` marked this registration
        // for deferral, so this is that destroy's single deferred teardown. `with_open` has
        // returned, so `close()` inside `finalize_destroy` cannot wait on this thread's own
        // registration.
        unsafe { crate::callbacks::finalize_destroy::<P>(ctx.cast()) };
    }
    result
}

struct FeedbackDrainGuard(Option<(*mut std::ffi::c_void, u32, u64)>);

impl FeedbackDrainGuard {
    fn enter(ctx: *mut std::ffi::c_void, param_id: u32, value: f32) -> Self {
        let prev = IN_AU_FEEDBACK_DRAIN
            .with(|c| c.replace(Some((ctx, param_id, f64::from(value).to_bits()))));
        Self(prev)
    }
}

impl Drop for FeedbackDrainGuard {
    fn drop(&mut self) {
        IN_AU_FEEDBACK_DRAIN.with(|c| c.set(self.0));
    }
}

// ---------------------------------------------------------------------------
// HostContext
// ---------------------------------------------------------------------------

/// One AU instance seen from its editor.
pub(crate) struct AuHost<P: PluginExport> {
    ctx: AuCtx,
    params: Arc<P::Params>,
    meter_store: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
    pending_state: Arc<StateLoadQueue>,
    transport: Arc<TransportSlot>,
    queue: Arc<HostQueue>,
    _plugin: PhantomData<fn() -> P>,
}

impl<P: PluginExport> AuHost<P> {
    fn send_gesture(&self, gesture: EditorGesture) {
        self.with_open(|| {
            // SAFETY: Runs inside `with_open`: the queue is open, so `finalize_destroy` (which
            // closes it first) has not freed the instance; only a shared reference is formed.
            let inst = unsafe { &*self.ctx.as_ptr().cast::<AuInstance<P>>() };
            Self::queue_gesture(inst, gesture);
        });
    }

    fn queue_gesture(inst: &AuInstance<P>, gesture: EditorGesture) {
        if let EditorGesture::SetParam(id, _) = gesture
            && inst.params_arc.parameter_affects_presentation(id)
        {
            inst.host_queue.signal(HostSignal::PRESENTATION);
        }
        if inst.is_ui_thread() {
            drain_editor_gestures(inst);
        }
        if inst.host_queue.is_closed() {
            return;
        }
        inst.editor_gestures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(gesture);
        if inst.is_ui_thread() {
            drain_editor_gestures(inst);
        } else if let Some(dispatch) = &inst.host_dispatch {
            dispatch.trigger_now();
        }
    }

    fn with_open<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        // SAFETY: `self.ctx` is the instance that owns `self.queue` (both captured in `new`), which
        // is `with_host_open`'s contract.
        unsafe { with_host_open::<P, R>(&self.queue, self.ctx.as_ptr(), f) }
    }

    /// # Safety
    /// `ctx` is a live `AuInstance<P>` pointer.
    unsafe fn new(ctx: *mut std::ffi::c_void) -> Self {
        // SAFETY: The caller guarantees `ctx` is a live `AuInstance<P>`; the fields are cloned out
        // and the reference is not kept.
        let inst = unsafe { &*ctx.cast::<AuInstance<P>>() };
        Self {
            ctx: AuCtx(ctx),
            params: Arc::clone(&inst.params_arc),
            meter_store: Arc::clone(&inst.meter_store),
            snapshot: Arc::clone(&inst.snapshot),
            pending_state: Arc::clone(&inst.pending_state),
            transport: Arc::clone(&inst.transport_slot),
            queue: Arc::clone(&inst.host_queue),
            _plugin: PhantomData,
        }
    }
}

fn drain_editor_gestures<P: PluginExport>(inst: &AuInstance<P>) {
    loop {
        let Some(gesture) = inst
            .editor_gestures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending
            .pop_front()
        else {
            return;
        };
        if inst.host_queue.is_closed() {
            return;
        }
        match gesture {
            // SAFETY: Called on the host main thread (UI-thread check or the main-queue drain)
            // while the queue is open, so the sink's `user_data` has not been released.
            EditorGesture::BeginEdit(id) => unsafe { inst.host_sink.begin_edit(id) },
            // SAFETY: Main thread with the queue open; the sink's `user_data` has not been
            // released.
            EditorGesture::SetParam(id, plain) => unsafe { inst.host_sink.set_param(id, plain) },
            // SAFETY: Main thread with the queue open; the sink's `user_data` has not been
            // released.
            EditorGesture::EndEdit(id) => unsafe { inst.host_sink.end_edit(id) },
        }
    }
}

#[cfg_attr(
    not(any(target_vendor = "apple", test)),
    allow(
        dead_code,
        reason = "used by drain_host_queue on Apple targets and in tests"
    )
)]
fn apply_pending_resize<P: PluginExport>(inst: &AuInstance<P>) {
    let packed = inst.pending_resize.swap(0, Ordering::AcqRel);
    if packed == 0 || inst.host_queue.is_closed() {
        return;
    }
    #[allow(clippy::cast_possible_truncation)]
    let (w, h) = ((packed >> 32) as u32, packed as u32);
    if w == 0 || h == 0 {
        return;
    }
    if let Some(mut gui) = inst.gui.try_enter() {
        let resized = gui.editor.as_mut().is_some_and(|e| e.set_size(w, h));
        if resized && let Some((aw, ah)) = gui.editor.as_ref().map(|e| e.size()) {
            // SAFETY: Main-thread drain with the queue open (checked above), so the sink's
            // `user_data` has not been released.
            let handled = unsafe { inst.host_sink.request_resize(aw, ah) };
            #[cfg(target_os = "macos")]
            if !handled && gui.view != 0 {
                // SAFETY: `gui.view` is non-zero only while `gui_open` holds the host's `NSView`,
                // and `finalize_destroy` detaches it before the view can go away; this runs on the
                // main thread as `moose_au_view_fit` requires.
                unsafe { moose_au_view_fit(gui.view as *mut std::ffi::c_void, aw, ah) };
            }
            #[cfg(not(target_os = "macos"))]
            let _ = handled;
        }
    } else {
        // Still re-entrantly inside a GUI callback; restore for the next drain.
        let _ =
            inst.pending_resize
                .compare_exchange(0, packed, Ordering::AcqRel, Ordering::Relaxed);
    }
}

impl<P: PluginExport> EditorBridge for AuHost<P> {
    fn begin_edit(&self, id: u32) {
        self.send_gesture(EditorGesture::BeginEdit(id));
    }

    fn set_param(&self, id: u32, normalized: f64) {
        self.with_open(|| {
            // SAFETY: Runs inside `with_open`, so the instance has not been finalized; only a
            // shared reference is formed.
            let inst = unsafe { &*self.ctx.as_ptr().cast::<AuInstance<P>>() };
            let plain = inst.edit_param(id, || {
                self.params.set_normalized_returning_plain(id, normalized)
            });
            Self::queue_gesture(inst, EditorGesture::SetParam(id, f32::from_f64(plain)));
        });
    }

    fn end_edit(&self, id: u32) {
        self.send_gesture(EditorGesture::EndEdit(id));
    }

    fn request_resize(&self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 {
            return false;
        }
        self.with_open(|| {
            // SAFETY: `with_open` keeps the `AuInstance<P>` alive against `cb_destroy`.
            let inst = unsafe { &*self.ctx.as_ptr().cast::<AuInstance<P>>() };
            if !inst.is_ui_thread() {
                inst.pending_resize
                    .store((u64::from(w) << 32) | u64::from(h), Ordering::Release);
                if let Some(dispatch) = &inst.host_dispatch {
                    dispatch.trigger_now();
                }
                return true;
            }
            if let Some(mut gui) = inst.gui.try_enter() {
                let resized = gui.editor.as_mut().is_some_and(|e| e.set_size(w, h));
                if resized && let Some((aw, ah)) = gui.editor.as_ref().map(|e| e.size()) {
                    // SAFETY: UI thread (checked above) inside `with_open`, so the sink's
                    // `user_data` has not been released.
                    let handled = unsafe { inst.host_sink.request_resize(aw, ah) };
                    #[cfg(target_os = "macos")]
                    if !handled && gui.view != 0 {
                        // SAFETY: `gui.view` is the host `NSView` recorded by `gui_open` and still
                        // attached; UI thread as `moose_au_view_fit` requires.
                        unsafe { moose_au_view_fit(gui.view as *mut std::ffi::c_void, aw, ah) };
                    }
                    #[cfg(not(target_os = "macos"))]
                    let _ = handled;
                }
                resized
            } else {
                inst.pending_resize
                    .store((u64::from(w) << 32) | u64::from(h), Ordering::Release);
                true
            }
        })
        .unwrap_or(false)
    }

    fn get_param(&self, id: u32) -> f64 {
        self.params.get_normalized(id).unwrap_or(0.0)
    }

    fn get_param_plain(&self, id: u32) -> f64 {
        self.params.get_plain(id).unwrap_or(0.0)
    }

    fn format_param(&self, id: u32) -> String {
        let plain = self.params.get_plain(id).unwrap_or(0.0);
        self.params
            .format_value(id, plain)
            .unwrap_or_else(|| format!("{plain:.1}"))
    }

    fn get_meter(&self, id: u32) -> f32 {
        self.meter_store.read(id)
    }

    fn get_state(&self) -> Vec<u8> {
        custom_state_view(&self.snapshot, &self.pending_state)
    }

    fn set_state(&self, data: Vec<u8>) {
        self.pending_state.force_push(state::DeserializedState {
            extra: Some(data),
            ..Default::default()
        });
    }

    fn transport(&self) -> Option<TransportInfo> {
        self.transport.read()
    }
}

impl<P: PluginExport> HostContext for AuHost<P> {
    fn mark_dirty(&self) {
        self.queue.signal(HostSignal::MARK_DIRTY);
    }

    fn request_restart(&self) {
        self.queue.signal(HostSignal::RESTART);
    }

    fn notify(&self, notice: HostNotice) {
        self.queue.signal(notice);
    }
}

/// Build the [`PluginContext`] handed to the instance's editor.
///
/// # Safety
/// `ctx` is a live `AuInstance<P>` pointer.
pub(crate) unsafe fn editor_context<P: PluginExport>(ctx: *mut std::ffi::c_void) -> PluginContext {
    // SAFETY: The caller guarantees `ctx` is a live `AuInstance<P>`.
    let inst = unsafe { &*ctx.cast::<AuInstance<P>>() };
    // SAFETY: Same live `ctx`. `AuHost` only dereferences it later inside `with_open`, which fails
    // once the instance is closed.
    let host: Arc<dyn HostContext> = Arc::new(unsafe { AuHost::<P>::new(ctx) });
    PluginContext::from_host(host, Arc::clone(&inst.params_arc) as Arc<dyn Params>)
        .with_tasks(inst.task_spawner.clone())
}

#[cfg_attr(
    not(any(target_vendor = "apple", test)),
    allow(
        dead_code,
        reason = "used by drain_host_queue on Apple targets and in unit tests"
    )
)]
struct AuSink<'a, P: PluginExport> {
    ctx: *mut std::ffi::c_void,
    inst: &'a AuInstance<P>,
    sync_catalog: bool,
    param_rescan: bool,
}

impl<P: PluginExport> HostSink for AuSink<'_, P> {
    fn persist_changed(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        if let Some((id, value)) = self.inst.params_arc.take_persist_change()
            && value.is_finite()
            && (0.0..=1.0).contains(&value)
            && self.inst.params_arc.get_normalized(id).is_some()
        {
            // SAFETY: `self.ctx` is a live `AuInstance<P>`.
            let context = unsafe { editor_context::<P>(self.ctx) };
            context.automate(id, value);
        }
    }

    fn latency_changed(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        // Promote observed latency BEFORE firing AU property / KVO
        // notifications so synchronous host re-reads immediately see the new
        // value. Commit only the latency lane so a latency-only drain can
        // never silently publish a racing tail observation.
        // Preparation can publish both slots before this queued obligation runs.
        let _ = self.inst.delay.commit_latency();
        // SAFETY: main thread, live instance.
        unsafe { self.inst.host_sink.latency_changed() };
    }

    fn mark_dirty(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        // SAFETY: main thread, live instance.
        unsafe { self.inst.host_sink.state_changed() };
    }

    fn audio_ports_changed(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        // SAFETY: main thread, live instance.
        unsafe { self.inst.host_sink.audio_ports_changed() };
    }

    // `request_restart` stays a no-op: AU hosts re-read latency, tail and
    // stream formats live, and there is no "please reinitialize me" call.

    fn tail_changed(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        let _ = self.inst.delay.commit_tail();
        // SAFETY: main thread, live instance.
        unsafe { self.inst.host_sink.tail_changed() };
    }

    fn param_values_changed(&mut self) {
        self.sync_catalog = true;
        self.param_rescan = true;
    }

    fn presentation_changed(&mut self) {
        self.sync_catalog = true;
    }

    fn flush_signals(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        if self.sync_catalog {
            let exposed = crate::catalog::current_snapshot(&self.inst.exposed);
            let (catalog_values, fresh, retry) = exposed.sync(self.inst.params_arc.as_ref());
            let presentation_changed = fresh.is_some();
            if retry && !self.inst.host_queue.is_closed() {
                // Keep the last verified snapshot and retry once presentation
                // updates settle instead of publishing an incoherent snapshot.
                self.inst
                    .host_queue
                    .signal(moose_core::host_queue::HostSignal::PRESENTATION);
            }
            if let Some(fresh) = fresh {
                let catalog = fresh.render_catalog();
                self.inst.param_edit_seqs.extend_catalog(&catalog);
                let _guard = self
                    .inst
                    .lifecycle_lock
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                *self
                    .inst
                    .exposed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = fresh;
                self.inst.render_catalog.publish(catalog);
                if self.inst.is_known_inactive() {
                    let mut audio = self.inst.audio.enter();
                    self.inst.render_catalog.reclaim();
                    self.inst.render_catalog.install(&mut audio.catalog);
                    self.inst.render_catalog.reclaim();
                }
            }
            if presentation_changed && !self.inst.host_queue.is_closed() {
                // SAFETY: Main-thread drain with the queue still open (checked just above), so the
                // sink's `user_data` has not been released.
                unsafe { self.inst.host_sink.param_presentation_changed() };
            }
            if (self.param_rescan || catalog_values) && !self.inst.host_queue.is_closed() {
                // SAFETY: Main-thread drain with the queue still open (checked just above), so the
                // sink's `user_data` has not been released.
                unsafe { self.inst.host_sink.param_values_changed() };
            }
        }
    }
}

/// Drain the instance's [`HostQueue`] and process-emitted parameter feedback
/// queue on the host main thread.
///
/// # Safety
/// `ctx` is a live `AuInstance<P>` pointer.
pub(crate) unsafe fn drain_host_queue<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    main: &MainThread,
) {
    // SAFETY: The caller guarantees `ctx` is a live `AuInstance<P>`; only a shared reference is
    // formed.
    let inst = unsafe { &*ctx.cast::<AuInstance<P>>() };

    inst.render_catalog.reclaim();
    if let Some(mut hooks) = inst.main_thread.try_borrow_mut(main) {
        let info = inst
            .pending_context_name
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(info) = info {
            // Keep draining the armed host queue even if the plugin hook panics.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hooks.track_info_changed(main, &info);
            }));
        }
    } else if inst
        .pending_context_name
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
        && let Some(dispatch) = &inst.host_dispatch
    {
        dispatch.trigger_now();
    }
    if inst.host_queue.is_closed() {
        return;
    }

    // 1. Deliver any off-UI-thread editor gestures and deferred resizes on
    //    the main/UI thread before host feedback.
    drain_editor_gestures(inst);
    apply_pending_resize(inst);

    // 2. Drain process-emitted ParamChange feedback through the host sink,
    //    skipping any stale entry superseded by a newer host/editor edit.
    while let Some((id, value, epoch)) = inst.param_out_queue.pop() {
        if inst.host_queue.is_closed() {
            return;
        }
        if !inst.feedback_is_current(id, epoch) {
            continue;
        }
        let _guard = FeedbackDrainGuard::enter(ctx, id, value);
        // SAFETY: Main-thread drain with the queue open (checked at the top of this iteration), so
        // the sink's `user_data` has not been released. A re-entrant destroy from the callback is
        // deferred by `with_host_open`.
        unsafe { inst.host_sink.set_param(id, value) };
    }

    if inst.host_queue.is_closed() {
        return;
    }

    // 3. Drain coalesced HostQueue signals via the shared `HostSink` dispatch.
    let drain = inst.host_queue.drain(main);
    let mut sink = AuSink {
        ctx,
        inst,
        sync_catalog: false,
        param_rescan: false,
    };
    dispatch(drain, &mut sink);
}

// ---------------------------------------------------------------------------
// GUI callbacks
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn cb_gui_has_editor<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> i32 {
    // SAFETY: `ctx` is null-checked and otherwise the shim's live instance; the editor slot goes
    // through the `gui` cell.
    run_extern_callback_with::<P, i32>("au", "gui_has_editor", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut gui = inst.gui.enter();
        if gui.editor.is_none() {
            gui.editor = (inst.editor_builder)(inst.params_arc.clone());
        }
        i32::from(gui.editor.is_some())
    })
}

pub(crate) unsafe extern "C" fn cb_gui_can_resize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> i32 {
    // SAFETY: `ctx` is null-checked and otherwise the shim's live instance.
    run_extern_callback_with::<P, i32>("au", "gui_can_resize", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        i32::from(
            inst.gui
                .enter()
                .editor
                .as_ref()
                .is_some_and(|e| e.can_resize()),
        )
    })
}

pub(crate) unsafe extern "C" fn cb_gui_set_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: u32,
    h: u32,
) {
    // SAFETY: `ctx` is null-checked and otherwise the shim's live instance.
    run_extern_callback_with::<P, ()>("au", "gui_set_size", (), || unsafe {
        if ctx.is_null() || w == 0 || h == 0 {
            return;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut gui) = inst.gui.try_enter() else {
            return;
        };
        if let Some(editor) = gui.editor.as_mut()
            && editor.can_resize()
        {
            let (cw, ch) = fit_logical_size(w, h, editor.as_ref());
            editor.set_size(cw, ch);
        }
    });
}

pub(crate) unsafe extern "C" fn cb_gui_get_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    // SAFETY: `ctx` is null-checked and otherwise the shim's live instance; the shim always passes
    // valid `w`/`h` out-pointers.
    run_extern_callback_with::<P, ()>("au", "gui_get_size", (), || unsafe {
        if ctx.is_null() {
            return;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut gui) = inst.gui.try_enter() else {
            let packed = inst.pending_resize.load(Ordering::Relaxed);
            if packed != 0 {
                #[allow(clippy::cast_possible_truncation)]
                {
                    *w = (packed >> 32) as u32;
                    *h = packed as u32;
                }
            }
            return;
        };
        if gui.editor.is_none() {
            gui.editor = (inst.editor_builder)(inst.params_arc.clone());
        }
        let packed = inst.pending_resize.swap(0, Ordering::Relaxed);
        if packed != 0
            && let Some(editor) = gui.editor.as_mut()
        {
            #[allow(clippy::cast_possible_truncation)]
            let (rw, rh) = ((packed >> 32) as u32, packed as u32);
            editor.set_size(rw, rh);
        }
        if let Some(ref editor) = gui.editor {
            let (ew, eh) = editor.size();
            *w = ew;
            *h = eh;
        }
    });
}

pub(crate) unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) -> i32 {
    run_extern_callback_with::<P, i32>("au", "gui_open", 0, || {
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        {
            let _ = parent;
            if !ctx.is_null() {
                // SAFETY: `ctx` is non-null and is the shim's live instance.
                let _ = unsafe { editor_context::<P>(ctx) };
            }
            0
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        // SAFETY: The view container calls `gui_open` only with its live instance and the host
        // `NSView`/`UIView` that will hold the editor; that view stays valid until
        // `moose_au_view_detach` in `finalize_destroy`.
        unsafe {
            let inst = &*ctx.cast::<AuInstance<P>>();
            let mut gui = inst.gui.enter();
            if let Some(ref mut editor) = gui.editor {
                let context = editor_context::<P>(ctx);
                #[cfg(target_os = "macos")]
                let handle = RawWindowHandle::AppKit(parent);
                #[cfg(target_os = "ios")]
                let handle = RawWindowHandle::UiKit(parent);
                editor.open(handle, context);
                let opened = editor.is_open();
                gui.view = if opened { parent as usize } else { 0 };
                return i32::from(opened);
            }
            0
        }
    })
}

pub(crate) unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    // SAFETY: The view container calls `gui_close` only with its live instance. `editor_ptr` comes
    // from the `gui` guard held across the call, so it is the only access to the editor.
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut gui = inst.gui.enter();
        if let Some(editor) = gui.editor.as_mut() {
            let editor_ptr: *mut dyn Editor = editor.as_mut();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (*editor_ptr).close();
            }));
        }
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `au_v2_view.m`: forget the instance so the container stops calling
    /// `gui_open` / `gui_close` on it.
    pub(crate) fn moose_au_view_detach(view: *mut std::ffi::c_void);
    /// `au_v2_view.m`: the editor resized itself to `w`x`h` points; the
    /// view the host holds follows. Main thread.
    pub(crate) fn moose_au_view_fit(view: *mut std::ffi::c_void, w: u32, h: u32);
}

unsafe extern "C" {
    pub(crate) fn malloc(size: usize) -> *mut std::ffi::c_void;
    pub(crate) fn free(ptr: *mut std::ffi::c_void);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callbacks::{cb_create_with_sink, cb_destroy};
    use crate::large_state_fixture::LargePlugin;
    use moose_core::editor::PluginContextReadF64;

    type TestPlugin = LargePlugin<16>;

    #[test]
    fn au_host_context_routes_state_and_signals_through_host_queue() {
        // SAFETY: `ctx` is a fresh instance used only on this thread; `context` is a cloned handle
        // and `inst` is not used after `cb_destroy`.
        unsafe {
            let ctx = cb_create_with_sink::<TestPlugin>(std::ptr::null());
            assert!(!ctx.is_null());
            let inst = &*ctx.cast::<AuInstance<TestPlugin>>();
            let context = editor_context::<TestPlugin>(ctx);

            context.automate(0_u32, 0.75);
            assert!((PluginContextReadF64::get_param(&context, 0_u32) - 0.75).abs() < 1e-6);

            context.set_state(vec![1, 2, 3]);
            assert_eq!(context.get_state(), vec![1, 2, 3]);
            assert!(inst.host_queue.pending().contains(HostSignal::MARK_DIRTY));

            context.request_restart();
            context.notify_host(HostNotice::NOTE_NAMES);
            assert!(
                inst.host_queue.pending().contains(
                    HostSignal::MARK_DIRTY | HostSignal::RESTART | HostSignal::NOTE_NAMES
                )
            );
            cb_destroy::<TestPlugin>(ctx);
        }
    }
}
