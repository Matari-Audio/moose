//! Owned `MooseAaxHostSink` vtable wrapper and main-thread `HostQueue` drain.
//!
//! Pro Tools / AAX drives `TimerWakeup()` on the host UI/main thread (~30 ms).
//! That callback invokes `cbs->drain_host_queue(ctx)`, which drains the
//! instance's [`HostQueue`] signals, off-UI-thread [`EditorGesture`] queue,
//! pending resize requests, and `ReportedDelay` changes through [`AaxHostSink`].

use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use moose_core::Float;
use moose_core::TransportSlot;
use moose_core::c_abi_driver::EditorGesture;
use moose_core::editor::{EditorBridge, HostContext, PluginContext};
use moose_core::events::TransportInfo;
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostQueue, HostSignal, HostSink, dispatch};
use moose_core::main_thread::HostNotice;
use moose_core::meters::MeterStore;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state::{self, StateLoadQueue};
use moose_core::thread::MainThread;
use moose_core::wrapper::custom_state_view;
use moose_params::Params;

use crate::ffi::MooseAaxHostSink;
use crate::instance::{AaxCtx, AaxInstance};

// ---------------------------------------------------------------------------
// Prefix-safe owned wrapper over MooseAaxHostSink
// ---------------------------------------------------------------------------

/// Owned value-copy of [`MooseAaxHostSink`] stored as the last field of
/// [`AaxInstance`] so `release_user_data` runs once after `host_queue.close()`
/// and editor teardown complete.
pub(crate) struct AaxHostSink(MooseAaxHostSink);

// SAFETY: `user_data` is an opaque host pointer accessed strictly on the host
// UI/main thread inside `drain_host_queue` / editor hooks, and released once
// on Drop after `HostQueue::close()`.
unsafe impl Send for AaxHostSink {}
unsafe impl Sync for AaxHostSink {}

impl AaxHostSink {
    pub(crate) fn empty() -> Self {
        Self(MooseAaxHostSink::default())
    }

    /// Copy only complete fields whose full extent fits inside `(*raw).size`.
    ///
    /// # Safety
    /// `raw` is either null or points to a readable `MooseAaxHostSink` prefix
    /// of at least `(*raw).size` bytes.
    pub(crate) unsafe fn from_raw(raw: *const MooseAaxHostSink) -> Self {
        if raw.is_null() {
            return Self::empty();
        }
        let declared = unsafe { (*raw).size as usize };
        if declared < std::mem::size_of::<u32>() * 2 {
            return Self::empty();
        }
        let mut copy = MooseAaxHostSink::default();
        macro_rules! copy_field {
            ($field:ident) => {{
                let end = std::mem::offset_of!(MooseAaxHostSink, $field)
                    + std::mem::size_of_val(&copy.$field);
                if declared >= end {
                    copy.$field = unsafe { std::ptr::addr_of!((*raw).$field).read() };
                }
            }};
        }
        copy_field!(_reserved);
        copy_field!(user_data);
        copy_field!(release_user_data);
        copy_field!(touch_param);
        copy_field!(set_param_normalized);
        copy_field!(release_param);
        copy_field!(latency_changed);
        copy_field!(state_dirty);
        copy_field!(param_values_changed);
        copy_field!(param_presentation_changed);
        copy_field!(request_resize);
        #[allow(clippy::cast_possible_truncation)]
        {
            copy.size = declared.min(std::mem::size_of::<MooseAaxHostSink>()) as u32;
        }
        Self(copy)
    }

    pub(crate) unsafe fn touch_param(&self, param_id: u32) {
        if let Some(f) = self.0.touch_param {
            unsafe { f(self.0.user_data, param_id) };
        }
    }

    pub(crate) unsafe fn set_param_normalized(&self, param_id: u32, normalized: f64) {
        if let Some(f) = self.0.set_param_normalized {
            unsafe { f(self.0.user_data, param_id, normalized) };
        }
    }

    pub(crate) unsafe fn release_param(&self, param_id: u32) {
        if let Some(f) = self.0.release_param {
            unsafe { f(self.0.user_data, param_id) };
        }
    }

    pub(crate) unsafe fn latency_changed(&self, samples: u32) {
        if let Some(f) = self.0.latency_changed {
            unsafe { f(self.0.user_data, samples) };
        }
    }

