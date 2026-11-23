//! Editor callbacks and [`Vst3Host`].

use crate::ffi;
use crate::instance::{StateLoadQueue, Vst3Instance};
use crate::lifecycle::vst3_midi_source;
use crate::midi_proxy::{MIDI_PROXY_PER_CHANNEL, allocated_midi_proxy_id};

use moose_core::TransportSlot;
use moose_core::editor::{
    EditGroup, Editor, EditorBridge, HostContext, PluginContext, RawWindowHandle,
    clamp_logical_size, fit_logical_size,
};
use moose_core::events::TransportInfo;
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostMessage, HostQueue, HostSignal};
use moose_core::main_thread::{HostNotice, MenuTarget};
use moose_core::meters::MeterStore;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::wrapper::{custom_state_view, run_extern_callback_with};
use moose_params::Params;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// HostContext
// ---------------------------------------------------------------------------

/// Opaque `Vst3Instance<P>` handle held by [`Vst3Host`].
#[derive(Clone, Copy)]
struct InstanceCtx(*mut std::ffi::c_void);

// SAFETY: `Vst3Host` only passes this pointer into `ffi::moose_vst3_*`
// helpers, which look up the live `Host` in the synchronized COM registry
// (no-op once unregistered on teardown) rather than dereferencing `ctx`.
unsafe impl Send for InstanceCtx {}
unsafe impl Sync for InstanceCtx {}

impl InstanceCtx {
    fn as_ptr(self) -> *mut std::ffi::c_void {
        self.0
    }
}

/// One VST3 instance seen from its editor.
pub(crate) struct Vst3Host<P: PluginExport> {
    ctx: InstanceCtx,
    params: Arc<P::Params>,
    meter_store: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
    pending_state: Arc<StateLoadQueue>,
    transport: Arc<TransportSlot>,
    queue: Arc<HostQueue>,
    _plugin: PhantomData<fn() -> P>,
}

