//! CLAP 1.2 draft extensions wired behind `feature = "clap-draft"`:
//! - `clap.params-origin/1` (plugin visual/bipolar parameter origin)
//! - `clap.param-hovered/1` (host notification of hovered parameter control)
//! - `clap.undo/4` + `clap.undo_context/4` (grouped undo boundaries, host undo/redo requests, and host undo context state)

use std::collections::VecDeque;
use std::ffi::{CStr, c_char};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::clap_plugin;
use moose_core::editor::EditGroup;
use moose_core::export::PluginExport;
use moose_core::wrapper::run_extern_callback_with;
use moose_params::Params;

use crate::data_from_plugin;
use crate::draft::{
    clap_host_param_hovered, clap_host_undo, clap_plugin_params_origin, clap_plugin_undo_context,
};
use crate::host_handle::{HostHandle, MainThread};

/// Host-supplied undo context state delivered through `clap.undo_context/4`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UndoContextState {
    pub(crate) can_undo: bool,
    pub(crate) can_redo: bool,
    pub(crate) undo_name: String,
    pub(crate) redo_name: String,
}

unsafe extern "C" fn params_origin_get<P: PluginExport>(
    plugin: *const clap_plugin,
    param_id: clap_id,
    out_value: *mut f64,
) -> bool {
    run_extern_callback_with::<P, bool>("CLAP", "params_origin_get", false, || unsafe {
        if plugin.is_null() || out_value.is_null() {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        let Some(origin) = data.params_arc.param_origin(param_id) else {
            return false;
        };
        let exposed = crate::catalog_sync::lock_exposed(&data.exposed);
        let Some(info) = exposed.infos().find(|info| info.id == param_id) else {
            return false;
        };
        if !valid_origin(&info.range, origin) {
            return false;
        }
        *out_value = origin;
        true
    })
}

fn valid_origin(range: &moose_params::ParamRange, origin: f64) -> bool {
    origin.is_finite()
        && (range.min()..=range.max()).contains(&origin)
        && (crate::params_ext::range_flags(range) & clap_sys::ext::params::CLAP_PARAM_IS_STEPPED
            == 0
            || origin.fract() == 0.0)
}

pub(crate) fn make_params_origin_extension<P: PluginExport>() -> clap_plugin_params_origin {
    clap_plugin_params_origin {
        get: Some(params_origin_get::<P>),
    }
}

unsafe extern "C" fn undo_context_set_can_undo<P: PluginExport>(
    plugin: *const clap_plugin,
    can_undo: bool,
) {
    if plugin.is_null() {
        return;
    }
    let data = unsafe { data_from_plugin::<P>(plugin) };
    data.undo_context
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .can_undo = can_undo;
}

unsafe extern "C" fn undo_context_set_can_redo<P: PluginExport>(
    plugin: *const clap_plugin,
    can_redo: bool,
) {
    if plugin.is_null() {
        return;
    }
    let data = unsafe { data_from_plugin::<P>(plugin) };
    data.undo_context
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .can_redo = can_redo;
}

unsafe extern "C" fn undo_context_set_undo_name<P: PluginExport>(
    plugin: *const clap_plugin,
    name: *const c_char,
) {
    if plugin.is_null() {
        return;
    }
    let text = if name.is_null() {
        String::new()
    } else {
        // SAFETY: CLAP guarantees NUL-terminated UTF-8 when non-null.
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    };
    let data = unsafe { data_from_plugin::<P>(plugin) };
    data.undo_context
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .undo_name = text;
}

unsafe extern "C" fn undo_context_set_redo_name<P: PluginExport>(
    plugin: *const clap_plugin,
    name: *const c_char,
) {
    if plugin.is_null() {
        return;
    }
    let text = if name.is_null() {
        String::new()
    } else {
        // SAFETY: CLAP guarantees NUL-terminated UTF-8 when non-null.
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    };
    let data = unsafe { data_from_plugin::<P>(plugin) };
    data.undo_context
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .redo_name = text;
}

pub(crate) fn make_undo_context_extension<P: PluginExport>() -> clap_plugin_undo_context {
    clap_plugin_undo_context {
        set_can_undo: Some(undo_context_set_can_undo::<P>),
        set_can_redo: Some(undo_context_set_can_redo::<P>),
        set_undo_name: Some(undo_context_set_undo_name::<P>),
        set_redo_name: Some(undo_context_set_redo_name::<P>),
    }
}

/// `[main-thread]` Forward a pointer-hover change to `clap.param-hovered/1`.
///
/// # Safety
/// `host` and `ext` (if non-null) must still be alive.
pub(crate) unsafe fn notify_param_hovered(
    host: HostHandle<MainThread>,
    ext: *const clap_host_param_hovered,
    id: Option<u32>,
) {
    let raw = host.as_raw();
    if raw.is_null() || ext.is_null() {
        return;
    }
    if let Some(update) = unsafe { (*ext).update } {
        unsafe { update(raw, id.unwrap_or(CLAP_INVALID_ID)) };
    }
}

/// Maximum number of queued edit-group pairs (`Begin` + reserved `End`)
/// waiting for the host main thread. Each admitted `Begin` reserves its
/// matching `End` slot so an open change can never lose its closing edge.
const MAX_PENDING_GROUPS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UndoOp {
    Begin,
    End,
    RequestUndo,
    RequestRedo,
}

/// Frozen state stream, or a capture failure (never substitute later live state).
pub(crate) type UndoSnapshot = Result<std::sync::Arc<Vec<u8>>, ()>;

#[derive(Clone, Debug)]
struct QueuedEdge {
    op: UndoOp,
    gui_seq: u64,
    snapshot: Option<UndoSnapshot>,
}

#[derive(Default)]
struct UndoGroupInner {
    pending: VecDeque<QueuedEdge>,
    explicit_reporting: bool,
    delivering: Option<u64>,
    /// Number of admitted `Begin` edges currently awaiting their matching `End`
    /// (on the editor side) — each reserves one slot in `pending`.
    open_admitted: usize,
    /// Number of `Begin` edges rejected at capacity whose matching `End` must
    /// also be suppressed so we never emit an unpaired `change_made`.
    open_suppressed: usize,
    /// Latest custom-state bytes written while an admitted edit group is open.
    staged_custom_state: Option<Vec<u8>>,
    /// Nesting depth of `begin_change` currently open on the host.
    host_open_depth: usize,
}

/// Ordered, reservation-backed `clap.undo/4` edit-group queue.
///
/// Coordinates `[main-thread]` `clap_host_undo` (`begin_change` / `change_made`
/// / `cancel_change`) with audio/flush-thread `gui_changes` draining via
/// lock-free sequence atomics so every `begin_change` precedes the gestures it
/// encloses and every `change_made` follows them.
pub(crate) struct UndoGroupQueue {
    pub(crate) retry_gui_change: crate::data::GuiChangeQueue,
    inner: Mutex<UndoGroupInner>,
    gui_pushed: AtomicU64,
    gui_drained: AtomicU64,
    /// Sequence of the first pending or in-flight group boundary.
    gui_begin_gate: AtomicU64,
    /// Lowest `gui_seq` of a waiting `End`, or `u64::MAX` when none is waiting.
    /// `flush_gui_changes` signals `HostSignal::UNDO_GROUP` once `gui_drained`
    /// reaches this sequence.
    undo_wait_seq: AtomicU64,
}

impl UndoGroupQueue {
    pub(crate) fn new() -> Self {
        Self {
            retry_gui_change: crate::data::GuiChangeQueue::new(1),
            inner: Mutex::new(UndoGroupInner::default()),
            gui_pushed: AtomicU64::new(0),
            gui_drained: AtomicU64::new(0),
            gui_begin_gate: AtomicU64::new(u64::MAX),
            undo_wait_seq: AtomicU64::new(u64::MAX),
        }
    }

    /// Serialize editor producers with group edges. The audio consumer never locks.
    pub(crate) fn push_gui(&self, push: impl FnOnce() -> bool) -> bool {
        let _guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let pushed = push();
        if pushed {
            self.gui_pushed.fetch_add(1, Ordering::Release);
        }
        pushed
    }

    /// Wait-free audio/flush-thread gate: current sequence ceiling before any
    /// undelivered `Begin`.
    #[inline]
    pub(crate) fn begin_gate(&self) -> u64 {
        self.gui_begin_gate.load(Ordering::Acquire)
    }

    /// Wait-free audio/flush-thread check: number of `gui_changes` drained so far.
    #[inline]
    pub(crate) fn gui_drained(&self) -> u64 {
        self.gui_drained.load(Ordering::Acquire)
    }

    /// Record that one `GuiParamChange` was popped by `flush_gui_changes`.
    #[inline]
    pub(crate) fn note_gui_drained(&self) {
        self.gui_drained.fetch_add(1, Ordering::AcqRel);
    }

    /// Returns `true` if `gui_drained` has reached a waiting `End` boundary and
    /// the host main thread should be woken (`HostSignal::UNDO_GROUP`).
    #[inline]
    pub(crate) fn should_wake_main_thread(&self) -> bool {
        let wait = self.undo_wait_seq.load(Ordering::Acquire);
        wait != u64::MAX && self.gui_drained.load(Ordering::Acquire) >= wait
    }

    /// Returns `true` if there is any queued undo operation awaiting main-thread
    /// delivery.
    pub(crate) fn has_pending(&self) -> bool {
        !self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending
            .is_empty()
    }

    /// Record custom-state bytes written while an admitted edit group is open
    /// so the matching `End` retains them for `change_made`'s `state.save`.
    pub(crate) fn push_custom_state(&self, data: Vec<u8>, push: impl FnOnce(Vec<u8>)) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.open_admitted > 0 {
            inner.staged_custom_state = Some(data.clone());
        }
        push(data);
    }

    /// Enqueue an editor edit-group edge or undo/redo request. Returns `true`
    /// when the main thread should be woken.
    pub(crate) fn enqueue(
        &self,
        op: UndoOp,
        capture: impl FnOnce(Option<Vec<u8>>) -> UndoSnapshot,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let seq = self.gui_pushed.load(Ordering::Acquire);
        match op {
            UndoOp::Begin => {
                // Keep room for every already-admitted open Begin's End plus
                // both edges of this new pair.
                let needed = inner.pending.len() + inner.open_admitted + 2;
                if needed > MAX_PENDING_GROUPS * 2 {
                    inner.open_suppressed += 1;
                    return false;
                }
                inner.explicit_reporting = true;
                inner.open_admitted += 1;
                inner.pending.push_back(QueuedEdge {
                    op: UndoOp::Begin,
                    gui_seq: seq,
                    snapshot: None,
                });
                Self::refresh_begin_gate(&inner, &self.gui_begin_gate);
                true
            }
            UndoOp::End => {
                if inner.open_suppressed > 0 {
                    inner.open_suppressed -= 1;
                    return false;
                }
                if inner.open_admitted == 0 {
                    // Unmatched End with no open Begin: never emit a stray
                    // `change_made` to the host.
                    return false;
                }
                inner.open_admitted -= 1;
                let snapshot = if inner.open_admitted == 0 {
                    Some(capture(inner.staged_custom_state.take()))
                } else {
                    None
                };
                inner.pending.push_back(QueuedEdge {
                    op: UndoOp::End,
                    gui_seq: seq,
                    snapshot,
                });
                Self::refresh_begin_gate(&inner, &self.gui_begin_gate);
                true
            }
            UndoOp::RequestUndo | UndoOp::RequestRedo => {
                if !inner.explicit_reporting {
                    return false;
                }
                let needed = inner.pending.len() + inner.open_admitted + 1;
                if needed > MAX_PENDING_GROUPS * 2 {
                    return false;
                }
                inner.pending.push_back(QueuedEdge {
                    op,
                    gui_seq: seq,
                    snapshot: None,
                });
                true
            }
        }
    }

    fn refresh_begin_gate(inner: &UndoGroupInner, gate: &AtomicU64) {
        let ceiling = inner.delivering.or_else(|| {
            inner
                .pending
                .iter()
                .find(|item| matches!(item.op, UndoOp::Begin | UndoOp::End))
                .map(|item| item.gui_seq)
        });
        gate.store(ceiling.unwrap_or(u64::MAX), Ordering::Release);
    }

    /// `[main-thread]` Drain ready `Begin` / `End` / undo / redo operations into
    /// `clap.undo/4`. Returns `true` if `Begin` unblocked queued `gui_changes`
    /// and the caller should poke `params.request_flush`.
    ///
    /// # Safety
    /// `host` and `ext` (when non-null) must be live on the host main thread.
    pub(crate) unsafe fn drain_main_thread(
        &self,
        host: HostHandle<MainThread>,
        ext: *const clap_host_undo,
        save_override: &moose_core::thread::MainThreadCell<Option<UndoSnapshot>>,
        main: &moose_core::thread::MainThread,
    ) -> bool {
        struct OverrideGuard<'a> {
            slot: &'a moose_core::thread::MainThreadCell<Option<UndoSnapshot>>,
            main: &'a moose_core::thread::MainThread,
        }
        impl Drop for OverrideGuard<'_> {
            fn drop(&mut self) {
                *self.slot.borrow_mut(self.main) = None;
            }
        }

        let mut request_flush = false;
        loop {
            let next = {
                let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
                if inner.delivering.is_some() {
                    break;
                }
                let Some(front) = inner.pending.front() else {
                    self.undo_wait_seq.store(u64::MAX, Ordering::Release);
                    Self::refresh_begin_gate(&inner, &self.gui_begin_gate);
                    break;
                };
                let wait_seq = front.gui_seq;
                let drained = self.gui_drained.load(Ordering::Acquire);
                if drained < wait_seq {
                    // Publish the wait target, then re-check `gui_drained`
                    // so a concurrent flush cannot miss our wait store.
                    self.undo_wait_seq.store(wait_seq, Ordering::Release);
                    if self.gui_drained.load(Ordering::Acquire) < wait_seq {
                        request_flush = true;
                        break;
                    }
                }
                let op = front.op;
                inner.delivering = Some(wait_seq);
                match op {
                    UndoOp::Begin => {
                        let _ = inner.pending.pop_front();
                        inner.host_open_depth += 1;
                        Self::refresh_begin_gate(&inner, &self.gui_begin_gate);
                        Some((UndoOp::Begin, inner.host_open_depth == 1, None))
                    }
                    UndoOp::End => {
                        let edge = inner.pending.pop_front();
                        self.undo_wait_seq.store(u64::MAX, Ordering::Release);
                        let close_host = if inner.host_open_depth > 0 {
                            inner.host_open_depth -= 1;
                            inner.host_open_depth == 0
                        } else {
                            false
                        };
                        Some((UndoOp::End, close_host, edge.and_then(|item| item.snapshot)))
                    }
                    UndoOp::RequestUndo | UndoOp::RequestRedo => {
                        let _ = inner.pending.pop_front();
                        Some((op, true, None))
                    }
                }
            };
            if let Some((op, notify_host, snapshot)) = next
                && notify_host
            {
                match op {
                    UndoOp::Begin => unsafe { notify_edit_group(host, ext, EditGroup::Begin) },
                    UndoOp::End => {
                        let _guard = snapshot.map(|bytes| {
                            *save_override.borrow_mut(main) = Some(bytes);
                            OverrideGuard {
                                slot: save_override,
                                main,
                            }
                        });
                        unsafe { notify_edit_group(host, ext, EditGroup::End) };
                    }
                    UndoOp::RequestUndo => unsafe { notify_request_undo(host, ext) },
                    UndoOp::RequestRedo => unsafe { notify_request_redo(host, ext) },
                }
            }
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            inner.delivering = None;
            Self::refresh_begin_gate(&inner, &self.gui_begin_gate);
            request_flush |= self.gui_pushed.load(Ordering::Acquire) > self.gui_drained();
        }
        request_flush
    }

    /// `[main-thread]` Cancel any still-open host undo transaction on destroy so
    /// the host is never left with an unpaired `begin_change`.
    ///
    /// # Safety
    /// `host` and `ext` (when non-null) must be live on the host main thread.
    pub(crate) unsafe fn cancel_open_on_destroy(
        &self,
        host: HostHandle<MainThread>,
        ext: *const clap_host_undo,
    ) {
        let was_open = {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            inner.pending.clear();
            inner.open_admitted = 0;
            inner.open_suppressed = 0;
            self.gui_begin_gate.store(u64::MAX, Ordering::Release);
            self.undo_wait_seq.store(u64::MAX, Ordering::Release);
            let open = inner.host_open_depth > 0;
            inner.host_open_depth = 0;
            open
        };
        if was_open {
            let raw = host.as_raw();
            if !raw.is_null()
                && !ext.is_null()
                && let Some(cancel) = unsafe { (*ext).cancel_change }
            {
                unsafe { cancel(raw) };
            }
        }
    }
}

