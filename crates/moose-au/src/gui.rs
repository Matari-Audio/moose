//! Editor callbacks and the shim's view entry points.

use crate::instance::AuInstance;

#[cfg(target_os = "macos")]
use moose_core::Float;
use moose_core::editor::Editor;
use moose_core::editor::fit_logical_size;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::editor::{ClosureBridge, PluginContext, RawWindowHandle, SendPtr};
use moose_core::export::PluginExport;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::state;
use moose_core::wrapper::run_extern_callback_with;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_params::Params;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use std::sync::Arc;
use std::sync::atomic::Ordering;

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
            let _ = ctx;
            let _ = parent;
            let _ = std::marker::PhantomData::<P>;
            0
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        unsafe {
            let inst = &*ctx.cast::<AuInstance<P>>();
            let mut gui = inst.gui.enter();
            if let Some(ref mut editor) = gui.editor {
                let params = Arc::clone(&inst.params_arc);
                let meter_store = Arc::clone(&inst.meter_store);
                let snapshot = Arc::clone(&inst.snapshot);
                let pending_state_for_get = inst.pending_state.clone();
                let ctx_raw = SendPtr::new(ctx);
                let params_for_set = params.clone();
                let params_for_get = params.clone();
                let params_for_plain = params.clone();
                let params_for_fmt = params.clone();
                let params_for_ctx = params.clone();
                let task_spawner_for_ctx = inst.task_spawner.clone();
                let pending_state_for_set = inst.pending_state.clone();
                let transport_slot = inst.transport_slot.clone();
                let ctx_for_begin = ctx_raw;
                let ctx_for_end = ctx_raw;
                // iOS AU v3 hosts the editor inside an .appex; v2's
                // `AUEventListener` doesn't exist there. Parameter
                // changes from the editor flow to the host directly
                // through the AUParameterTree's setter (handled by the
                // Swift shim). The begin/set/end closures are no-ops on
                // iOS so the plugin's editor code stays platform-agnostic.
                let context = PluginContext::from_closures(
                    ClosureBridge {
                        #[cfg(target_os = "macos")]
                        begin_edit: Box::new(move |id| {
                            // Broadcasts kAudioUnitEvent_BeginParameterChangeGesture
                            // via AUEventListenerNotify so hosts (Logic, Live,
                            // Reaper) group subsequent set_param calls into one
                            // undo step and one automation gesture.
                            moose_au_v2_host_begin_param_gesture(
                                ctx_for_begin.as_ptr().cast_mut(),
                                id,
                            );
                        }),
                        #[cfg(target_os = "ios")]
                        begin_edit: Box::new(move |_id| {
                            let _ = ctx_for_begin;
                        }),
                        #[cfg(target_os = "macos")]
                        set_param: Box::new(move |id, value| {
                            // One combined trait dispatch (set_normalized
                            // + get_plain) instead of two - the
                            // `#[derive(Params)]` impl can compute both in
                            // a single match-arm walk.
                            let plain = f32::from_f64(
                                params_for_set.set_normalized_returning_plain(id, value),
                            );
                            moose_au_v2_host_set_param(ctx_raw.as_ptr().cast_mut(), id, plain);
                        }),
                        #[cfg(target_os = "ios")]
                        set_param: Box::new(move |id, value| {
                            // No host-notify on iOS; just write the
                            // normalised value through. The Swift shim
                            // polls the parameter tree.
                            let _ = ctx_raw;
                            let _ = params_for_set.set_normalized_returning_plain(id, value);
                        }),
                        #[cfg(target_os = "macos")]
                        end_edit: Box::new(move |id| {
                            // Closes the gesture started by begin_edit so the
                            // host commits the undo group / stops automation
                            // recording.
                            moose_au_v2_host_end_param_gesture(ctx_for_end.as_ptr().cast_mut(), id);
                        }),
                        #[cfg(target_os = "ios")]
                        end_edit: Box::new(move |_id| {
                            let _ = ctx_for_end;
                        }),
                        request_resize: Box::new(move |w, h| {
                            // AU v2 has no host-driven resize API: the
                            // host observes the plug-in's NSView frame
                            // via AppKit and updates its container in
                            // response. So `ctx.request_resize` here
                            // routes back into the editor's own
                            // `set_size`, which resizes the baseview
                            // NSView; AppKit propagates the frame
                            // change to the host as a notification.
                            //
                            // SAFETY: `ctx_raw` points at the live
                            // `AuInstance<P>`. The closure runs on the
                            // GUI thread, same as `cb_gui_open` which
                            // installed it.
                            if w == 0 || h == 0 {
                                return false;
                            }
                            let inst = &*ctx_raw.as_ptr().cast::<AuInstance<P>>();
                            // An author can call this from inside an `Editor`
                            // method (`set_size`, `state_changed`) that a GUI
                            // callback invoked while holding the `gui` cell -
                            // re-entering `enter()` there is same-thread
                            // aliasing UB. `try_enter` applies the resize when
                            // the cell is free (the common editor-loop path)
                            // and otherwise stashes it for `cb_gui_get_size` to
                            // apply on the next size query.
                            if let Some(mut gui) = inst.gui.try_enter() {
                                let resized = gui.editor.as_mut().is_some_and(|e| e.set_size(w, h));
                                // The host sees the container, not the editor's
                                // own view: size it (AU v2) or announce it
                                // (AU v3 `preferredContentSize`) to match.
                                #[cfg(target_os = "macos")]
                                if resized
                                    && gui.view != 0
                                    && let Some((w, h)) = gui.editor.as_ref().map(|e| e.size())
                                {
                                    moose_au_view_fit(gui.view as *mut std::ffi::c_void, w, h);
                                }
                                resized
                            } else {
                                inst.pending_resize
                                    .store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
                                true
                            }
                        }),
                        get_param: Box::new(move |id| {
                            params_for_get.get_normalized(id).unwrap_or(0.0)
                        }),
                        get_param_plain: Box::new(move |id| {
                            params_for_plain.get_plain(id).unwrap_or(0.0)
                        }),
                        format_param: Box::new(move |id| {
                            let plain = params_for_fmt.get_plain(id).unwrap_or(0.0);
                            params_for_fmt
                                .format_value(id, plain)
                                .unwrap_or_else(|| format!("{plain:.1}"))
                        }),
                        get_meter: Box::new(move |id| meter_store.read(id)),
                        get_state: Box::new(move || {
                            // Editor state read: never touches the plugin. A
                            // load still queued for the audio thread wins.
                            moose_core::wrapper::custom_state_view(
                                &snapshot,
                                &pending_state_for_get,
                            )
                        }),
                        set_state: Box::new(move |bytes| {
                            // The editor sends RAW custom-state bytes -
                            // exactly what `save_state()` emits and
                            // `get_state` above returns - NOT a full
                            // `serialize_state` envelope. Route them to the
                            // plugin's `load_state` on the audio thread via
                            // the same handoff queue the host load path uses
                            // (the queue is what avoids aliasing
                            // `process()`'s `&mut plugin`). No params ride
                            // along: the editor mutates params through
                            // `set_param`.
                            pending_state_for_set.force_push(state::DeserializedState {
                                params: Vec::new(),
                                extra: Some(bytes),
                                persist: Vec::new(),
                            });
                        }),
                        transport: Box::new(move || transport_slot.read()),
                    },
                    params_for_ctx,
                )
                .with_tasks(task_spawner_for_ctx);
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
}