impl<P: PluginExport> Vst3Host<P> {
    /// # Safety
    /// `ctx` is a live `Vst3Instance<P>` pointer.
    unsafe fn new(ctx: *mut std::ffi::c_void) -> Self {
        let inst = unsafe { &*ctx.cast::<Vst3Instance<P>>() };
        Self {
            ctx: InstanceCtx(ctx),
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

impl<P: PluginExport> EditorBridge for Vst3Host<P> {
    fn begin_edit(&self, id: u32) {
        ffi::moose_vst3_begin_edit(self.ctx.as_ptr(), id);
    }

    fn set_param(&self, id: u32, normalized: f64) {
        // Single trait dispatch: the post-clamp normalized value is what
        // the host expects for `IComponentHandler::performEdit`.
        let norm = self
            .params
            .set_normalized_returning_normalized(id, normalized);
        if self.params.parameter_affects_presentation(id) {
            self.queue.signal(HostSignal::PRESENTATION);
        }
        ffi::moose_vst3_perform_edit(self.ctx.as_ptr(), id, norm);
    }

    fn end_edit(&self, id: u32) {
        ffi::moose_vst3_end_edit(self.ctx.as_ptr(), id);
    }

    fn request_resize(&self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 {
            return false;
        }
        let Some((pw, ph)) = self.queue.with_open(|| {
            // SAFETY: `with_open` keeps `Vst3Instance<P>` alive against
            // `cb_destroy` while we read its atomic scale.
            let inst = unsafe { &*self.ctx.as_ptr().cast::<Vst3Instance<P>>() };
            logical_to_phys(w, h, inst.host_scale())
        }) else {
            return false;
        };
        ffi::moose_vst3_request_resize(self.ctx.as_ptr(), pw, ph) != 0
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
        // Always enqueue, never apply here: this can run on the editor's own
        // thread, and the plugin cell is entered only from the audio thread
        // (active) or the host main thread (inactive).
        self.pending_state.force_push(state::DeserializedState {
            extra: Some(data),
            ..Default::default()
        });
    }

    fn transport(&self) -> Option<TransportInfo> {
        self.transport.read()
    }
}

impl<P: PluginExport> HostContext for Vst3Host<P> {
    fn mark_dirty(&self) {
        self.queue.signal(HostSignal::MARK_DIRTY);
        ffi::moose_vst3_flush_host_queue(self.ctx.as_ptr());
    }

    fn group_edits(&self, edge: EditGroup) {
        ffi::moose_vst3_group_edit(self.ctx.as_ptr(), edge);
    }

    fn request_restart(&self) {
        self.queue.signal(HostSignal::RESTART);
        ffi::moose_vst3_flush_host_queue(self.ctx.as_ptr());
    }

    fn context_menu(&self, target: MenuTarget, x: i32, y: i32) -> bool {
        let ctx = self.ctx.as_ptr();
        if !ffi::moose_vst3_has_context_menu(ctx) {
            return false;
        }
        let ok = self
            .queue
            .post(HostMessage::ContextMenu { target, x, y })
            .is_ok();
        if ok {
            ffi::moose_vst3_flush_host_queue(ctx);
        }
        ok
    }

    fn notify(&self, notice: HostNotice) {
        self.queue.signal(notice);
        ffi::moose_vst3_flush_host_queue(self.ctx.as_ptr());
    }

    fn window_rescaled(&self, scale: f64) {
        if scale.is_finite() && scale > 0.0 {
            self.queue.with_open(|| {
                // SAFETY: `with_open` keeps `Vst3Instance<P>` alive against `cb_destroy`.
                let inst = unsafe { &*self.ctx.as_ptr().cast::<Vst3Instance<P>>() };
                inst.window_scale.store(scale.to_bits(), Ordering::Relaxed);
            });
        }
    }
}

/// Build the [`PluginContext`] handed to the instance's editor.
///
/// # Safety
/// `ctx` is a live `Vst3Instance<P>` pointer.
pub(crate) unsafe fn editor_context<P: PluginExport>(ctx: *mut std::ffi::c_void) -> PluginContext {
    let inst = unsafe { &*ctx.cast::<Vst3Instance<P>>() };
    let host: Arc<dyn HostContext> = Arc::new(unsafe { Vst3Host::<P>::new(ctx) });
    PluginContext::from_host(host, Arc::clone(&inst.params_arc) as Arc<dyn Params>)
        .with_tasks(inst.task_spawner.clone())
}

/// Compute the physical `ViewRect` dimensions for `editor` on the current platform.
fn editor_phys_size<P: PluginExport>(inst: &Vst3Instance<P>, editor: &dyn Editor) -> (u32, u32) {
    let (ew, eh) = editor.size();
    #[cfg(target_os = "macos")]
    {
        let _ = inst;
        (ew, eh)
    }
    #[cfg(not(target_os = "macos"))]
    {
        logical_to_phys(ew, eh, inst.host_scale())
    }
}

// ---------------------------------------------------------------------------
// GUI callbacks
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn cb_gui_has_editor<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> i32 {
    // The editor builder is author code; firewall its lazy construction so
    // a panic there can't unwind across the C ABI (0 = "no editor").
    run_extern_callback_with::<P, i32>("vst3", "gui_has_editor", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut gui = inst.gui.enter();
        if gui.editor.is_none() {
            // Built from the lock-free param store the wrapper already
            // holds outside the plugin, so opening the GUI never
            // stalls the audio thread.
            gui.editor =
                (inst.editor_builder)(inst.params_arc.clone()).filter(|editor| editor.has_ui());
            // Replay a content scale the host reported before the editor
            // existed (a valid VST3 ordering - `setContentScaleFactor`
            // can precede the editor object). Only a scale the host really
            // sent: replaying a default would pin the editor to it instead
            // of the OS scale. macOS never stores one (AppKit drives Retina).
            if let (Some(scale), Some(editor)) = (inst.reported_host_scale(), gui.editor.as_mut()) {
                editor.set_scale_factor(scale);
            }
            if let Some(ref editor) = gui.editor {
                let (pw, ph) = editor_phys_size(inst, editor.as_ref());
                inst.store_phys_size(pw, ph);
            }
        }
        i32::from(gui.editor.is_some())
    })
}

pub(crate) unsafe extern "C" fn cb_gui_get_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    // `Editor::size` is author code; firewall it so a panic can't unwind
    // across the C ABI.
    run_extern_callback_with::<P, ()>("vst3", "gui_get_size", (), || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        // A host can query the size synchronously while an outer GUI callback
        // holds the cell (mid-resize or mid-open). Report the size the plugin
        // just requested through `request_resize` (still pending), falling back
        // to the last known physical size so `IPlugView::getSize` never returns
        // a zeroed 0x0 ViewRect - never re-enter the cell.
        let Some(mut gui) = inst.gui.try_enter() else {
            let mut packed = inst.pending_resize.load(Ordering::Relaxed);
            if packed == 0 {
                packed = inst.last_phys_size.load(Ordering::Relaxed);
            }
            if packed != 0 {
                #[allow(clippy::cast_possible_truncation)]
                {
                    *w = (packed >> 32) as u32;
                    *h = packed as u32;
                }
            }
            return;
        };
        // Apply a resize the plugin requested re-entrantly (stashed by
        // `cb_gui_set_size` because the cell was busy) before reporting.
        let packed = inst.pending_resize.swap(0, Ordering::Relaxed);
        if let Some(editor) = gui.editor.as_deref() {
            inst.note_window_scale(editor);
        }
        if packed != 0
            && let Some(editor) = gui.editor.as_mut()
        {
            #[allow(clippy::cast_possible_truncation)]
            let (pw, ph) = ((packed >> 32) as u32, packed as u32);
            apply_physical_resize(editor.as_mut(), pw, ph, inst.host_scale());
        }
        if let Some(ref editor) = gui.editor {
            let (pw, ph) = editor_phys_size(inst, editor.as_ref());
            *w = pw;
            *h = ph;
            inst.store_phys_size(pw, ph);
        }
    });
}