/// `[main-thread]` Map an editor edit group onto `clap.undo/4` `begin_change`
/// / `change_made`.
///
/// # Safety
/// `host` and `ext` (if non-null) must still be alive.
pub(crate) unsafe fn notify_edit_group(
    host: HostHandle<MainThread>,
    ext: *const clap_host_undo,
    edge: EditGroup,
) {
    let raw = host.as_raw();
    if raw.is_null() || ext.is_null() {
        return;
    }
    match edge {
        EditGroup::Begin => {
            if let Some(begin) = unsafe { (*ext).begin_change } {
                unsafe { begin(raw) };
            }
        }
        EditGroup::End => {
            if let Some(done) = unsafe { (*ext).change_made } {
                // `clap/ext/draft/undo.h` requires a non-null UTF-8 name string
                // for every `change_made` call (delta may be null when unused).
                unsafe { done(raw, c"Edit".as_ptr(), ptr::null(), 0, false) };
            }
        }
    }
}

/// `[main-thread]` Forward an undo request to `clap.undo/4`.
///
/// # Safety
/// `host` and `ext` (if non-null) must still be alive.
pub(crate) unsafe fn notify_request_undo(host: HostHandle<MainThread>, ext: *const clap_host_undo) {
    let raw = host.as_raw();
    if raw.is_null() || ext.is_null() {
        return;
    }
    if let Some(req) = unsafe { (*ext).request_undo } {
        unsafe { req(raw) };
    }
}

