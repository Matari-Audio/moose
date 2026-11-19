//! Editor callbacks.

use crate::ffi;
use crate::instance::Vst3Instance;
use crate::lifecycle::vst3_midi_source;
use crate::midi_proxy::{MIDI_PROXY_PER_CHANNEL, allocated_midi_proxy_id};

use moose_core::editor::{
    ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr, clamp_logical_size,
    fit_logical_size,
};
use moose_core::export::PluginExport;
use moose_core::state;
use moose_core::wrapper::{custom_state_view, run_extern_callback_with};
use moose_params::Params;
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
            gui.editor = (inst.editor_builder)(inst.params_arc.clone());
            // Replay a content scale the host reported before the editor
            // existed (a valid VST3 ordering - `setContentScaleFactor`
            // can precede the editor object). Only a scale the host really
            // sent: replaying a default would pin the editor to it instead
            // of the OS scale. macOS never stores one (AppKit drives Retina).
            if let (Some(scale), Some(editor)) = (inst.reported_host_scale(), gui.editor.as_mut()) {
                editor.set_scale_factor(scale);
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
        // holds the cell (mid-resize). If so, report the size the plugin just
        // requested through `request_resize` (still pending), leaving the
        // host's value untouched when there is none - never re-enter the cell.
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
            let (ew, eh) = editor.size();
            // VST3 `ViewRect` is documented as "in pixels". That's literally
            // true on Windows/Linux, where hosts expect physical pixels and
            // may drive the scale via `IPlugViewContentScaleSupport`. On
            // macOS, AppKit handles the Retina backing automatically and
            // hosts expect logical points - scaling here would double the
            // window on Retina displays.
            #[cfg(target_os = "macos")]
            {
                *w = ew;
                *h = eh;
            }
            #[cfg(not(target_os = "macos"))]
            {
                // Round-to-nearest, not truncate - `(w * scale) as u32`
                // would round 199.9 → 199, drifting one pixel on
                // fractional scales. Matches the CLAP / AAX / `to_physical_px`
                // helper used elsewhere. Logical pixel sizes are bounded
                // by `u32::MAX / scale`; in practice no editor exceeds
                // 16384 logical pixels, so the `f64 → u32` truncation
                // and sign casts are safe.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    let host_scale = inst.host_scale();
                    *w = (f64::from(ew) * host_scale).round() as u32;
                    *h = (f64::from(eh) * host_scale).round() as u32;
                }
            }
        }
    });
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
        drop(gui);
        if changed && lw > 0 && lh > 0 {
            let (pw, ph) = logical_to_phys(lw, lh, scale);
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
                if let Some(editor) = gui.editor.as_mut() {
                    inst.note_window_scale(editor.as_ref());
                    apply_physical_resize(editor.as_mut(), w, h, inst.host_scale());
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
    let channel = u8::try_from(channel).unwrap_or(0);
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
            // Re-sync the editor's content scale to the host's current
            // value right before opening. `host_scale` lives on the
            // persistent instance and can already be set by the time the
            // host attaches (REAPER on Linux reports it via
            // `setContentScaleFactor` around `getSize`, *before* this
            // `open`), but the editor's own copy only got the value if a
            // `setContentScaleFactor` / `has_editor` replay happened to
            // land after the editor object existed and before now. When
            // it didn't - editor built while `host_scale` was still 1.0,
            // host bumped it to 2.0, then attached - the editor would open
            // pinned to 1.0 and render a half-size view in the host's 2x
            // frame (with 2x-off click targets). Applying it here makes
            // the open-time scale authoritative regardless of callback
            // ordering. Skip macOS: Retina is driven through AppKit there
            // and `host_scale` stays 1.0, so pinning would force 1x.
            #[cfg(not(target_os = "macos"))]
            editor.set_scale_factor(inst.host_scale());
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
            let context = PluginContext::from_closures(
                ClosureBridge {
                    begin_edit: Box::new(move |id| {
                        ffi::moose_vst3_begin_edit(ctx_raw.as_ptr().cast_mut(), id);
                    }),
                    set_param: Box::new(move |id, value| {
                        // Single trait dispatch: same value-then-readback
                        // pattern collapsed via the trait helper. The
                        // post-clamp normalized value is what the host
                        // expects for `IComponentHandler::performEdit`.
                        let norm = params_for_set.set_normalized_returning_normalized(id, value);
                        ffi::moose_vst3_perform_edit(ctx_raw.as_ptr().cast_mut(), id, norm);
                    }),
                    end_edit: Box::new(move |id| {
                        ffi::moose_vst3_end_edit(ctx_raw.as_ptr().cast_mut(), id);
                    }),
                    request_resize: Box::new(move |w, h| {
                        // SAFETY: `ctx_raw` is the live
                        // `Vst3Instance` pointer the shim holds in
                        // its ctx -> MooseComponent table. The
                        // closure runs on the GUI thread, same as
                        // `cb_gui_set_content_scale` which is the
                        // only writer of `host_scale`. Routing
                        // through the shim's component (rather
                        // than holding a plug view pointer) avoids
                        // UAF across host editor recreations.
                        let host_scale = (*ctx_raw.as_ptr().cast::<Vst3Instance<P>>()).host_scale();
                        // VST3 hosts speak physical points;
                        // `Editor` speaks logical.
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let pw = (f64::from(w) * host_scale).round() as u32;
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let ph = (f64::from(h) * host_scale).round() as u32;
                        ffi::moose_vst3_request_resize(ctx_raw.as_ptr().cast_mut(), pw, ph) != 0
                    }),
                    get_param: Box::new(move |id| params_for_get.get_normalized(id).unwrap_or(0.0)),
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
                        // Editor state read: lock-free, reads the snapshot
                        // the audio thread publishes each block. Never
                        // touches the plugin, so an editor read can't
                        // stall audio.
                        custom_state_view(&snapshot, &pending_state_for_get)
                    }),
                    set_state: Box::new(move |bytes| {
                        // The editor sends RAW custom-state bytes -
                        // exactly what `save_state()` emits and
                        // `get_state` above returns - NOT a full
                        // `serialize_state` envelope. No params ride
                        // along: the editor mutates params through
                        // `set_param`.
                        //
                        // Always enqueue, never apply here: this closure can
                        // run on the editor's own thread (baseview drives a
                        // separate thread on Linux), and the plugin cell must
                        // only ever be entered from the audio thread (active)
                        // or the host main thread (inactive) - never a third.
                        // The audio thread drains the queue while active; while
                        // inactive the main thread drains it in `cb_state_save`
                        // / `cb_set_active`, so an edit made in that window is
                        // neither lost nor stranded.
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
            .with_tasks(task_spawner_for_ctx)
            .with_mark_dirty(Arc::new(move || {
                ffi::moose_vst3_mark_dirty(ctx_raw.as_ptr().cast_mut());
            }))
            .with_group_edits(Arc::new(move |edge| {
                ffi::moose_vst3_group_edit(ctx_raw.as_ptr().cast_mut(), edge);
            }));
            editor.open(handle, context);
            // Re-sync the scale after `open`: on Windows `open` pumps
            // messages, so a re-entrant `setContentScaleFactor` may have
            // updated `host_scale` while `cb_gui_set_content_scale` had to skip
            // the editor (the cell was held here). Apply the authoritative
            // value now that the editor exists. macOS drives Retina through
            // AppKit, so `host_scale` stays 1.0 and pinning would force 1x.
            #[cfg(not(target_os = "macos"))]
            editor.set_scale_factor(inst.host_scale());
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
    run_extern_callback_with::<P, i32>("vst3", "gui_event_fd", -1, || unsafe {
        #[cfg(unix)]
        {
            let inst = &*ctx.cast::<Vst3Instance<P>>();
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