/// `IParameterFinder::findParameter`: hit-test the editor at host view
/// coordinates `(x, y)` (physical pixels on Windows/Linux, logical points on
/// macOS) and write the matching parameter ID into `out_id`.
pub(crate) unsafe extern "C" fn cb_gui_param_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    x: i32,
    y: i32,
    out_id: *mut u32,
) -> i32 {
    run_extern_callback_with::<P, i32>("vst3", "gui_param_at", 0, || unsafe {
        let Some(inst) = ctx.cast::<Vst3Instance<P>>().as_ref() else {
            return 0;
        };
        let Some(out) = out_id.as_mut() else {
            return 0;
        };
        let Some(gui) = inst.gui.try_enter() else {
            return 0;
        };
        let Some(editor) = gui.editor.as_deref() else {
            return 0;
        };
        #[cfg(target_os = "macos")]
        let (lx, ly) = (x, y);
        #[cfg(not(target_os = "macos"))]
        let (lx, ly) = {
            inst.note_window_scale(editor);
            let scale = inst.host_scale();
            #[allow(clippy::cast_possible_truncation)]
            (
                (f64::from(x) / scale).round() as i32,
                (f64::from(y) / scale).round() as i32,
            )
        };
        if let Some(id) = editor.param_at(lx, ly) {
            *out = id;
            1
        } else {
            0
        }
    })
}

pub(crate) unsafe extern "C" fn cb_gui_set_content_scale<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    scale: f64,
) {
    // `Editor::set_scale_factor` is author code; firewall it.
    run_extern_callback_with::<P, ()>("vst3", "gui_set_content_scale", (), || unsafe {
        // macOS: `ViewRect` is in logical points and AppKit applies the
        // backing scale; a stored scale would only mis-convert host sizes.
        if cfg!(target_os = "macos") || ctx.is_null() || !scale.is_finite() || scale <= 0.0 {
            return;
        }
        // Clamp to the same range the GUI cluster's `EditorScale`
        // cell uses. A buggy host passing `f64::MAX` would otherwise
        // propagate to the editor and overflow when the editor
        // multiplies its logical size to physical pixels.
        let scale = scale.clamp(0.25, 8.0);
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let changed = inst.reported_host_scale() != Some(scale);
        inst.set_host_scale(scale);
        // `try_enter`, not `enter`: a host can deliver `setContentScaleFactor`
        // synchronously while an outer GUI callback holds the cell - on
        // Windows, `editor.open` (held across `cb_gui_open`) pumps messages and
        // the host may re-enter here. Re-entering with `enter` would hand out a
        // second aliasing `&mut` in release. When busy, skip the editor call:
        // `host_scale` is already persisted above, and `cb_gui_open` re-syncs
        // the editor's scale from it once `open` returns.
        let Some(mut gui) = inst.gui.try_enter() else {
            return;
        };
        let Some(editor) = gui.editor.as_mut() else {
            return;
        };
        editor.set_scale_factor(scale);
        // The view's physical size follows the new scale; tell the host
        // (`IPlugFrame::resizeView`) so its frame matches the child. Not a
        // no-op re-send: skipped when the scale didn't change.
        let (lw, lh) = editor.size();
        let (pw, ph) = logical_to_phys(lw, lh, scale);
        inst.store_phys_size(pw, ph);
        drop(gui);
        if changed && lw > 0 && lh > 0 {
            ffi::moose_vst3_request_resize(ctx, pw, ph);
        }
    });
}