/// `[main-thread]` Forward a redo request to `clap.undo/4`.
///
/// # Safety
/// `host` and `ext` (if non-null) must still be alive.
pub(crate) unsafe fn notify_request_redo(host: HostHandle<MainThread>, ext: *const clap_host_undo) {
    let raw = host.as_raw();
    if raw.is_null() || ext.is_null() {
        return;
    }
    if let Some(req) = unsafe { (*ext).request_redo } {
        unsafe { req(raw) };
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    use clap_sys::host::clap_host;
    use clap_sys::version::CLAP_VERSION;
    use moose_core::buffer::AudioBuffer;
    use moose_core::config::AudioConfig;
    use moose_core::events::EventList;
    use moose_core::info::PluginInfo;
    use moose_core::meters::MeterStore;
    use moose_core::plugin::PluginRuntime;
    use moose_core::process::{ProcessContext, ProcessStatus};
    use moose_core::snapshot::SnapshotSlot;
    use moose_params::ParamInfo;
    use moose_test_fixtures::host_value_fixture::{TestParams, TestPlugin};

    use super::*;
    use crate::draft::{
        CLAP_EXT_PARAM_HOVERED, CLAP_EXT_PARAMS_ORIGIN, CLAP_EXT_UNDO, CLAP_EXT_UNDO_CONTEXT,
    };
    use crate::gui::clap_plugin_get_extension;
    use crate::host_context::editor_context;
    use crate::plugin::{clap_plugin_destroy, clap_plugin_init, clap_plugin_on_main_thread};
    use crate::{DescriptorHolder, create_plugin_instance};

    #[test]
    fn origin_matches_exposed_range_and_resolution() {
        let stepped = moose_params::ParamRange::Stepped {
            min: -2.0,
            max: 2.0,
            step: 1.0,
        };
        assert!(valid_origin(&stepped, -2.0));
        assert!(valid_origin(&stepped, 2.0));
        assert!(!valid_origin(&stepped, 0.5));
        assert!(!valid_origin(&stepped, 3.0));
        assert!(!valid_origin(&stepped, f64::NAN));
        let fractional = moose_params::ParamRange::Stepped {
            min: -2.0,
            max: 2.0,
            step: 0.5,
        };
        assert!(valid_origin(&fractional, 0.5));
    }

    struct OriginParams(TestParams);
    impl moose_params::__private::Sealed for OriginParams {}
    impl Params for OriginParams {
        fn param_infos(&self) -> Vec<ParamInfo> {
            self.0.param_infos()
        }
        fn count(&self) -> usize {
            self.0.count()
        }
        fn get_normalized(&self, id: u32) -> Option<f64> {
            self.0.get_normalized(id)
        }
        fn set_normalized(&self, id: u32, value: f64) {
            self.0.set_normalized(id, value);
        }
        fn get_plain(&self, id: u32) -> Option<f64> {
            self.0.get_plain(id)
        }
        fn set_plain(&self, id: u32, value: f64) {
            self.0.set_plain(id, value);
        }
        fn param_origin(&self, id: u32) -> Option<f64> {
            (id == self.0.param_infos()[0].id).then_some(0.25)
        }
        fn format_value(&self, id: u32, value: f64) -> Option<String> {
            self.0.format_value(id, value)
        }
        fn parse_value(&self, id: u32, text: &str) -> Option<f64> {
            self.0.parse_value(id, text)
        }
        fn snap_smoothers(&self) {
            self.0.snap_smoothers();
        }
        fn set_sample_rate(&self, sample_rate: f64) {
            self.0.set_sample_rate(sample_rate);
        }
        fn serialize_persist(&self) -> Vec<u8> {
            self.0.serialize_persist()
        }
        fn collect_values(&self) -> (Vec<u32>, Vec<f64>) {
            self.0.collect_values()
        }
        fn restore_values(&self, entries: &[(u32, f64)]) {
            self.0.restore_values(entries);
        }
    }

    struct DraftPlugin {
        inner: TestPlugin,
        params: Arc<OriginParams>,
    }
    impl PluginExport for DraftPlugin {
        type Params = OriginParams;
        fn create() -> Self {
            let inner = TestPlugin::create();
            let params = Arc::new(OriginParams(TestParams::default()));
            Self { inner, params }
        }
        fn params(&self) -> &OriginParams {
            &self.params
        }
        fn params_arc(&self) -> Arc<OriginParams> {
            Arc::clone(&self.params)
        }
        fn meter_store(&self) -> Arc<MeterStore> {
            self.inner.meter_store()
        }
        fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
            self.inner.snapshot_slot()
        }
    }
    impl PluginRuntime for DraftPlugin {
        type Sample = f32;
        fn reset(&mut self, _: &AudioConfig) {}
        fn info() -> PluginInfo {
            TestPlugin::info()
        }
        fn process(
            &mut self,
            _: &mut AudioBuffer,
            _: &EventList,
            _: &mut ProcessContext,
        ) -> ProcessStatus {
            ProcessStatus::Normal
        }
    }

    #[test]
    fn params_origin_param_hovered_and_undo_drafts_round_trip() {
        static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

        unsafe extern "C" fn on_hover(_: *const clap_host, id: clap_id) {
            let label = if id == CLAP_INVALID_ID {
                "hover:none".to_owned()
            } else {
                format!("hover:{id}")
            };
            EVENTS.lock().unwrap().push(label);
        }
        unsafe extern "C" fn on_begin(_: *const clap_host) {
            EVENTS.lock().unwrap().push("undo:begin".to_owned());
        }
        unsafe extern "C" fn on_made(
            _: *const clap_host,
            name: *const c_char,
            _: *const c_void,
            _: usize,
            _: bool,
        ) {
            assert!(!name.is_null(), "CLAP change_made requires a non-null name");
            let label = unsafe { CStr::from_ptr(name) }.to_string_lossy();
            EVENTS.lock().unwrap().push(format!("undo:made:{label}"));
        }
        unsafe extern "C" fn on_undo(_: *const clap_host) {
            EVENTS.lock().unwrap().push("undo:request_undo".to_owned());
        }
        unsafe extern "C" fn on_redo(_: *const clap_host) {
            EVENTS.lock().unwrap().push("undo:request_redo".to_owned());
        }

        unsafe extern "C" fn subscribe(_: *const clap_host, enabled: bool) {
            EVENTS.lock().unwrap().push(format!("subscribe:{enabled}"));
        }
        static HOVER_EXT: clap_host_param_hovered = clap_host_param_hovered {
            update: Some(on_hover),
        };
        static UNDO_EXT: clap_host_undo = clap_host_undo {
            begin_change: Some(on_begin),
            cancel_change: None,
            change_made: Some(on_made),
            request_undo: Some(on_undo),
            request_redo: Some(on_redo),
            set_wants_context_updates: Some(subscribe),
        };

        unsafe extern "C" fn get_ext(_: *const clap_host, id: *const c_char) -> *const c_void {
            let ext_id = unsafe { CStr::from_ptr(id) };
            if ext_id == CLAP_EXT_PARAM_HOVERED {
                (&raw const HOVER_EXT).cast()
            } else if ext_id == CLAP_EXT_UNDO {
                (&raw const UNDO_EXT).cast()
            } else {
                ptr::null()
            }
        }

        EVENTS.lock().unwrap().clear();
        unsafe {
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            host.get_extension = Some(get_ext);
            let desc = DescriptorHolder::new(&DraftPlugin::info());
            let plugin =
                create_plugin_instance::<DraftPlugin>(&raw const desc.descriptor, &raw const host);
            assert!(clap_plugin_init::<DraftPlugin>(plugin));

            // 1. clap.params-origin/1
            let origin_ext =
                clap_plugin_get_extension::<DraftPlugin>(plugin, CLAP_EXT_PARAMS_ORIGIN.as_ptr())
                    .cast::<clap_plugin_params_origin>();
            assert!(!origin_ext.is_null());
            let get_origin = (*origin_ext).get.unwrap();
            let first_id = data_from_plugin::<DraftPlugin>(plugin)
                .params_arc
                .param_infos()[0]
                .id;
            let mut val = 0.0;
            assert!(get_origin(plugin, first_id, &raw mut val));
            assert!((val - 0.25).abs() < 1e-12);
            assert!(!get_origin(plugin, 999_999, &raw mut val));
            assert!(!get_origin(plugin, first_id, ptr::null_mut()));

            // 2. clap.undo_context/4
            let undo_ctx_ext =
                clap_plugin_get_extension::<DraftPlugin>(plugin, CLAP_EXT_UNDO_CONTEXT.as_ptr())
                    .cast::<clap_plugin_undo_context>();
            assert!(!undo_ctx_ext.is_null());
            ((*undo_ctx_ext).set_can_undo.unwrap())(plugin, true);
            ((*undo_ctx_ext).set_can_redo.unwrap())(plugin, false);
            ((*undo_ctx_ext).set_undo_name.unwrap())(plugin, c"Edit Gain".as_ptr());
            ((*undo_ctx_ext).set_redo_name.unwrap())(plugin, c"Redo Gain".as_ptr());
            {
                let st = data_from_plugin::<DraftPlugin>(plugin)
                    .undo_context
                    .lock()
                    .unwrap();
                assert!(st.can_undo);
                assert!(!st.can_redo);
                assert_eq!(st.undo_name, "Edit Gain");
                assert_eq!(st.redo_name, "Redo Gain");
            }

            // 3. clap.param-hovered/1 & clap.undo/4 via PluginContext -> on_main_thread
            let ctx = editor_context::<DraftPlugin>(plugin);
            ctx.param_hovered(Some(first_id));
            clap_plugin_on_main_thread::<DraftPlugin>(plugin);
            // Rapid hover flood ending in `None` must coalesce and never drop `None`.
            for i in 0..64u32 {
                ctx.param_hovered(Some(i));
            }
            ctx.param_hovered(None);
            ctx.begin_group();
            ctx.end_group();
            ctx.request_undo();
            ctx.request_redo();
            clap_plugin_on_main_thread::<DraftPlugin>(plugin);

            assert_eq!(
                *EVENTS.lock().unwrap(),
                vec![
                    format!("hover:{first_id}"),
                    "hover:none".to_owned(),
                    "subscribe:true".to_owned(),
                    "undo:begin".to_owned(),
                    "undo:made:Edit".to_owned(),
                    "undo:request_undo".to_owned(),
                    "undo:request_redo".to_owned(),
                ]
            );

            clap_plugin_destroy::<DraftPlugin>(plugin);
            assert_eq!(EVENTS.lock().unwrap().last().unwrap(), "subscribe:false");
        }
    }

    mod ordered_undo {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use clap_sys::events::{
            CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_VALUE,
            clap_event_header, clap_output_events,
        };
        use clap_sys::ext::params::CLAP_EXT_PARAMS;
        use clap_sys::ext::params::clap_host_params;
        use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state};

        use crate::params_ext::params_flush;

        static TRACE: Mutex<Vec<String>> = Mutex::new(Vec::new());
        static UNDO_QUERIES: AtomicUsize = AtomicUsize::new(0);

        static INSTANCE: AtomicUsize = AtomicUsize::new(0);
        static REFUSE_END: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
        unsafe extern "C" fn on_mark_dirty(_: *const clap_host) {
            TRACE.lock().unwrap().push("state:dirty".to_owned());
        }
        unsafe extern "C" fn on_request_flush(_: *const clap_host) {}
        unsafe extern "C" fn on_begin(_: *const clap_host) {
            let out = clap_output_events {
                ctx: ptr::null_mut(),
                try_push: Some(push_event),
            };
            unsafe {
                params_flush::<DraftPlugin>(
                    INSTANCE.load(Ordering::Relaxed) as *const _,
                    ptr::null(),
                    &raw const out,
                );
            }
            TRACE.lock().unwrap().push("undo:begin".to_owned());
        }
        unsafe extern "C" fn on_cancel(_: *const clap_host) {
            TRACE.lock().unwrap().push("undo:cancel".to_owned());
        }
        unsafe extern "C" fn on_made(
            _: *const clap_host,
            name: *const c_char,
            _: *const c_void,
            _: usize,
            _: bool,
        ) {
            assert!(!name.is_null());
            let label = unsafe { CStr::from_ptr(name) }.to_string_lossy();
            TRACE.lock().unwrap().push(format!("undo:made:{label}"));
        }
        unsafe extern "C" fn push_event(
            _: *const clap_output_events,
            ev: *const clap_event_header,
        ) -> bool {
            let tag = match unsafe { (*ev).type_ } {
                CLAP_EVENT_PARAM_GESTURE_BEGIN => "param:begin",
                CLAP_EVENT_PARAM_VALUE => "param:value",
                CLAP_EVENT_PARAM_GESTURE_END => "param:end",
                _ => "param:other",
            };
            if tag == "param:end" {
                if REFUSE_END.swap(false, Ordering::Relaxed) {
                    return false;
                }
                unsafe {
                    clap_plugin_on_main_thread::<DraftPlugin>(
                        INSTANCE.load(Ordering::Relaxed) as *const _
                    );
                }
            }
            TRACE.lock().unwrap().push(tag.to_owned());
            true
        }

        static STATE_EXT: clap_host_state = clap_host_state {
            mark_dirty: Some(on_mark_dirty),
        };
        static PARAMS_EXT: clap_host_params = clap_host_params {
            rescan: None,
            clear: None,
            request_flush: Some(on_request_flush),
        };
        static UNDO_EXT: clap_host_undo = clap_host_undo {
            begin_change: Some(on_begin),
            cancel_change: Some(on_cancel),
            change_made: Some(on_made),
            request_undo: None,
            request_redo: None,
            set_wants_context_updates: None,
        };

        unsafe extern "C" fn get_ext(_: *const clap_host, id: *const c_char) -> *const c_void {
            let ext_id = unsafe { CStr::from_ptr(id) };
            if ext_id == CLAP_EXT_STATE {
                (&raw const STATE_EXT).cast()
            } else if ext_id == CLAP_EXT_PARAMS {
                (&raw const PARAMS_EXT).cast()
            } else if ext_id == CLAP_EXT_UNDO {
                UNDO_QUERIES.fetch_add(1, Ordering::Relaxed);
                (&raw const UNDO_EXT).cast()
            } else {
                ptr::null()
            }
        }

        #[test]
        fn undo_ext_is_lazy_for_dirty_only_and_keeps_group_ordered_around_gestures() {
            TRACE.lock().unwrap().clear();
            UNDO_QUERIES.store(0, Ordering::Relaxed);
            unsafe {
                let mut host: clap_host = std::mem::zeroed();
                host.clap_version = CLAP_VERSION;
                host.get_extension = Some(get_ext);
                let desc = DescriptorHolder::new(&DraftPlugin::info());
                let plugin = create_plugin_instance::<DraftPlugin>(
                    &raw const desc.descriptor,
                    &raw const host,
                );
                assert!(clap_plugin_init::<DraftPlugin>(plugin));

                INSTANCE.store(plugin as usize, Ordering::Relaxed);
                // Dirty-only usage must NOT query `clap.undo/4`.
                let ctx = editor_context::<DraftPlugin>(plugin);
                ctx.request_undo();
                ctx.request_redo();
                ctx.mark_dirty();
                clap_plugin_on_main_thread::<DraftPlugin>(plugin);
                assert_eq!(UNDO_QUERIES.load(Ordering::Relaxed), 0);
                assert_eq!(*TRACE.lock().unwrap(), vec!["state:dirty".to_owned()]);
                TRACE.lock().unwrap().clear();

                // Grouped edit: even if `params_flush` runs BEFORE `on_main_thread`,
                // the host must observe `undo:begin` -> param events -> `undo:made:Edit`.
                let first_id = data_from_plugin::<DraftPlugin>(plugin)
                    .params_arc
                    .param_infos()[0]
                    .id;
                let out_events = clap_output_events {
                    ctx: ptr::null_mut(),
                    try_push: Some(push_event),
                };
                ctx.begin_group();
                ctx.begin_edit(first_id);
                ctx.set_param(first_id, 0.75);
                ctx.end_edit(first_id);
                ctx.end_group();

                // Flush before on_main_thread: gated until `undo:begin` is delivered.
                params_flush::<DraftPlugin>(plugin, ptr::null(), &raw const out_events);
                assert!(TRACE.lock().unwrap().is_empty());

                // First main-thread tick delivers `undo:begin` and holds `undo:made`
                // until enclosed gestures flush.
                clap_plugin_on_main_thread::<DraftPlugin>(plugin);
                assert_eq!(UNDO_QUERIES.load(Ordering::Relaxed), 1);
                assert_eq!(*TRACE.lock().unwrap(), vec!["undo:begin".to_owned()]);

                // A refused final gesture holds the End boundary until its retry succeeds.
                params_flush::<DraftPlugin>(plugin, ptr::null(), &raw const out_events);
                clap_plugin_on_main_thread::<DraftPlugin>(plugin);
                assert_eq!(
                    *TRACE.lock().unwrap(),
                    ["undo:begin", "param:begin", "param:value"]
                );

                params_flush::<DraftPlugin>(plugin, ptr::null(), &raw const out_events);
                clap_plugin_on_main_thread::<DraftPlugin>(plugin);
                assert_eq!(
                    *TRACE.lock().unwrap(),
                    vec![
                        "undo:begin".to_owned(),
                        "param:begin".to_owned(),
                        "param:value".to_owned(),
                        "param:end".to_owned(),
                        "undo:made:Edit".to_owned(),
                    ]
                );

                // Destroy with an unclosed group must emit `cancel_change`.
                TRACE.lock().unwrap().clear();
                ctx.begin_group();
                clap_plugin_on_main_thread::<DraftPlugin>(plugin);
                clap_plugin_destroy::<DraftPlugin>(plugin);
                assert_eq!(
                    *TRACE.lock().unwrap(),
                    vec!["undo:begin".to_owned(), "undo:cancel".to_owned()]
                );
            }
        }
    }

    #[test]
    fn deferred_undo_completions_preserve_each_groups_custom_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use clap_sys::ext::state::CLAP_EXT_STATE;
        use clap_sys::plugin::clap_plugin_descriptor;
        use clap_sys::stream::clap_ostream;

        static PLUGIN_PTR: AtomicUsize = AtomicUsize::new(0);
        static SAVED_BLOBS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

        unsafe extern "C" fn accept(
            _: *const clap_sys::events::clap_output_events,
            _: *const clap_sys::events::clap_event_header,
        ) -> bool {
            true
        }
        unsafe extern "C" fn write_vec(
            stream: *const clap_ostream,
            buf: *const c_void,
            size: u64,
        ) -> i64 {
            let out = unsafe { &mut *(*stream).ctx.cast::<Vec<u8>>() };
            let len = usize::try_from(size).unwrap();
            let slice = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) };
            out.extend_from_slice(slice);
            i64::try_from(size).unwrap()
        }
        unsafe extern "C" fn on_begin(_: *const clap_host) {}
        unsafe extern "C" fn on_made(
            _: *const clap_host,
            _: *const c_char,
            _: *const c_void,
            _: usize,
            _: bool,
        ) {
            let plugin = PLUGIN_PTR.load(Ordering::Acquire) as *const clap_plugin;
            let state_ext = unsafe {
                clap_plugin_get_extension::<DraftPlugin>(plugin, CLAP_EXT_STATE.as_ptr())
                    .cast::<clap_sys::ext::state::clap_plugin_state>()
            };
            let mut bytes = Vec::<u8>::new();
            let stream = clap_ostream {
                ctx: (&raw mut bytes).cast(),
                write: Some(write_vec),
            };
            assert!(unsafe { ((*state_ext).save.unwrap())(plugin, &raw const stream) });
            SAVED_BLOBS.lock().unwrap().push(bytes);
        }
        static UNDO_EXT: clap_host_undo = clap_host_undo {
            begin_change: Some(on_begin),
            cancel_change: None,
            change_made: Some(on_made),
            request_undo: None,
            request_redo: None,
            set_wants_context_updates: None,
        };
        unsafe extern "C" fn host_get_ext(
            _: *const clap_host,
            ext_id: *const c_char,
        ) -> *const c_void {
            if unsafe { CStr::from_ptr(ext_id) } == CLAP_EXT_UNDO {
                (&raw const UNDO_EXT).cast()
            } else {
                ptr::null()
            }
        }
        unsafe extern "C" fn nop_host(_: *const clap_host) {}

        SAVED_BLOBS.lock().unwrap().clear();
        let desc = clap_plugin_descriptor {
            clap_version: CLAP_VERSION,
            id: c"com.matari.draftstate".as_ptr(),
            name: c"DraftState".as_ptr(),
            vendor: c"Matari".as_ptr(),
            url: c"".as_ptr(),
            manual_url: c"".as_ptr(),
            support_url: c"".as_ptr(),
            version: c"1.0.0".as_ptr(),
            description: c"".as_ptr(),
            features: [ptr::null()].as_ptr(),
        };
        let host = clap_host {
            clap_version: CLAP_VERSION,
            host_data: ptr::null_mut(),
            name: c"TestHost".as_ptr(),
            vendor: c"".as_ptr(),
            url: c"".as_ptr(),
            version: c"".as_ptr(),
            get_extension: Some(host_get_ext),
            request_restart: Some(nop_host),
            request_process: Some(nop_host),
            request_callback: Some(nop_host),
        };

        unsafe {
            let plugin = create_plugin_instance::<DraftPlugin>(&raw const desc, &raw const host);
            assert!(clap_plugin_init::<DraftPlugin>(plugin));
            PLUGIN_PTR.store(plugin as usize, Ordering::Release);

            let ctx = editor_context::<DraftPlugin>(plugin);
            let data = data_from_plugin::<DraftPlugin>(plugin);
            let id = data.params_arc.param_infos()[0].id;
            ctx.begin_group();
            ctx.set_param(id, 0.25);
            data.params_arc.0.completion.complete(1);
            let persist_a = data.params_arc.serialize_persist();
            let a = data.params_arc.get_plain(id).unwrap();
            ctx.set_state(b"CUSTOM_GROUP_A".to_vec());
            ctx.end_group();
            ctx.begin_group();
            ctx.set_state(b"CUSTOM_GROUP_B".to_vec());
            ctx.set_param(id, 0.75);
            data.params_arc.0.completion.complete(2);
            let persist_b = data.params_arc.serialize_persist();
            let b = data.params_arc.get_plain(id).unwrap();
            ctx.end_group();
            ctx.begin_group();
            ctx.end_group();
            ctx.set_state(b"LATER".to_vec());

            let out = clap_sys::events::clap_output_events {
                ctx: ptr::null_mut(),
                try_push: Some(accept),
            };
            clap_plugin_on_main_thread::<DraftPlugin>(plugin);
            assert!(SAVED_BLOBS.lock().unwrap().is_empty());
            for _ in 0..2 {
                crate::params_ext::params_flush::<DraftPlugin>(plugin, ptr::null(), &raw const out);
                clap_plugin_on_main_thread::<DraftPlugin>(plugin);
            }
            let blobs = SAVED_BLOBS.lock().unwrap().clone();
            assert_eq!(blobs.len(), 3);
            assert_ne!(a, b);
            assert_ne!(persist_a, persist_b);
            for (blob, (value, extra, persist)) in blobs.iter().zip([
                (a, b"CUSTOM_GROUP_A", &persist_a),
                (b, b"CUSTOM_GROUP_B", &persist_b),
                (b, b"CUSTOM_GROUP_B", &persist_b),
            ]) {
                let state =
                    moose_core::state::deserialize_state(blob, data.plugin_id_hash).unwrap();
                assert_eq!(state.extra.as_deref(), Some(extra.as_slice()));
                assert!(state.params.contains(&(id, value)));
                assert_eq!(&state.persist, persist);
            }

            PLUGIN_PTR.store(0, Ordering::Release);
            clap_plugin_destroy::<DraftPlugin>(plugin);
        }
    }
}
