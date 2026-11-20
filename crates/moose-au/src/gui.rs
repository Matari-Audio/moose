//! Editor callbacks and the shim's view entry points.

use crate::instance::{AuCtx, AuInstance, StateLoadQueue};

#[cfg(target_os = "macos")]
use moose_core::Float;
use moose_core::TransportSlot;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::editor::RawWindowHandle;
use moose_core::editor::{Editor, EditorBridge, HostContext, PluginContext, fit_logical_size};
use moose_core::events::TransportInfo;
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostQueue, HostSignal};
use moose_core::main_thread::HostNotice;
use moose_core::meters::MeterStore;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::thread::MainThread;
use moose_core::wrapper::{custom_state_view, run_extern_callback_with};
use moose_params::Params;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

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
    /// # Safety
    /// `ctx` is a live `AuInstance<P>` pointer.
    unsafe fn new(ctx: *mut std::ffi::c_void) -> Self {
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

impl<P: PluginExport> EditorBridge for AuHost<P> {
    fn begin_edit(&self, id: u32) {
        // Broadcasts kAudioUnitEvent_BeginParameterChangeGesture via
        // AUEventListenerNotify on macOS AU v2 so hosts (Logic, Live,
        // Reaper) group subsequent set_param calls into one undo step and
        // automation gesture. iOS AU v3 notifies via the AUParameterTree.
        #[cfg(target_os = "macos")]
        self.queue.with_open(|| unsafe {
            moose_au_v2_host_begin_param_gesture(self.ctx.as_ptr(), id);
        });
        #[cfg(not(target_os = "macos"))]
        let _ = (self.ctx, id);
    }

    fn set_param(&self, id: u32, normalized: f64) {
        let plain = self.params.set_normalized_returning_plain(id, normalized);
        #[cfg(target_os = "macos")]
        self.queue.with_open(|| unsafe {
            moose_au_v2_host_set_param(self.ctx.as_ptr(), id, f32::from_f64(plain));
        });
        #[cfg(not(target_os = "macos"))]
        let _ = (self.ctx, plain);
    }

    fn end_edit(&self, id: u32) {
        #[cfg(target_os = "macos")]
        self.queue.with_open(|| unsafe {
            moose_au_v2_host_end_param_gesture(self.ctx.as_ptr(), id);
        });
        #[cfg(not(target_os = "macos"))]
        let _ = (self.ctx, id);
    }

    fn request_resize(&self, w: u32, h: u32) -> bool {
        // AU v2 has no host-driven resize API: the host observes the
        // plug-in's NSView frame via AppKit and updates its container in
        // response. `request_resize` routes back into the editor's own
        // `set_size` (and fits the container view on macOS).
        if w == 0 || h == 0 {
            return false;
        }
        self.queue
            .with_open(|| {
                // SAFETY: `with_open` keeps the `AuInstance<P>` alive against `cb_destroy`.
                let inst = unsafe { &*self.ctx.as_ptr().cast::<AuInstance<P>>() };
                // `try_enter` applies the resize when the cell is free and otherwise
                // stashes it for `cb_gui_get_size` to apply on the next size query.
                if let Some(mut gui) = inst.gui.try_enter() {
                    let resized = gui.editor.as_mut().is_some_and(|e| e.set_size(w, h));
                    #[cfg(target_os = "macos")]
                    if resized
                        && gui.view != 0
                        && let Some((w, h)) = gui.editor.as_ref().map(|e| e.size())
                    {
                        unsafe { moose_au_view_fit(gui.view as *mut std::ffi::c_void, w, h) };
                    }
                    resized
                } else {
                    inst.pending_resize
                        .store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
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
    let inst = unsafe { &*ctx.cast::<AuInstance<P>>() };
    let host: Arc<dyn HostContext> = Arc::new(unsafe { AuHost::<P>::new(ctx) });
    PluginContext::from_host(host, Arc::clone(&inst.params_arc) as Arc<dyn Params>)
        .with_tasks(inst.task_spawner.clone())
}

/// Drain the instance's [`HostQueue`] on the main thread.
///
/// # Safety
/// `ctx` is a live `AuInstance<P>` pointer.
#[cfg_attr(
    not(any(target_vendor = "apple", test)),
    allow(
        dead_code,
        reason = "called by the Apple main-queue dispatch source and unit tests"
    )
)]
pub(crate) unsafe fn drain_host_queue<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    main: &MainThread,
) {
    let inst = unsafe { &*ctx.cast::<AuInstance<P>>() };
    let drain = inst.host_queue.drain(main);
    // Commit only the delay lanes covered by the drained signals so a tail-only
    // drain can never silently publish a racing latency observation.
    if drain.has(HostSignal::LATENCY) && inst.delay.commit_latency() {
        #[cfg(target_os = "macos")]
        unsafe {
            moose_au_v2_host_latency_changed(ctx);
        }
    }
    if drain.has(HostSignal::TAIL) && inst.delay.commit_tail() {
        #[cfg(target_os = "macos")]
        unsafe {
            moose_au_v2_host_tail_changed(ctx);
        }
    }
    if drain.has(HostSignal::PERSIST)
        && let Some((id, value)) = inst.params_arc.take_persist_change()
        && value.is_finite()
        && (0.0..=1.0).contains(&value)
        && inst.params_arc.get_normalized(id).is_some()
    {
        let context = unsafe { editor_context::<P>(ctx) };
        context.automate(id, value);
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
    run_extern_callback_with::<P, i32>("au", "gui_has_editor", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut gui = inst.gui.enter();
        if gui.editor.is_none() {
            // Built from the lock-free param store the wrapper already
            // holds outside the plugin, so opening the GUI never
            // stalls the audio thread.
            gui.editor = (inst.editor_builder)(inst.params_arc.clone());
        }
        i32::from(gui.editor.is_some())
    })
}

/// Used by the AU v3 Swift shim in `viewDidLayoutSubviews` to
/// decide whether to forward host bounds changes to the editor,
/// and by the AU v2 `uiViewForAudioUnit:withSize:` path to pick
/// between the host's `preferredSize` and the editor's natural
/// size. Returns 1 / 0 mapping to "yes / no resizable".
pub(crate) unsafe extern "C" fn cb_gui_can_resize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> i32 {
    // `Editor::can_resize` is author code; firewall it (0 = "not resizable").
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

/// Host-driven `set_size`. The AU v2 Cocoa view's
/// `setFrameSize:` / superview-frame observer calls this when the
/// host resizes its outer container; the AU v3 Swift shim calls
/// it from `viewDidLayoutSubviews`. Clamps to the editor's
/// `min_size` / `max_size` / `aspect_ratio` so a host dragging
/// below the editor's floor doesn't clip widgets (mirrors the
/// CLAP and VST3 wrappers).
pub(crate) unsafe extern "C" fn cb_gui_set_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: u32,
    h: u32,
) {
    // `Editor::set_size` / `can_resize` are author code; firewall them.
    run_extern_callback_with::<P, ()>("au", "gui_set_size", (), || unsafe {
        if ctx.is_null() || w == 0 || h == 0 {
            return;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        // `try_enter`, not `enter`: if `editor.set_size` (driven by a plugin
        // `request_resize`) ever makes the host re-drive layout synchronously
        // back into this callback on the same thread, `enter` would
        // double-enter the still-held cell. Skipping a re-entrant commit is
        // safe - the host re-asserts the size on its next layout pass.
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
    // The editor builder + `Editor::size` are author code; firewall them so
    // a panic can't unwind across the C ABI.
    run_extern_callback_with::<P, ()>("au", "gui_get_size", (), || unsafe {
        if ctx.is_null() {
            return;
        }
        // Lazily install the editor here too. Some AU validators
        // (`auval`, Logic Pro's plugin validator) call `..._get_size`
        // before `..._has_editor`, which is the canonical install
        // site. Without the lazy install here those validators see
        // `inst.editor == None` and silently receive a 0x0 view,
        // which shows up as "plugin reports invalid size" in their
        // reports.
        let inst = &*ctx.cast::<AuInstance<P>>();
        // `try_enter`: same synchronous-relayout re-entry guard as
        // `cb_gui_set_size`. If busy, report the size the plugin last
        // requested (still pending), leaving the host's value when there is
        // none - never re-enter the cell.
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
            // Built from the lock-free param store the wrapper already
            // holds outside the plugin, so opening the GUI never
            // stalls the audio thread.
            gui.editor = (inst.editor_builder)(inst.params_arc.clone());
        }
        // Apply a resize the plugin requested re-entrantly through
        // `request_resize` (deferred there because the `gui` cell was held)
        // before reporting the size, so the host sees the value the plugin
        // asked for. A re-request from this `set_size` re-stashes and lands on
        // the next query.
        let packed = inst.pending_resize.swap(0, Ordering::Relaxed);
        if packed != 0
            && let Some(editor) = gui.editor.as_mut()
        {
            #[allow(clippy::cast_possible_truncation)]
            let (rw, rh) = ((packed >> 32) as u32, packed as u32);
            editor.set_size(rw, rh);
        }
        if let Some(ref editor) = gui.editor {
            // AU is macOS-only; hosts embed our NSView inside a Cocoa
            // container at logical-point coordinates and AppKit handles
            // the Retina backing transparently. Report the editor size
            // as-is - no scaling.
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
    // `editor.open` runs author GUI-construction code that can panic;
    // firewall it so the panic can't unwind across the C ABI.
    run_extern_callback_with::<P, i32>("au", "gui_open", 0, || {
        // AU is macOS+iOS-only at runtime. Linux/Windows builds compile
        // the wrapper crate for completeness (it's part of the workspace
        // build matrix) but the body references AppKit / UIKit /
        // AUEventListener APIs that don't exist off-Apple. Stubbing the
        // body keeps the FFI table population in `register_au_inner`
        // type-checking on every platform.
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        {
            let _ = parent;
            if !ctx.is_null() {
                let _ = unsafe { editor_context::<P>(ctx) };
            }
            0
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        unsafe {
            let inst = &*ctx.cast::<AuInstance<P>>();
            let mut gui = inst.gui.enter();
            if let Some(ref mut editor) = gui.editor {
                let context = editor_context::<P>(ctx);
                #[cfg(target_os = "macos")]
                let handle = RawWindowHandle::AppKit(parent);
                #[cfg(target_os = "ios")]
                let handle = RawWindowHandle::UiKit(parent);
                // Scale policy (same as moose-clap / moose-vst3): the host
                // scale wins when one is given, else the OS backing scale,
                // never both multiplied. AU has no host content-scale API,
                // so nothing is pinned via `set_scale_factor`: sizes stay in
                // logical AppKit points and the editor follows the parent
                // view's backing scale.
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
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut gui = inst.gui.enter();
        if let Some(editor) = gui.editor.as_mut() {
            // Same boundary-protection as `cb_destroy`: any panic
            // during `editor.close()` (wgpu surface drop, baseview
            // window close, NSView removal) would otherwise cross
            // the FFI line and become an unhandled ObjC exception
            // in the host.
            let editor_ptr: *mut dyn Editor = editor.as_mut();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (*editor_ptr).close();
            }));
        }
        // Keep the editor alive - just closed, not dropped.
        //
        // Dropping the editor here would synchronously deallocate its
        // baseview NSWindow + content NSView. Logic / Pro Tools tend
        // to call `gui_close` from inside their own NSTimer fire or
        // a `[CALayer display]` callback, both of which run inside an
        // implicit autorelease pool that's about to pop. If
        // `[NSTimer invalidate]` (which baseview's drop chain calls
        // via `WindowHandle::drop`) re-enters that pool's pop
        // sequence, the host crashes inside `objc_release` on a
        // freed `NSAutoreleasePool*`. The editor's `close()` has
        // already released the NSView contents and Metal resources;
        // the lightweight Rust struct that survives is reopened
        // in-place by the next `gui_open` call.
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

// AU v2 host-side automation notifiers: gated to macOS because v2 is
// macOS-only. iOS uses AU v3 exclusively, where host notification
// goes through the parameter tree directly.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    pub(crate) fn moose_au_v2_host_set_param(ctx: *mut std::ffi::c_void, param_id: u32, value: f32);
    pub(crate) fn moose_au_v2_host_begin_param_gesture(ctx: *mut std::ffi::c_void, param_id: u32);
    pub(crate) fn moose_au_v2_host_end_param_gesture(ctx: *mut std::ffi::c_void, param_id: u32);
    pub(crate) fn moose_au_v2_host_latency_changed(ctx: *mut std::ffi::c_void);
    pub(crate) fn moose_au_v2_host_tail_changed(ctx: *mut std::ffi::c_void);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callbacks::{cb_create, cb_destroy};
    use crate::large_state_fixture::LargePlugin;
    use moose_core::editor::PluginContextReadF64;

    type TestPlugin = LargePlugin<16>;

    #[test]
    fn au_host_context_routes_state_and_signals_through_host_queue() {
        unsafe {
            let ctx = cb_create::<TestPlugin>();
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

            let main = MainThread::assume();
            drain_host_queue::<TestPlugin>(ctx, &main);
            assert!(inst.host_queue.pending().is_empty());

            cb_destroy::<TestPlugin>(ctx);
        }
    }
}