/// `IPlugView::canResize` callback. Returns 1 / 0 mapping to
/// `kResultOk` / `kResultFalse` on the shim side.
pub(crate) unsafe extern "C" fn cb_gui_can_resize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> i32 {
    // `Editor::can_resize` is author code; firewall it (0 = "not resizable").
    run_extern_callback_with::<P, i32>("vst3", "gui_can_resize", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        i32::from(
            inst.gui
                .enter()
                .editor
                .as_ref()
                .is_some_and(|e| e.can_resize()),
        )
    })
}

/// `IPlugView::checkSizeConstraint` callback. Clamps the
/// requested physical width / height in place against the
/// editor's `min_size` / `max_size` / `aspect_ratio`. For
/// fixed-size editors snaps to the editor's current size (JUCE's
/// Ableton-Live workaround pattern).
pub(crate) unsafe extern "C" fn cb_gui_check_size_constraint<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    // `Editor::can_resize` / `size` are author code; firewall them.
    run_extern_callback_with::<P, ()>("vst3", "gui_check_size_constraint", (), || unsafe {
        if ctx.is_null() || w.is_null() || h.is_null() {
            return;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        // `try_enter`, not `enter`: a host answering a plugin `request_resize`
        // calls `resizeView` -> `checkSizeConstraint` (then `onSize`)
        // synchronously on this thread, re-entering the cell an outer editor
        // callback still holds. When busy, leave the host's requested `*w`/`*h`
        // unchanged (skip the constraint pass this frame) rather than aliasing -
        // matching the `cb_gui_set_size` / `cb_gui_get_size` siblings.
        let Some(gui) = inst.gui.try_enter() else {
            return;
        };
        let Some(ref editor) = gui.editor else {
            return;
        };
        inst.note_window_scale(editor.as_ref());
        let host_scale = inst.host_scale();
        if editor.can_resize() {
            // Physical -> logical, fit, logical -> physical. Fit the largest
            // on-ratio box *inside* the requested cursor box (never larger on
            // either axis). VST3 hosts drive the drag from the raw cursor and
            // re-assert it every frame, so any size we return that exceeds the
            // cursor (a single-edge "grow the other axis") is honoured for one
            // frame then bounced - the window judders. A size <= the cursor
            // is a fixed point the host converges on.
            let (lw, lh) = phys_to_logical(*w, *h, host_scale);
            let (fw, fh) = fit_logical_size(lw, lh, editor.as_ref());
            let (pw, ph) = logical_to_phys(fw, fh, host_scale);
            *w = pw;
            *h = ph;
        } else {
            // Snap to current size; host-side Live quirk handled
            // identically by JUCE.
            let (cw, ch) = editor.size();
            let (pw, ph) = logical_to_phys(cw, ch, host_scale);
            *w = pw;
            *h = ph;
        }
    });
}