    pub(crate) unsafe fn state_dirty(&self) {
        if let Some(f) = self.0.state_dirty {
            unsafe { f(self.0.user_data) };
        }
    }

    pub(crate) unsafe fn param_values_changed(&self) {
        if let Some(f) = self.0.param_values_changed {
            unsafe { f(self.0.user_data) };
        }
    }

    pub(crate) unsafe fn param_presentation_changed(&self) {
        if let Some(f) = self.0.param_presentation_changed {
            unsafe { f(self.0.user_data) };
        }
    }

    pub(crate) unsafe fn request_resize(&self, width: u32, height: u32) -> bool {
        if let Some(f) = self.0.request_resize {
            unsafe { f(self.0.user_data, width, height) != 0 }
        } else {
            false
        }
    }
}

impl Drop for AaxHostSink {
    fn drop(&mut self) {
        if let Some(release) = self.0.release_user_data.take() {
            let ud = self.0.user_data;
            self.0.user_data = std::ptr::null_mut();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                release(ud);
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Editor HostContext / EditorBridge
// ---------------------------------------------------------------------------

/// One AAX instance seen from its editor.
pub(crate) struct AaxHost<P: PluginExport> {
    ctx: AaxCtx,
    params: Arc<P::Params>,
    meter_store: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
    pending_state: Arc<StateLoadQueue>,
    transport: Arc<TransportSlot>,
    queue: Arc<HostQueue>,
    _plugin: PhantomData<fn() -> P>,
}

impl<P: PluginExport> AaxHost<P> {
    /// # Safety
    /// `ctx` must be a live `AaxInstance<P>` pointer.
    unsafe fn new(ctx: *mut c_void) -> Self {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        Self {
            ctx: AaxCtx(ctx),
            params: Arc::clone(&inst.params_arc),
            meter_store: Arc::clone(&inst.meter_store),
            snapshot: Arc::clone(&inst.snapshot),
            pending_state: Arc::clone(&inst.pending_state),
            transport: Arc::clone(&inst.transport_slot),
            queue: Arc::clone(&inst.host_queue),
            _plugin: PhantomData,
        }
    }

    fn push_gesture(&self, gesture: EditorGesture) {
        let _ = self.queue.with_open(|| {
            let inst = unsafe { &*self.ctx.as_ptr().cast::<AaxInstance<P>>() };
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
            }
        });
    }
}

impl<P: PluginExport> EditorBridge for AaxHost<P> {
    fn begin_edit(&self, id: u32) {
        self.push_gesture(EditorGesture::BeginEdit(id));
    }

    fn set_param(&self, id: u32, normalized: f64) {
        let clamped = normalized.clamp(0.0, 1.0);
        let _ = self.params.set_normalized_returning_plain(id, clamped);
        self.push_gesture(EditorGesture::SetParam(id, f32::from_f64(clamped)));
    }

    fn end_edit(&self, id: u32) {
        self.push_gesture(EditorGesture::EndEdit(id));
    }

    fn request_resize(&self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 {
            return false;
        }
        self.queue
            .with_open(|| {
                let inst = unsafe { &*self.ctx.as_ptr().cast::<AaxInstance<P>>() };
                if !inst.is_ui_thread() {
                    inst.pending_resize
                        .store((u64::from(w) << 32) | u64::from(h), Ordering::Release);
                    return true;
                }
                if let Some(mut gui) = inst.gui.try_enter() {
                    let resized = gui.editor.as_mut().is_some_and(|e| e.set_size(w, h));
                    if resized && let Some((aw, ah)) = gui.editor.as_ref().map(|e| e.size()) {
                        return unsafe { inst.host_sink.request_resize(aw, ah) };
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

impl<P: PluginExport> HostContext for AaxHost<P> {
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
/// `ctx` must be a live `AaxInstance<P>` pointer.
pub(crate) unsafe fn editor_context<P: PluginExport>(ctx: *mut c_void) -> PluginContext {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let host: Arc<dyn HostContext> = Arc::new(unsafe { AaxHost::<P>::new(ctx) });
    PluginContext::from_host(host, Arc::clone(&inst.params_arc) as Arc<dyn Params>)
        .with_tasks(inst.task_spawner.clone())
}

// ---------------------------------------------------------------------------
// Main-thread HostQueue + GestureQueue drain (driven from AAX TimerWakeup)
// ---------------------------------------------------------------------------

pub(crate) fn drain_editor_gestures<P: PluginExport>(inst: &AaxInstance<P>) {
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
            EditorGesture::BeginEdit(id) => unsafe { inst.host_sink.touch_param(id) },
            EditorGesture::SetParam(id, normalized) => unsafe {
                inst.host_sink
                    .set_param_normalized(id, f64::from(normalized));
            },
            EditorGesture::EndEdit(id) => unsafe { inst.host_sink.release_param(id) },
        }
    }
}

fn apply_pending_resize<P: PluginExport>(inst: &AaxInstance<P>) {
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
            let _ = unsafe { inst.host_sink.request_resize(aw, ah) };
        }
    } else {
        let _ =
            inst.pending_resize
                .compare_exchange(0, packed, Ordering::AcqRel, Ordering::Relaxed);
    }
}

struct AaxSink<'a, P: PluginExport> {
    ctx: *mut c_void,
    inst: &'a AaxInstance<P>,
    sync_catalog: bool,
    param_values: bool,
    presentation: bool,
}

impl<P: PluginExport> HostSink for AaxSink<'_, P> {
    fn persist_changed(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        // Any #[persist] field mutation dirties the AAX chunk and optionally
        // touches a mirror parameter if the plugin bound one.
        unsafe { self.inst.host_sink.state_dirty() };
        if let Some((id, value)) = self.inst.params_arc.take_persist_change()
            && value.is_finite()
            && (0.0..=1.0).contains(&value)
            && self.inst.params_arc.get_normalized(id).is_some()
        {
            let context = unsafe { editor_context::<P>(self.ctx) };
            context.automate(id, value);
        }
    }

    fn latency_changed(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        // A reset may already have published the prepared latency to both
        // slots. The queued signal still carries a submission obligation.
        let _ = self.inst.delay.commit_latency();
        let samples = self.inst.delay.latency();
        unsafe { self.inst.host_sink.latency_changed(samples) };
    }

    fn tail_changed(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        let _ = self.inst.delay.commit_tail();
    }

    fn mark_dirty(&mut self) {
        if self.inst.host_queue.is_closed() {
            return;
        }
        unsafe { self.inst.host_sink.state_dirty() };
    }

    fn param_values_changed(&mut self) {
        self.sync_catalog = true;
        self.param_values = true;
    }

    fn presentation_changed(&mut self) {
        self.sync_catalog = true;
        self.presentation = true;
    }
}

/// Drain the instance's `HostQueue`, off-UI-thread `GestureQueue`, and pending
/// resize on the host UI/main thread. Called from `cb_drain_host_queue` (AAX
/// `TimerWakeup`).
///
/// # Safety
/// `ctx` must be a live `AaxInstance<P>` pointer and this must run on the
/// host UI/main thread.
pub(crate) unsafe fn run_host_drain<P: PluginExport>(ctx: *mut c_void) {
    let queue = {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        Arc::clone(&inst.host_queue)
    };
    let _ = queue.with_open(|| {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        inst.record_ui_thread();
        // SAFETY: AAX invokes `TimerWakeup` strictly on the host UI/main thread.
        let main = unsafe { MainThread::assume() };
        let mut sink = AaxSink {
            ctx,
            inst,
            sync_catalog: false,
            param_values: false,
            presentation: false,
        };
        let drain = inst.host_queue.drain(&main);
        dispatch(drain, &mut sink);
        if sink.sync_catalog && !inst.host_queue.is_closed() {
            inst.sync_catalog();
        }
        if sink.presentation && !inst.host_queue.is_closed() {
            unsafe { inst.host_sink.param_presentation_changed() };
        }
        if sink.param_values && !inst.host_queue.is_closed() {
            unsafe { inst.host_sink.param_values_changed() };
        }
        drain_editor_gestures(inst);
        apply_pending_resize(inst);
        if let Some(mut gui) = inst.gui.try_enter()
            && let Some(editor) = gui.editor.as_mut()
        {
            editor.idle();
        }
    });
}