/// `IPlugView::onSize` callback. Host committed a new size; delegate
/// to `Editor::set_size` after scaling physical -> logical. The editor
/// *fills* the committed window (min/max clamp only) rather than
/// re-fitting onto the aspect ratio - that shaping happened earlier in
/// `checkSizeConstraint`, the host's drag-negotiation point, and
/// flooring it again here would leave a 1px letterbox line at the
/// bottom. `onSize` must not request a resize: VST3 forbids
/// `IPlugFrame::resizeView` from inside `onSize`, and a reentrant call
/// judders the drag.
pub(crate) unsafe extern "C" fn cb_gui_set_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: u32,
    h: u32,
) {
    // `Editor::set_size` / `can_resize` are author code; firewall them.
    run_extern_callback_with::<P, ()>("vst3", "gui_set_size", (), || unsafe {
        if ctx.is_null() || w == 0 || h == 0 {
            return;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        // A host answering a plugin `request_resize` (resizeView) synchronously
        // lands `onSize` here while an outer GUI callback still holds the cell.
        // `try_enter` avoids the same-thread aliasing re-entry: apply when free,
        // else stash for `cb_gui_get_size` to apply on the next size query.
        match inst.gui.try_enter() {
            Some(mut gui) => {
                // Clear any stale deferred resize so a subsequent `getSize`
                // cannot overwrite this fresher host commit.
                inst.pending_resize.store(0, Ordering::Relaxed);
                if let Some(editor) = gui.editor.as_mut() {
                    inst.note_window_scale(editor.as_ref());
                    apply_physical_resize(editor.as_mut(), w, h, inst.host_scale());
                    let (pw, ph) = editor_phys_size(inst, editor.as_ref());
                    inst.store_phys_size(pw, ph);
                }
            }
            None => {
                inst.pending_resize
                    .store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
            }
        }
    });
}

/// `IMidiMapping::getMidiControllerAssignment` callback. Resolves the
/// host's controller query to a bound parameter or the instance's
/// collision-free hidden proxy ID.
//
// `controller as u8` is guarded by the `0..=127` match arm; `channel`
// goes through `try_from` so a negative never wraps.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) unsafe extern "C" fn cb_midi_mapping_get_param_id<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    bus_index: i32,
    channel: i16,
    controller: i16,
    out_param_id: *mut u32,
) -> i32 {
    if ctx.is_null() {
        return 0;
    }
    let inst = unsafe { &*ctx.cast::<Vst3Instance<P>>() };
    let Some(source) = vst3_midi_source(controller) else {
        return 0; // kResultFalse
    };
    let Ok(channel) = u8::try_from(channel) else {
        return 0;
    };
    // Returns a hit-flag (1/0); the shim maps it to kResultOk /
    // kResultFalse for the VST3 boundary. Explicit bindings win on
    // every bus - a bound parameter is one value, so per-port
    // separation doesn't apply to it. Everything unbound falls
    // through to the hidden proxy bank for the queried bus, keeping
    // controllers attributed per port (program change excepted -
    // not proxied).
    if let Some(id) = moose_params::map_source_to_param(&inst.param_infos, channel, source) {
        unsafe { out_param_id.write(id) };
        return 1;
    }
    let Ok(port) = u8::try_from(bus_index) else {
        return 0;
    };
    if P::info().accepts_midi_in
        && port < P::info().midi_input_ports
        && channel < 16
        && let Ok(controller) = u32::try_from(controller)
        && controller < MIDI_PROXY_PER_CHANNEL
        && let Some(id) = allocated_midi_proxy_id(&inst.midi_proxy_ids, port, channel, controller)
    {
        unsafe { out_param_id.write(id) };
        return 1;
    }
    0
}

/// Convert physical pixels (what the VST3 host speaks) to logical
/// points (what `Editor` works in). Identity when `host_scale` is
/// 1.0 or invalid.
pub(crate) fn phys_to_logical(pw: u32, ph: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (pw, ph);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lw = (f64::from(pw) / host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lh = (f64::from(ph) / host_scale).round() as u32;
    (lw.max(1), lh.max(1))
}

/// Inverse of `phys_to_logical`.
pub(crate) fn logical_to_phys(lw: u32, lh: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (lw, lh);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pw = (f64::from(lw) * host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ph = (f64::from(lh) * host_scale).round() as u32;
    (pw.max(1), ph.max(1))
}

/// Apply a host-supplied physical `(w, h)` to a resizable editor: convert to
/// logical points, clamp to the editor's min/max, and `set_size`. Shared by
/// `cb_gui_set_size` (the host's `onSize`) and the deferred-resize drain in
/// `cb_gui_get_size`.
pub(crate) fn apply_physical_resize(editor: &mut dyn Editor, w: u32, h: u32, host_scale: f64) {
    if editor.can_resize() {
        let (lw, lh) = phys_to_logical(w, h, host_scale);
        let (cw, ch) = clamp_logical_size(lw, lh, editor);
        editor.set_size(cw, ch);
    }
}

/// `Editor::is_open` after `open`; 0 when there is none or it panicked.
pub(crate) unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) -> i32 {
    #[cfg(target_os = "macos")]
    let handle = RawWindowHandle::AppKit(parent);
    #[cfg(target_os = "windows")]
    let handle = RawWindowHandle::Win32(parent);
    #[cfg(target_os = "linux")]
    let handle = RawWindowHandle::X11(parent as u64);
    // SAFETY: forwarded from the shim's own contract for this callback.
    unsafe { gui_open_with::<P>(ctx, handle) }
}

/// Open the editor on `handle`; `Editor::is_open` after `open`, 0 when
/// there is none or it panicked.
///
/// # Safety
/// `ctx` is a live `Vst3Instance<P>`; host UI thread.
pub(crate) unsafe fn gui_open_with<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    handle: RawWindowHandle,
) -> i32 {
    // `editor.open` runs author GUI-construction code that can panic;
    // firewall it so the panic can't unwind across the C ABI.
    run_extern_callback_with::<P, i32>("vst3", "gui_open", 0, || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut gui = inst.gui.enter();
        if let Some(ref mut editor) = gui.editor {
            // Re-sync the editor's content scale to the host's reported
            // value right before opening. Only replay a scale the host
            // actually sent (`reported_host_scale`): passing the default
            // 1.0 fallback from `host_scale()` would pin the editor to 1x
            // instead of letting it follow the OS / monitor DPI scale.
            if let Some(scale) = inst.reported_host_scale() {
                editor.set_scale_factor(scale);
            }
            let (pw, ph) = editor_phys_size(inst, editor.as_ref());
            inst.store_phys_size(pw, ph);
            let context = editor_context::<P>(ctx);
            editor.open(handle, context);
            // Re-sync after `open`: on Windows `open` pumps messages, so a
            // re-entrant `setContentScaleFactor` may have updated
            // `host_scale` while `cb_gui_set_content_scale` had to skip
            // the editor (the cell was held here).
            if let Some(scale) = inst.reported_host_scale() {
                editor.set_scale_factor(scale);
            }
            inst.note_window_scale(editor.as_ref());
            let (pw, ph) = editor_phys_size(inst, editor.as_ref());
            inst.store_phys_size(pw, ph);
            i32::from(editor.is_open())
        } else {
            0
        }
    })
}

pub(crate) unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    // `editor.close` runs author teardown code that can panic; firewall it.
    run_extern_callback_with::<P, ()>("vst3", "gui_close", (), || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(ref mut editor) = inst.gui.enter().editor {
            editor.close();
        }
    });
}

/// `Editor::supports_wayland_embedding`; 0 without an editor.
pub(crate) unsafe extern "C" fn cb_gui_supports_wayland<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> i32 {
    run_extern_callback_with::<P, i32>("vst3", "gui_supports_wayland", 0, || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let gui = inst.gui.enter();
        i32::from(
            gui.editor
                .as_ref()
                .is_some_and(|e| e.supports_wayland_embedding()),
        )
    })
}

/// Open the editor on the host's Wayland `display` as a subsurface of
/// `surface`. Same result as `cb_gui_open`.
pub(crate) unsafe extern "C" fn cb_gui_open_wayland<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    display: *mut std::ffi::c_void,
    surface: *mut std::ffi::c_void,
) -> i32 {
    // SAFETY: forwarded from the shim's own contract for this callback.
    unsafe { gui_open_with::<P>(ctx, RawWindowHandle::WaylandSurface { display, surface }) }
}

/// `Editor::event_fd` of the open editor, or -1.
pub(crate) unsafe extern "C" fn cb_gui_event_fd<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> i32 {
    run_extern_callback_with::<P, i32>("vst3", "gui_event_fd", -1, || {
        #[cfg(unix)]
        {
            // SAFETY: `ctx` is the live instance the shim registered.
            let inst = unsafe { &*ctx.cast::<Vst3Instance<P>>() };
            let gui = inst.gui.enter();
            gui.editor.as_ref().and_then(|e| e.event_fd()).unwrap_or(-1)
        }
        #[cfg(not(unix))]
        {
            let _ = ctx;
            -1
        }
    })
}

/// `Editor::on_fd`. Skipped while a GUI callback holds the editor; the
/// 16 ms timer catches up.
pub(crate) unsafe extern "C" fn cb_gui_on_fd<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    run_extern_callback_with::<P, ()>("vst3", "gui_on_fd", (), || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(mut gui) = inst.gui.try_enter()
            && let Some(editor) = gui.editor.as_mut()
        {
            editor.on_fd();
        }
    });
}

/// `Editor::on_timer`, skipped like `cb_gui_on_fd`.
pub(crate) unsafe extern "C" fn cb_gui_on_timer<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    run_extern_callback_with::<P, ()>("vst3", "gui_on_timer", (), || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(mut gui) = inst.gui.try_enter()
            && let Some(editor) = gui.editor.as_mut()
        {
            editor.on_timer();
        }
    });
}

/// `Editor::set_wayland_sync` around host-driven `IPlugView::onSize`.
pub(crate) unsafe extern "C" fn cb_gui_wayland_set_sync<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sync: i32,
) {
    run_extern_callback_with::<P, ()>("vst3", "gui_wayland_set_sync", (), || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(mut gui) = inst.gui.try_enter()
            && let Some(editor) = gui.editor.as_mut()
        {
            editor.set_wayland_sync(sync != 0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_value_fixture::TestPlugin;
    use crate::lifecycle::{cb_create, cb_destroy};
    use moose_core::thread::MainThread;
    use std::sync::Mutex;

    struct RecordingEditor {
        size: (u32, u32),
        open: bool,
        scales: Arc<Mutex<Vec<f64>>>,
    }

    impl Editor for RecordingEditor {
        fn size(&self) -> (u32, u32) {
            self.size
        }

        fn can_resize(&self) -> bool {
            true
        }

        fn set_size(&mut self, width: u32, height: u32) -> bool {
            self.size = (width, height);
            true
        }

        fn set_scale_factor(&mut self, factor: f64) {
            self.scales.lock().unwrap().push(factor);
        }

        fn open(&mut self, _parent: RawWindowHandle, _context: PluginContext) {
            self.open = true;
        }

        fn close(&mut self) {
            self.open = false;
        }

        fn is_open(&self) -> bool {
            self.open
        }
    }

    #[test]
    fn vst3_host_context_routes_state_and_signals_through_host_queue() {
        unsafe {
            let ctx = cb_create::<TestPlugin>();
            assert!(!ctx.is_null());
            let inst = &*ctx.cast::<Vst3Instance<TestPlugin>>();
            let editor_ctx = editor_context::<TestPlugin>(ctx);

            // State from the editor is queued for the processor, and reads
            // see the queued state before it is applied.
            editor_ctx.set_state(vec![1, 2, 3]);
            assert_eq!(editor_ctx.get_state(), [1, 2, 3]);

            editor_ctx.mark_dirty();
            editor_ctx.request_restart();
            editor_ctx.notify_host(HostNotice::AUDIO_PORTS);
            editor_ctx.window_rescaled(1.5);
            assert_eq!(
                f64::from_bits(inst.window_scale.load(Ordering::Relaxed)),
                1.5
            );
            // No live COM host with `IComponentHandler3`: the editor shows
            // its own menu.
            assert!(!editor_ctx.context_menu(MenuTarget::Global, 12, 34));

            let main = MainThread::assume();
            let drain = inst.host_queue.drain(&main);
            assert!(drain.signals.contains(HostSignal::MARK_DIRTY));
            assert!(drain.signals.contains(HostSignal::RESTART));
            assert!(drain.signals.notices().contains(HostNotice::AUDIO_PORTS));
            assert_eq!(drain.count(), 0, "a refused menu posts nothing");

            cb_destroy::<TestPlugin>(ctx);
        }
    }

    #[test]
    fn busy_gui_get_size_returns_cached_last_phys_size_instead_of_zero() {
        unsafe {
            let ctx = cb_create::<TestPlugin>();
            let inst = &*ctx.cast::<Vst3Instance<TestPlugin>>();
            inst.gui.enter().editor = Some(Box::new(RecordingEditor {
                size: (640, 400),
                open: false,
                scales: Arc::default(),
            }));

            let (mut w, mut h) = (0, 0);
            cb_gui_get_size::<TestPlugin>(ctx, &raw mut w, &raw mut h);
            assert_eq!((w, h), (640, 400));

            // Simulate a re-entrant `IPlugView::getSize` while the GUI cell is
            // already held and no `pending_resize` is queued.
            let held = inst.gui.enter();
            let (mut busy_w, mut busy_h) = (0, 0);
            cb_gui_get_size::<TestPlugin>(ctx, &raw mut busy_w, &raw mut busy_h);
            assert_eq!((busy_w, busy_h), (640, 400));
            drop(held);

            cb_destroy::<TestPlugin>(ctx);
        }
    }

    #[test]
    fn gui_open_with_only_replays_reported_host_scale() {
        unsafe {
            let ctx = cb_create::<TestPlugin>();
            let inst = &*ctx.cast::<Vst3Instance<TestPlugin>>();
            let scales = Arc::new(Mutex::new(Vec::new()));
            inst.gui.enter().editor = Some(Box::new(RecordingEditor {
                size: (500, 300),
                open: false,
                scales: Arc::clone(&scales),
            }));

            assert_eq!(inst.reported_host_scale(), None);
            assert_eq!(gui_open_with::<TestPlugin>(ctx, RawWindowHandle::X11(1)), 1);
            assert!(
                scales.lock().unwrap().is_empty(),
                "must not pin editor to default 1.0 scale when host reported no content scale"
            );

            cb_gui_close::<TestPlugin>(ctx);
            cb_gui_set_content_scale::<TestPlugin>(ctx, 2.0);
            scales.lock().unwrap().clear();

            assert_eq!(gui_open_with::<TestPlugin>(ctx, RawWindowHandle::X11(1)), 1);
            // macOS uses AppKit backing scale rather than IPlugViewContentScaleSupport,
            // so `cb_gui_set_content_scale` does not record a host scale there.
            #[cfg(target_os = "macos")]
            assert!(scales.lock().unwrap().is_empty());
            #[cfg(not(target_os = "macos"))]
            assert_eq!(*scales.lock().unwrap(), vec![2.0, 2.0]);

            cb_destroy::<TestPlugin>(ctx);
        }
    }

    #[test]
    fn gui_set_size_clears_stale_pending_resize() {
        unsafe {
            let ctx = cb_create::<TestPlugin>();
            let inst = &*ctx.cast::<Vst3Instance<TestPlugin>>();
            inst.gui.enter().editor = Some(Box::new(RecordingEditor {
                size: (400, 300),
                open: true,
                scales: Arc::default(),
            }));

            // Queue a deferred resize while the GUI cell is busy.
            let held = inst.gui.enter();
            cb_gui_set_size::<TestPlugin>(ctx, 900, 700);
            assert_ne!(inst.pending_resize.load(Ordering::Relaxed), 0);
            drop(held);

            // An immediate resize after the cell is released must clear the
            // older queued resize so `cb_gui_get_size` does not overwrite it.
            cb_gui_set_size::<TestPlugin>(ctx, 640, 480);
            assert_eq!(inst.pending_resize.load(Ordering::Relaxed), 0);

            let (mut w, mut h) = (0, 0);
            cb_gui_get_size::<TestPlugin>(ctx, &raw mut w, &raw mut h);
            assert_eq!((w, h), (640, 480));

            cb_destroy::<TestPlugin>(ctx);
        }
    }

    #[test]
    fn midi_mapping_rejects_negative_channel() {
        unsafe {
            let ctx = cb_create::<TestPlugin>();
            let mut param_id = u32::MAX;
            assert_eq!(
                cb_midi_mapping_get_param_id::<TestPlugin>(ctx, 0, -1, 1, &raw mut param_id),
                0
            );
            assert_eq!(param_id, u32::MAX, "a refused query writes nothing");
            cb_destroy::<TestPlugin>(ctx);
        }
    }
}
