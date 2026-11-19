//! Extension: `clap.gui`, plus the editor context and instance accessors.

use crate::context_menu;
use crate::data::GuiMode;
use crate::data::GuiParamChange;
use crate::data_from_plugin;
use crate::note_name;
use crate::param_indication;
use crate::params_ext::make_params_extension;
use crate::ports::configurable_ports_apply;
use crate::ports::configurable_ports_can_apply;
use crate::ports::make_audio_ports_config_extension;
use crate::ports::make_audio_ports_extension;
use crate::ports::make_note_ports_extension;
use crate::ports::ports_activation_set_active;
use crate::ports::ports_activation_while_processing;
use crate::ports::surround_map;
use crate::ports::surround_mask;
use crate::remote_controls::make_remote_controls_extension;
use crate::resolved_name;
use crate::state_ext::make_preset_load_extension;
use crate::state_ext::make_state_extension;
use crate::state_ext::request_presentation_refresh;
use crate::state_ext::state_context_load;
use crate::state_ext::state_context_save;
use crate::track_info;
use crate::voice_info;
#[cfg(all(target_os = "linux", feature = "clap-draft"))]
use crate::wayland_nested;

use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_plugin_audio_ports};
use clap_sys::ext::audio_ports_activation::{
    CLAP_EXT_AUDIO_PORTS_ACTIVATION, CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT,
    clap_plugin_audio_ports_activation,
};
use clap_sys::ext::audio_ports_config::{
    CLAP_EXT_AUDIO_PORTS_CONFIG, clap_plugin_audio_ports_config,
};
use clap_sys::ext::configurable_audio_ports::{
    CLAP_EXT_CONFIGURABLE_AUDIO_PORTS, CLAP_EXT_CONFIGURABLE_AUDIO_PORTS_COMPAT,
    clap_plugin_configurable_audio_ports,
};
use clap_sys::ext::context_menu::{
    CLAP_EXT_CONTEXT_MENU, CLAP_EXT_CONTEXT_MENU_COMPAT, clap_plugin_context_menu,
};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_name::{CLAP_EXT_NOTE_NAME, clap_plugin_note_name};
use clap_sys::ext::note_ports::{CLAP_EXT_NOTE_PORTS, clap_plugin_note_ports};
use clap_sys::ext::param_indication::{
    CLAP_EXT_PARAM_INDICATION, CLAP_EXT_PARAM_INDICATION_COMPAT, clap_plugin_param_indication,
};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_plugin_params};
use clap_sys::ext::preset_load::{
    CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT, clap_plugin_preset_load,
};
use clap_sys::ext::remote_controls::{
    CLAP_EXT_REMOTE_CONTROLS, CLAP_EXT_REMOTE_CONTROLS_COMPAT, clap_plugin_remote_controls,
};
use clap_sys::ext::render::{
    CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, clap_plugin_render, clap_plugin_render_mode,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::ext::state_context::{CLAP_EXT_STATE_CONTEXT, clap_plugin_state_context};
use clap_sys::ext::surround::{CLAP_EXT_SURROUND, CLAP_EXT_SURROUND_COMPAT, clap_plugin_surround};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_plugin_tail};
use clap_sys::ext::track_info::{
    CLAP_EXT_TRACK_INFO, CLAP_EXT_TRACK_INFO_COMPAT, clap_plugin_track_info,
};
use clap_sys::ext::voice_info::{CLAP_EXT_VOICE_INFO, clap_plugin_voice_info};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use moose_core::config::ProcessMode;
use moose_core::editor::{
    ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr, fit_logical_size,
};
use moose_core::export::PluginExport;
use moose_core::state;
use moose_core::wrapper::{custom_state_view, run_extern_callback_with};
use moose_params::Params;
use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// GUI extension
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
use clap_sys::ext::gui::CLAP_WINDOW_API_COCOA;
#[cfg(target_os = "windows")]
use clap_sys::ext::gui::CLAP_WINDOW_API_WIN32;
use clap_sys::ext::gui::{
    CLAP_EXT_GUI, clap_gui_resize_hints, clap_host_gui, clap_plugin_gui, clap_window,
};
#[cfg(target_os = "linux")]
use clap_sys::ext::gui::{CLAP_WINDOW_API_WAYLAND, CLAP_WINDOW_API_X11};

pub(crate) unsafe extern "C" fn gui_is_api_supported<P: PluginExport>(
    plugin: *const clap_plugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    #[cfg(not(target_os = "linux"))]
    let _ = plugin;
    if api.is_null() {
        return false;
    }
    run_extern_callback_with::<P, bool>("CLAP", "gui_is_api_supported", false, || unsafe {
        let api = CStr::from_ptr(api);
        #[cfg(target_os = "linux")]
        if api == CLAP_WINDOW_API_WAYLAND {
            if !is_floating {
                return false;
            }
            let data = data_from_plugin::<P>(plugin);
            let mut gui = data.gui.enter();
            if gui.editor.is_none() {
                gui.editor = (data.editor_builder)(data.params_arc.clone());
            }
            return gui
                .editor
                .as_ref()
                .is_some_and(|editor| editor.supports_floating_wayland());
        }
        #[cfg(target_os = "linux")]
        if api == CLAP_WINDOW_API_X11 && is_floating {
            let data = data_from_plugin::<P>(plugin);
            let mut gui = data.gui.enter();
            if gui.editor.is_none() {
                gui.editor = (data.editor_builder)(data.params_arc.clone());
            }
            return gui
                .editor
                .as_ref()
                .is_some_and(|editor| editor.supports_floating_x11());
        }
        #[cfg(all(target_os = "linux", feature = "clap-draft"))]
        if api == wayland_nested::API {
            let data = data_from_plugin::<P>(plugin);
            if is_floating || !wayland_nested::host_supports(data.host) {
                return false;
            }
            let mut gui = data.gui.enter();
            if gui.editor.is_none() {
                gui.editor = (data.editor_builder)(data.params_arc.clone());
            }
            return gui
                .editor
                .as_ref()
                .is_some_and(|editor| editor.supports_wayland_embedding());
        }
        if is_floating {
            return false;
        }
        #[cfg(target_os = "macos")]
        if api == CLAP_WINDOW_API_COCOA {
            return true;
        }
        #[cfg(target_os = "windows")]
        if api == CLAP_WINDOW_API_WIN32 {
            return true;
        }
        #[cfg(target_os = "linux")]
        if api == CLAP_WINDOW_API_X11 {
            return true;
        }
        false
    })
}

pub(crate) unsafe extern "C" fn gui_get_preferred_api<P: PluginExport>(
    plugin: *const clap_plugin,
    api: *mut *const c_char,
    is_floating: *mut bool,
) -> bool {
    #[cfg(not(target_os = "linux"))]
    let _ = plugin;
    if api.is_null() || is_floating.is_null() {
        return false;
    }
    unsafe {
        #[cfg(target_os = "macos")]
        {
            *api = CLAP_WINDOW_API_COCOA.as_ptr();
            *is_floating = false;
            return true;
        }
        #[cfg(target_os = "windows")]
        {
            *api = CLAP_WINDOW_API_WIN32.as_ptr();
            *is_floating = false;
            return true;
        }
        #[cfg(target_os = "linux")]
        {
            // Embedded X11 whenever an X server (incl. XWayland) is reachable:
            // embedding hosts such as Bitwig follow this preference and never
            // show a floating window. Floating Wayland stays available to
            // hosts that request it, and is preferred only without X11.
            if std::env::var_os("DISPLAY").is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_some()
                && gui_is_api_supported::<P>(plugin, CLAP_WINDOW_API_WAYLAND.as_ptr(), true)
            {
                *api = CLAP_WINDOW_API_WAYLAND.as_ptr();
                *is_floating = true;
                return true;
            }
            *api = CLAP_WINDOW_API_X11.as_ptr();
            *is_floating = false;
            return true;
        }
        #[allow(unreachable_code)]
        false
    }
}

pub(crate) unsafe extern "C" fn gui_create<P: PluginExport>(
    plugin: *const clap_plugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    // The editor builder is author code; firewall its construction so a
    // panic can't unwind across the C ABI (false = "no editor created").
    run_extern_callback_with::<P, bool>("CLAP", "gui_create", false, || unsafe {
        if !gui_is_api_supported::<P>(plugin, api, is_floating) {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        let mut gui = data.gui.enter();
        #[cfg(all(target_os = "linux", feature = "clap-draft"))]
        let nested = CStr::from_ptr(api) == wayland_nested::API;
        #[cfg(not(all(target_os = "linux", feature = "clap-draft")))]
        let nested = false;
        let mode = if nested {
            GuiMode::WaylandNested
        } else if !is_floating {
            GuiMode::Embedded
        } else if CStr::from_ptr(api).to_bytes() == b"x11" {
            GuiMode::FloatingX11
        } else {
            GuiMode::FloatingWayland
        };
        if gui.gui_created {
            return gui.mode == mode;
        }
        // Built through the cached lock-free editor factory, so opening
        // the GUI never stalls the audio thread (and `--shell` rebuilds
        // from the reloaded dylib).
        if gui.editor.is_none() {
            gui.editor = (data.editor_builder)(data.params_arc.clone());
        }
        gui.mode = mode;
        data.floating_mode.store(is_floating, Ordering::Release);
        // Floating and nested windows take their scale from the window system.
        if is_floating || nested {
            data.host_scale.store(0, Ordering::Relaxed);
        }
        gui.title = resolved_name(&data.info).into();
        // Each `create` builds a fresh editor, but the host's scale lives
        // on the instance and outlives `destroy`. A host that sets it only
        // once, or calls `set_scale` before `create`, would leave the new
        // editor at 1.0 inside the host-scaled frame `gui_get_size`
        // reports, so replay it. VST3's `cb_gui_open` does the same.
        if let (Some(scale), Some(editor)) = (data.reported_host_scale(), gui.editor.as_mut()) {
            editor.set_scale_factor(scale);
        }
        gui.gui_created = gui.editor.is_some();
        gui.gui_created
    })
}

pub(crate) unsafe extern "C" fn gui_destroy<P: PluginExport>(plugin: *const clap_plugin) {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        // Take the editor out under the cell guard, then release the guard
        // before tearing it down so author teardown code can't re-enter the
        // cell (e.g. a stray `request_resize`).
        let editor = {
            let mut gui = data.gui.enter();
            data.floating_visible.store(false, Ordering::Release);
            data.floating_closed.store(false, Ordering::Release);
            gui.visible = false;
            gui.gui_created = false;
            #[cfg(all(target_os = "linux", feature = "clap-draft"))]
            wayland_nested::unregister(data.host, &mut gui.nested);
            gui.editor.take()
        };
        // Both `editor.close()` and the `Box<dyn Editor>` drop run author
        // teardown (wgpu surface drop, NSView teardown, baseview window
        // close). Firewall the whole teardown - a panic in either would
        // otherwise become an unhandled Obj-C exception and abort the host.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            if let Some(mut editor) = editor {
                editor.close();
            }
        }));
    }
}

pub(crate) unsafe extern "C" fn gui_set_scale<P: PluginExport>(
    plugin: *const clap_plugin,
    scale: f64,
) -> bool {
    // `Editor::set_scale_factor` is author code; firewall it so a panic
    // can't unwind across the C ABI (false = "scale not applied").
    run_extern_callback_with::<P, bool>("CLAP", "gui_set_scale", false, || unsafe {
        // macOS: the Cocoa GUI API is in logical points and AppKit applies
        // the backing scale, so refuse (the spec's answer for "the OS
        // handles scaling"). Accepting would divide host sizes by it.
        if cfg!(target_os = "macos") || !scale.is_finite() || scale <= 0.0 {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        if data.gui.enter().mode != GuiMode::Embedded {
            return false;
        }
        let changed = data.reported_host_scale() != Some(scale);
        data.set_host_scale(scale);
        let mut gui = data.gui.enter();
        let Some(editor) = gui.editor.as_mut() else {
            return true;
        };
        editor.set_scale_factor(scale);
        // An open window's physical size follows the scale; tell the host so
        // its frame matches (it need not re-query `get_size` on its own).
        let (lw, lh) = editor.size();
        let open = editor.is_open() && gui.gui_created;
        drop(gui);
        if changed && open && lw > 0 && lh > 0 {
            request_host_resize(data.host, scale, lw, lh);
        }
        true
    })
}

pub(crate) unsafe extern "C" fn gui_get_size<P: PluginExport>(
    plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    // `Editor::size` is author code; firewall it so a panic can't unwind
    // across the C ABI (false = "size unavailable").
    run_extern_callback_with::<P, bool>("CLAP", "gui_get_size", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        // Re-entered while an outer GUI callback holds the cell (mid-resize):
        // report the size the plugin just requested through `request_resize`,
        // leaving the host's value when there is none - never re-enter the cell.
        let Some(mut gui) = data.gui.try_enter() else {
            let packed = data.pending_resize.load(Ordering::Relaxed);
            if packed == 0 {
                return false;
            }
            #[allow(clippy::cast_possible_truncation)]
            {
                *width = (packed >> 32) as u32;
                *height = packed as u32;
            }
            return true;
        };
        // Apply a resize the plugin requested re-entrantly (stashed by
        // `gui_set_size` because the cell was busy) before reporting.
        let packed = data.pending_resize.swap(0, Ordering::Relaxed);
        if let Some(editor) = gui.editor.as_deref() {
            data.note_window_scale(editor);
        }
        if packed != 0
            && let Some(editor) = gui.editor.as_mut()
        {
            #[allow(clippy::cast_possible_truncation)]
            let (pw, ph) = ((packed >> 32) as u32, packed as u32);
            apply_physical_resize(editor.as_mut(), pw, ph, data.host_scale());
        }
        if let Some(ref editor) = gui.editor {
            data.note_window_scale(editor.as_ref());
            let (w, h) = editor.size();
            // Like VST3, the CLAP spec describes gui size as pixels, but
            // macOS AppKit handles Retina backing automatically. On macOS
            // we report logical points and let the host / OS scale; on
            // Windows/Linux we multiply by the host-reported scale (default
            // 1.0 if the host never called `gui.set_scale`).
            #[cfg(target_os = "macos")]
            {
                *width = w;
                *height = h;
            }
            #[cfg(not(target_os = "macos"))]
            {
                // Round-to-nearest, not truncate - `(w * scale) as u32`
                // would round 199.9 → 199, drifting one pixel on
                // fractional scales. Matches VST3 / AAX / the
                // `to_physical_px` helper used elsewhere. Logical
                // pixel sizes are bounded by `u32::MAX / scale`; in
                // practice no editor exceeds 16384 logical pixels, so
                // the `f64 → u32` truncation/sign casts are safe.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    let host_scale = data.host_scale();
                    *width = (f64::from(w) * host_scale).round() as u32;
                    *height = (f64::from(h) * host_scale).round() as u32;
                }
            }
            return true;
        }
        false
    })
}

pub(crate) unsafe extern "C" fn gui_can_resize<P: PluginExport>(
    plugin: *const clap_plugin,
) -> bool {
    // `Editor::can_resize` is author code; firewall it (false = "not resizable").
    run_extern_callback_with::<P, bool>("CLAP", "gui_can_resize", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.gui
            .enter()
            .editor
            .as_ref()
            .is_some_and(|e| e.can_resize())
    })
}

pub(crate) unsafe extern "C" fn gui_set_parent<P: PluginExport>(
    plugin: *const clap_plugin,
    window: *const clap_window,
) -> bool {
    // `editor.open` is author GUI-construction code; firewall it so a panic
    // can't unwind across the C ABI (false = "parenting failed").
    run_extern_callback_with::<P, bool>("CLAP", "gui_set_parent", false, || unsafe {
        gui_set_parent_inner::<P>(plugin, window)
    })
}

pub(crate) unsafe fn gui_set_parent_inner<P: PluginExport>(
    plugin: *const clap_plugin,
    window: *const clap_window,
) -> bool {
    unsafe {
        if window.is_null() || !gui_is_api_supported::<P>(plugin, (*window).api, false) {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        let mut gui = data.gui.enter();
        if !gui.gui_created || gui.mode != GuiMode::Embedded {
            return false;
        }
        let Some(editor) = gui.editor.as_mut() else {
            return false;
        };

        #[cfg(target_os = "macos")]
        let parent_ptr = (*window).specific.cocoa;
        #[cfg(target_os = "windows")]
        let parent_ptr = (*window).specific.win32;
        #[cfg(target_os = "linux")]
        let parent_ptr = (*window).specific.ptr;

        if parent_ptr.is_null() {
            return false;
        }

        let context = editor_context::<P>(plugin);

        #[cfg(target_os = "macos")]
        let handle = RawWindowHandle::AppKit(parent_ptr);
        #[cfg(target_os = "windows")]
        let handle = RawWindowHandle::Win32(parent_ptr);
        #[cfg(target_os = "linux")]
        let handle = RawWindowHandle::X11(parent_ptr as u64);

        editor.open(handle, context);
        // `set_parent` "returns true on success": no window, no success.
        if !editor.is_open() {
            return false;
        }
        // baseview attaches its child NSView at the parent's
        // `(0, 0)`. NSView's coordinate system is unflipped, so
        // `(0, 0)` is the parent's bottom-left - which renders the
        // editor anchored to the bottom-left of Reaper's plugin
        // area when Reaper opens the window at a size larger than
        // the editor's natural dimensions (the common case for
        // backends whose `editor.set_size` doesn't actually apply,
        // e.g. vizia: Reaper falls back to a default plugin-window
        // size and our child sits in the corner of all that extra
        // space). Re-anchor the child to the parent's top with a
        // fully-fixed autoresize mask (`0`) - the parent grows the
        // empty space below/right of the child rather than dragging
        // the child along. We re-pin per-frame from the editor
        // (`reanchor_to_superview_top` in `on_frame`) since
        // baseview's `setFrameSize:` notifications during embed can
        // override the origin we set here.
        #[cfg(target_os = "macos")]
        anchor_child_to_top(parent_ptr);
        true
    }
}

pub(crate) unsafe fn editor_context<P: PluginExport>(plugin: *const clap_plugin) -> PluginContext {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let params = Arc::clone(&data.params_arc);
        let meter_store = Arc::clone(&data.meter_store);
        let snapshot = Arc::clone(&data.snapshot);
        let pending_state_for_get = data.pending_state.clone();
        let gui_changes = data.gui_changes.clone();
        let gui_changes2 = data.gui_changes.clone();
        let gui_changes3 = data.gui_changes.clone();
        let host = SendPtr::new(data.host);
        let host_params = SendPtr::new(data.host_params);
        let request_flush = move || {
            // `host_params` is null when the host omits the optional
            // `clap_host_params` extension (the spec marks it optional);
            // dereferencing it would crash hosts that don't implement
            // params.
            let hp = host_params.as_ptr();
            if hp.is_null() {
                return;
            }
            if let Some(f) = (*hp).request_flush {
                f(host.as_ptr());
            }
        };
        // `request_flush` is a `move ||` over only `Copy` captures, so the closure
        // itself is `Copy` and we re-bind rather than `.clone()`.
        let request_flush2 = request_flush;
        let request_flush3 = request_flush;
        let needs_rescan = data.needs_rescan.clone();
        let presentation_pending = data.presentation_pending.clone();
        let host_for_callback = SendPtr::new(data.host);
        let params_for_set = params.clone();
        let params_for_get = params.clone();
        let params_for_plain = params.clone();
        let params_for_fmt = params.clone();
        let params_for_ctx = params.clone();
        let task_spawner_for_ctx = data.task_spawner.clone();
        let pending_state_for_set = data.pending_state.clone();
        let transport_slot = data.transport_slot.clone();
        PluginContext::from_closures(
            ClosureBridge {
                begin_edit: Box::new(move |id| {
                    // Push can fail only if the audio thread hasn't
                    // drained for `GUI_QUEUE_CAPACITY` gestures.
                    // request_flush below pokes the host to call
                    // back; a value the queue drops falls back to a
                    // values rescan in set_param.
                    let _ = gui_changes.push(GuiParamChange::GestureBegin(id));
                    request_flush();
                }),
                set_param: Box::new(move |id, value| {
                    let plain = params_for_set.set_normalized_returning_plain(id, value);
                    if params_for_set.parameter_affects_presentation(id) {
                        request_presentation_refresh(
                            &presentation_pending,
                            host_for_callback.as_ptr(),
                        );
                    }
                    let queued = gui_changes2.push(GuiParamChange::Value(id, plain)).is_ok();
                    request_flush2();
                    // The queued PARAM_VALUE event is how the host hears
                    // this edit. A values rescan per drag frame made hosts
                    // (Bitwig) re-read every parameter on the main thread
                    // mid-gesture, so it is only the fallback for a value
                    // the full queue dropped.
                    //
                    // Symmetry with the host-pointer null guards at
                    // `:297, :365, :1404`. CLAP guarantees a valid
                    // host on init, but a host that creates a plugin
                    // without ever calling `clap_plugin_init`-style
                    // setup (rare validators) could leave `data.host`
                    // null; the deref below would crash inside the
                    // GUI thread.
                    let host_ptr = host_for_callback.as_ptr();
                    if !queued
                        && !needs_rescan.swap(true, Ordering::Relaxed)
                        && !host_ptr.is_null()
                        && let Some(req_cb) = (*host_ptr).request_callback
                    {
                        req_cb(host_ptr);
                    }
                }),
                end_edit: Box::new(move |id| {
                    let _ = gui_changes3.push(GuiParamChange::GestureEnd(id));
                    request_flush3();
                }),
                request_resize: Box::new({
                    let host_ptr = SendPtr::new(data.host);
                    let plugin_ptr = SendPtr::new(plugin);
                    move |lw, lh| {
                        // Read `host_scale` live (it's atomic) so a
                        // `gui_set_scale` after the editor opened is reflected
                        // instead of using a stale snapshot.
                        let data = data_from_plugin::<P>(plugin_ptr.as_ptr());
                        if data.floating_mode.load(Ordering::Acquire) {
                            return false;
                        }
                        let host_scale = data.host_scale();
                        request_host_resize(host_ptr.as_ptr(), host_scale, lw, lh)
                    }
                }),
                get_param: Box::new(move |id| params_for_get.get_normalized(id).unwrap_or(0.0)),
                get_param_plain: Box::new(move |id| params_for_plain.get_plain(id).unwrap_or(0.0)),
                format_param: Box::new(move |id| {
                    let plain = params_for_fmt.get_plain(id).unwrap_or(0.0);
                    params_for_fmt
                        .format_value(id, plain)
                        .unwrap_or_else(|| format!("{plain:.1}"))
                }),
                get_meter: Box::new(move |id| meter_store.read(id)),
                get_state: Box::new(move || {
                    // Editor state read: never touches the plugin. A load
                    // still queued for the audio thread wins over the slot.
                    custom_state_view(&snapshot, &pending_state_for_get)
                }),
                set_state: Box::new(move |bytes| {
                    // The editor sends RAW custom-state bytes - exactly
                    // what `save_state()` emits and `get_state` above
                    // returns - NOT a full `serialize_state` envelope.
                    // Route them to the plugin's `load_state` through the
                    // handoff queue. No params ride along: the editor
                    // mutates params through `set_param`.
                    //
                    // Always enqueue, never apply here: this closure can run
                    // on the editor's own thread (baseview drives a separate
                    // thread on Linux), and the plugin cell must only ever be
                    // entered from the audio thread (active) or the host main
                    // thread (inactive) - never a third. The audio thread
                    // drains the queue while active; while inactive the main
                    // thread drains it in `state_save` / `deactivate`, so an
                    // edit made in that window is neither lost nor stranded.
                    pending_state_for_set.force_push(state::DeserializedState {
                        extra: Some(bytes),
                        ..state::DeserializedState::default()
                    });
                }),
                transport: Box::new(move || transport_slot.read()),
            },
            params_for_ctx,
        )
        .with_tasks(task_spawner_for_ctx)
        .with_notification_delivery(moose_core::editor::EditorNotificationDelivery::QueuedEdits)
        .with_host_notifier(data.host_notifier.clone())
        .with_window_rescaled({
            // The next `request_resize` converts at the window's new scale
            // while the host has sent none of its own (`host_scale`).
            let plugin = SendPtr::new(plugin);
            Arc::new(move |scale| {
                if scale.is_finite() && scale > 0.0 {
                    let data = data_from_plugin::<P>(plugin.as_ptr());
                    data.window_scale.store(scale.to_bits(), Ordering::Relaxed);
                }
            })
        })
        .with_mark_dirty({
            // `state.mark_dirty` is [main-thread]: flag it and ask for
            // `on_main_thread` (`request_callback` is thread-safe).
            let dirty = data.dirty_pending.clone();
            let host = SendPtr::new(data.host);
            Arc::new(move || {
                let host = host.as_ptr();
                if !dirty.swap(true, Ordering::AcqRel)
                    && !host.is_null()
                    && let Some(request) = (*host).request_callback
                {
                    request(host);
                }
            })
        })
    }
}

/// Move every direct subview of `parent` so its top edge sits at
/// the parent's top in unflipped Cocoa coordinates (where
/// `origin.y = 0` is the bottom edge). Pinning is via the
/// autoresize mask `NSViewMinYMargin | NSViewMaxXMargin`, which
/// keeps the gap between child-bottom and parent-bottom flexible
/// (so a taller parent grows the empty space below the child, not
/// above it) and the gap between child-right and parent-right
/// flexible (so a wider parent grows the empty space to the right
/// of the child). The child stays at its built size; `AppKit` just
/// repositions it as the parent resizes.
///
/// Mirrors `moose-lv2`'s `anchor_child_to_top` (the non-resizable
/// arm of `anchor_child_for_resize`). No `objc` dep needed in
/// moose-clap until the autoresize-mask install path comes back -
/// we use a local `class!`-free `msg_send!` via `objc2_app_kit`
/// types instead.
#[cfg(target_os = "macos")]
pub(crate) unsafe fn anchor_child_to_top(parent: *mut c_void) {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};
    // `MinYMargin | MaxXMargin`: bottom margin and right margin
    // elastic, view size + top/left margins fixed. As the parent
    // resizes, AppKit grows the empty space below/right of the
    // child rather than dragging the child along. Per-frame
    // `reanchor_to_superview_top` (from `on_frame`) catches any
    // host-driven setFrame that bypasses autoresize.
    const NSVIEW_MIN_Y_MARGIN: u64 = 8;
    const NSVIEW_MAX_X_MARGIN: u64 = 4;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsRect {
        origin: NsPoint,
        size: NsSize,
    }
    if parent.is_null() {
        return;
    }
    let parent_obj = parent.cast::<Object>();
    let parent_frame: NsRect = msg_send![parent_obj, frame];
    let subviews: *mut Object = msg_send![parent_obj, subviews];
    if subviews.is_null() {
        return;
    }
    let count: usize = msg_send![subviews, count];
    for i in 0..count {
        let child: *mut Object = msg_send![subviews, objectAtIndex: i];
        if child.is_null() {
            continue;
        }
        let child_frame: NsRect = msg_send![child, frame];
        let new_origin = NsPoint {
            x: child_frame.origin.x,
            y: parent_frame.size.height - child_frame.size.height,
        };
        let _: () = msg_send![child, setFrameOrigin: new_origin];
        let _: () =
            msg_send![child, setAutoresizingMask: NSVIEW_MIN_Y_MARGIN | NSVIEW_MAX_X_MARGIN];
    }
}

pub(crate) unsafe extern "C" fn gui_show<P: PluginExport>(plugin: *const clap_plugin) -> bool {
    run_extern_callback_with::<P, bool>("CLAP", "gui_show", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        let mut gui = data.gui.enter();
        if !gui.gui_created {
            return false;
        }
        if matches!(gui.mode, GuiMode::Embedded | GuiMode::WaylandNested)
            || (gui.visible && data.floating_visible.load(Ordering::Acquire))
        {
            return true;
        }
        gui.visible = false;
        let title = gui.title.clone();
        let mode = gui.mode;
        let Some(editor) = gui.editor.as_mut() else {
            return false;
        };
        let visible = data.floating_visible.clone();
        let closed = data.floating_closed.clone();
        let host = SendPtr::new(data.host);
        visible.store(true, Ordering::Release);
        let on_close: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if visible.swap(false, Ordering::AcqRel) {
                closed.store(true, Ordering::Release);
                let host = host.as_ptr();
                if !host.is_null()
                    && let Some(request) = (*host).request_callback
                {
                    request(host);
                }
            }
        });
        let opened = match mode {
            GuiMode::FloatingX11 => {
                editor.open_floating_x11(editor_context::<P>(plugin), &title, on_close)
            }
            GuiMode::FloatingWayland => {
                editor.open_floating_wayland(editor_context::<P>(plugin), &title, on_close)
            }
            GuiMode::Embedded | GuiMode::WaylandNested => false,
        };
        data.floating_visible.store(opened, Ordering::Release);
        data.note_window_scale(editor.as_ref());
        gui.visible = opened;
        opened
    })
}

pub(crate) unsafe extern "C" fn gui_hide<P: PluginExport>(plugin: *const clap_plugin) -> bool {
    run_extern_callback_with::<P, bool>("CLAP", "gui_hide", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        let mut gui = data.gui.enter();
        if !gui.gui_created {
            return false;
        }
        if matches!(gui.mode, GuiMode::FloatingX11 | GuiMode::FloatingWayland) {
            data.floating_visible.store(false, Ordering::Release);
            if let Some(editor) = gui.editor.as_mut() {
                editor.close();
            }
            gui.visible = false;
        }
        true
    })
}

/// Reports horizontal/vertical/aspect-ratio constraints from the
/// editor. Returns `false` when the editor isn't resizable (Bitwig's
/// probe-call still gets a meaningful `false`, not a `None` slot).
pub(crate) unsafe extern "C" fn gui_get_resize_hints<P: PluginExport>(
    plugin: *const clap_plugin,
    hints: *mut clap_gui_resize_hints,
) -> bool {
    // `Editor::can_resize` / `aspect_ratio` are author code; firewall them
    // (false = "no hints").
    run_extern_callback_with::<P, bool>("CLAP", "gui_get_resize_hints", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        let gui = data.gui.enter();
        let Some(editor) = gui.editor.as_ref() else {
            return false;
        };
        if !editor.can_resize() {
            return false;
        }
        if hints.is_null() {
            return false;
        }
        let (aw, ah) = editor.aspect_ratio().unwrap_or((0, 0));
        let (min, max) = (editor.min_size(), editor.max_size());
        *hints = clap_gui_resize_hints {
            can_resize_horizontally: min.0 < max.0,
            can_resize_vertically: min.1 < max.1,
            preserve_aspect_ratio: editor.aspect_ratio().is_some(),
            aspect_ratio_width: aw,
            aspect_ratio_height: ah,
        };
        true
    })
}

/// CLAP defines no exported Wayland parent token/connection. A host-owned
/// `wl_surface` cannot be used on our independent display connection. Return false
/// rather than treating a foreign proxy pointer as our own Wayland object.
pub(crate) unsafe extern "C" fn gui_set_transient<P: PluginExport>(
    _plugin: *const clap_plugin,
    _window: *const clap_window,
) -> bool {
    false
}

/// Retain a floating title before show and apply changes to an open toplevel.
pub(crate) unsafe extern "C" fn gui_suggest_title<P: PluginExport>(
    plugin: *const clap_plugin,
    title: *const c_char,
) {
    if title.is_null() {
        return;
    }
    run_extern_callback_with::<P, ()>("CLAP", "gui_suggest_title", (), || unsafe {
        let data = data_from_plugin::<P>(plugin);
        let mut gui = data.gui.enter();
        gui.title = CStr::from_ptr(title).to_string_lossy().into_owned();
        let title = gui.title.clone();
        if gui.mode != GuiMode::Embedded
            && let Some(editor) = gui.editor.as_mut()
        {
            editor.set_floating_title(&title);
        }
    });
}

/// Ask the host to resize its window to a logical size via
/// `clap_host_gui.request_resize`. Returns false when the host
/// doesn't expose the gui extension. Logical-to-physical scaling
/// matches `gui_get_size`.
pub(crate) unsafe fn request_host_resize(
    host: *const clap_host,
    host_scale: f64,
    lw: u32,
    lh: u32,
) -> bool {
    unsafe {
        if host.is_null() {
            return false;
        }
        let Some(get_ext) = (*host).get_extension else {
            return false;
        };
        let host_gui_ptr = get_ext(host, CLAP_EXT_GUI.as_ptr()).cast::<clap_host_gui>();
        if host_gui_ptr.is_null() {
            return false;
        }
        let Some(req) = (*host_gui_ptr).request_resize else {
            return false;
        };
        let (pw, ph) = scale_logical_to_physical(lw, lh, host_scale);
        req(host, pw, ph)
    }
}

/// Host commits a new size. When the editor opts into resizing,
/// delegate to `Editor::set_size`; otherwise accept only the
/// editor's current fixed size (Bitwig probe-quirk - see
/// `gui_get_resize_hints` rationale).
pub(crate) unsafe extern "C" fn gui_set_size<P: PluginExport>(
    plugin: *const clap_plugin,
    width: u32,
    height: u32,
) -> bool {
    // `Editor::set_size` / `can_resize` / `size` are author code; firewall
    // them (false = "size rejected").
    run_extern_callback_with::<P, bool>("CLAP", "gui_set_size", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        // A host answering a plugin `request_resize` (`request_host_resize`)
        // synchronously calls `set_size` back here while an outer GUI callback
        // (open / set_scale / a re-entrant set_size) still holds the cell.
        // `try_enter` avoids the same-thread aliasing re-entry: apply when
        // free, else stash for `gui_get_size` to apply on the next size query.
        let Some(mut gui) = data.gui.try_enter() else {
            data.pending_resize.store(
                (u64::from(width) << 32) | u64::from(height),
                Ordering::Relaxed,
            );
            return true;
        };
        let Some(editor) = gui.editor.as_mut() else {
            return false;
        };
        data.note_window_scale(editor.as_ref());
        let host_scale = data.host_scale();
        if editor.can_resize() {
            // Host passes physical points; moose works in logical.
            // Divide by the host-applied scale before handing to the
            // editor so `set_size` receives the same units `size()`
            // returns. Fit the largest on-ratio box *inside* the request:
            // a pure function of the request (no `current` dependence), so
            // it can't oscillate when a host re-asserts a pinned axis every
            // frame (Reaper caps the editor height at its FX-window content
            // height and reverts any grow past it).
            let req = scale_physical_to_logical(width, height, host_scale);
            let (lw, lh) = fit_logical_size(req.0, req.1, editor.as_ref());
            let accepted = editor.set_size(lw, lh);
            // The fit is `<=` the request, so this only ever asks the host to
            // shrink its window onto the on-ratio size (which hosts honour) -
            // never to grow past a pinned axis. Idempotent, so the host's
            // echoed `set_size` agrees and the exchange settles.
            let adopted = editor.size();
            drop(gui);
            if accepted && adopted != req {
                request_host_resize(data.host, host_scale, adopted.0, adopted.1);
            }
            accepted
        } else {
            // Drop the editor borrow before `gui_get_size` re-enters the cell.
            drop(gui);
            let mut current_w: u32 = 0;
            let mut current_h: u32 = 0;
            if !gui_get_size::<P>(plugin, &raw mut current_w, &raw mut current_h) {
                return false;
            }
            width == current_w && height == current_h
        }
    })
}

/// Host asks "what's the nearest size you can render at?". CLAP
/// defines this as "clamp to acceptable", not "reject if not exact".
/// Resizable editors clamp to `min_size` / `max_size` and apply the
/// `aspect_ratio` constraint; fixed-size editors snap to current.
pub(crate) unsafe extern "C" fn gui_adjust_size<P: PluginExport>(
    plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    // `Editor::can_resize` / `size` are author code; firewall them
    // (false = "no adjustment").
    run_extern_callback_with::<P, bool>("CLAP", "gui_adjust_size", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        // `try_enter`, not `enter`: a host may query the nearest valid size
        // while an outer GUI callback holds the cell mid resize round-trip
        // (same hazard the `gui_set_size` / `gui_get_size` siblings guard).
        // When busy, leave the host's requested `*width`/`*height` unchanged
        // rather than forming a second `&mut`.
        let Some(gui) = data.gui.try_enter() else {
            return false;
        };
        let Some(editor) = gui.editor.as_ref() else {
            return false;
        };
        data.note_window_scale(editor.as_ref());
        let host_scale = data.host_scale();
        if editor.can_resize() {
            let req = scale_physical_to_logical(*width, *height, host_scale);
            let (lw, lh) = fit_logical_size(req.0, req.1, editor.as_ref());
            // Convert clamped logical back to physical for the host.
            let (pw, ph) = scale_logical_to_physical(lw, lh, host_scale);
            *width = pw;
            *height = ph;
            true
        } else {
            // Drop the editor borrow before `gui_get_size` re-enters the cell.
            drop(gui);
            let mut current_w: u32 = 0;
            let mut current_h: u32 = 0;
            if !gui_get_size::<P>(plugin, &raw mut current_w, &raw mut current_h) {
                return false;
            }
            *width = current_w;
            *height = current_h;
            true
        }
    })
}

/// Convert physical points (what the host passes in resize APIs)
/// to logical points (what `Editor` works in). CLAP host scale is
/// 1.0 when the host hasn't called `set_scale`.
pub(crate) fn scale_physical_to_logical(pw: u32, ph: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (pw, ph);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lw = (f64::from(pw) / host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lh = (f64::from(ph) / host_scale).round() as u32;
    (lw.max(1), lh.max(1))
}

/// Apply a host-supplied physical `(w, h)` to a resizable editor: convert to
/// logical points, fit to the editor's on-ratio box, and `set_size`. Shared by
/// `gui_set_size` (the host's committed size) and the deferred-resize drain in
/// `gui_get_size`.
pub(crate) fn apply_physical_resize(editor: &mut dyn Editor, w: u32, h: u32, host_scale: f64) {
    if editor.can_resize() {
        let (lw, lh) = scale_physical_to_logical(w, h, host_scale);
        let (cw, ch) = fit_logical_size(lw, lh, editor);
        editor.set_size(cw, ch);
    }
}

/// Inverse of `scale_physical_to_logical`.
pub(crate) fn scale_logical_to_physical(lw: u32, lh: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (lw, lh);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pw = (f64::from(lw) * host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ph = (f64::from(lh) * host_scale).round() as u32;
    (pw.max(1), ph.max(1))
}

pub(crate) fn make_gui_extension<P: PluginExport>() -> clap_plugin_gui {
    clap_plugin_gui {
        is_api_supported: Some(gui_is_api_supported::<P>),
        get_preferred_api: Some(gui_get_preferred_api::<P>),
        create: Some(gui_create::<P>),
        destroy: Some(gui_destroy::<P>),
        set_scale: Some(gui_set_scale::<P>),
        get_size: Some(gui_get_size::<P>),
        can_resize: Some(gui_can_resize::<P>),
        get_resize_hints: Some(gui_get_resize_hints::<P>),
        adjust_size: Some(gui_adjust_size::<P>),
        set_size: Some(gui_set_size::<P>),
        set_parent: Some(gui_set_parent::<P>),
        set_transient: Some(gui_set_transient::<P>),
        suggest_title: Some(gui_suggest_title::<P>),
        show: Some(gui_show::<P>),
        hide: Some(gui_hide::<P>),
    }
}

// ---------------------------------------------------------------------------
// get_extension
// ---------------------------------------------------------------------------

/// Holds the static extension structs. One per monomorphization, which is fine
/// because we only have one plugin type per shared library.
pub(crate) struct Extensions<P: PluginExport> {
    pub(crate) params: clap_plugin_params,
    pub(crate) remote_controls: clap_plugin_remote_controls,
    pub(crate) state: clap_plugin_state,
    pub(crate) state_context: clap_plugin_state_context,
    pub(crate) preset_load: clap_plugin_preset_load,
    pub(crate) audio_ports: clap_plugin_audio_ports,
    pub(crate) audio_ports_config: clap_plugin_audio_ports_config,
    pub(crate) configurable_audio_ports: clap_plugin_configurable_audio_ports,
    pub(crate) audio_ports_activation: clap_plugin_audio_ports_activation,
    pub(crate) surround: clap_plugin_surround,
    pub(crate) note_ports: clap_plugin_note_ports,
    pub(crate) gui: clap_plugin_gui,
    pub(crate) latency: clap_plugin_latency,
    pub(crate) tail: clap_plugin_tail,
    pub(crate) render: clap_plugin_render,
    pub(crate) track_info: clap_plugin_track_info,
    pub(crate) note_name: clap_plugin_note_name,
    pub(crate) voice_info: clap_plugin_voice_info,
    pub(crate) param_indication: clap_plugin_param_indication,
    pub(crate) context_menu: clap_plugin_context_menu,
    pub(crate) _phantom: PhantomData<P>,
}

pub(crate) unsafe extern "C" fn latency_get<P: PluginExport>(plugin: *const clap_plugin) -> u32 {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.latency_cache.load(Ordering::Relaxed)
    }
}

pub(crate) unsafe extern "C" fn tail_get<P: PluginExport>(plugin: *const clap_plugin) -> u32 {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.tail_cache.load(Ordering::Relaxed)
    }
}

/// `clap.render`: we have no hard realtime requirement - the plugin can
/// render offline. Returning `false` lets hosts drive an offline bounce.
pub(crate) unsafe extern "C" fn render_has_hard_realtime<P: PluginExport>(
    _plugin: *const clap_plugin,
) -> bool {
    false
}

/// `clap.render::set`: the host announces realtime vs offline. Store the
/// mode; `activate` re-preps with it (hosts deactivate before an offline
/// bounce), and each `process` block reads it for the per-block mode.
pub(crate) unsafe extern "C" fn render_set<P: PluginExport>(
    plugin: *const clap_plugin,
    mode: clap_plugin_render_mode,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let pm = if mode == CLAP_RENDER_OFFLINE {
            ProcessMode::Offline
        } else {
            ProcessMode::Realtime
        };
        data.render_mode.store(pm.as_u8(), Ordering::Relaxed);
        true
    }
}

impl<P: PluginExport> Extensions<P> {
    pub(crate) fn new() -> Self {
        Self {
            params: make_params_extension::<P>(),
            remote_controls: make_remote_controls_extension::<P>(),
            state: make_state_extension::<P>(),
            state_context: clap_plugin_state_context {
                save: Some(state_context_save::<P>),
                load: Some(state_context_load::<P>),
            },
            preset_load: make_preset_load_extension::<P>(),
            audio_ports: make_audio_ports_extension::<P>(),
            audio_ports_config: make_audio_ports_config_extension::<P>(),
            configurable_audio_ports: clap_plugin_configurable_audio_ports {
                can_apply_configuration: Some(configurable_ports_can_apply::<P>),
                apply_configuration: Some(configurable_ports_apply::<P>),
            },
            audio_ports_activation: clap_plugin_audio_ports_activation {
                can_activate_while_processing: Some(ports_activation_while_processing),
                set_active: Some(ports_activation_set_active::<P>),
            },
            surround: clap_plugin_surround {
                is_channel_mask_supported: Some(surround_mask::<P>),
                get_channel_map: Some(surround_map::<P>),
            },
            note_ports: make_note_ports_extension::<P>(),
            gui: make_gui_extension::<P>(),
            latency: clap_plugin_latency {
                get: Some(latency_get::<P>),
            },
            tail: clap_plugin_tail {
                get: Some(tail_get::<P>),
            },
            render: clap_plugin_render {
                has_hard_realtime_requirement: Some(render_has_hard_realtime::<P>),
                set: Some(render_set::<P>),
            },
            track_info: track_info::make_extension::<P>(),
            note_name: note_name::make_extension::<P>(),
            voice_info: voice_info::make_extension::<P>(),
            param_indication: param_indication::make_extension::<P>(),
            context_menu: context_menu::make_extension::<P>(),
            _phantom: PhantomData,
        }
    }
}

pub(crate) unsafe extern "C" fn clap_plugin_get_extension<P: PluginExport>(
    plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    unsafe {
        if id.is_null() || plugin.is_null() {
            return ptr::null();
        }
        let ext_id = CStr::from_ptr(id);

        let data = data_from_plugin::<P>(plugin);
        let extensions = &data.extensions;

        if ext_id == CLAP_EXT_PARAMS {
            return (&raw const extensions.params).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_REMOTE_CONTROLS || ext_id == CLAP_EXT_REMOTE_CONTROLS_COMPAT {
            let generated = (&raw const extensions.remote_controls).cast::<c_void>();
            return run_extern_callback_with::<P, *const c_void>(
                "CLAP",
                "remote controls",
                generated,
                || {
                    P::clap_extension(CLAP_EXT_REMOTE_CONTROLS)
                        .map_or(generated, moose_core::export::CustomClapExtension::as_ptr)
                },
            );
        }
        if ext_id == CLAP_EXT_STATE {
            return (&raw const extensions.state).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_STATE_CONTEXT {
            return (&raw const extensions.state_context).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_PRESET_LOAD || ext_id == CLAP_EXT_PRESET_LOAD_COMPAT {
            return (&raw const extensions.preset_load).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_AUDIO_PORTS {
            return (&raw const extensions.audio_ports).cast::<c_void>();
        }
        // Only advertise config switching when there's more than one
        // layout to switch between; a single-layout plugin is fully
        // described by `audio_ports` alone.
        if ext_id == CLAP_EXT_AUDIO_PORTS_CONFIG {
            return if P::bus_layouts().len() > 1 {
                (&raw const extensions.audio_ports_config).cast::<c_void>()
            } else {
                ptr::null()
            };
        }
        if ext_id == CLAP_EXT_CONFIGURABLE_AUDIO_PORTS
            || ext_id == CLAP_EXT_CONFIGURABLE_AUDIO_PORTS_COMPAT
        {
            return (&raw const extensions.configurable_audio_ports).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_AUDIO_PORTS_ACTIVATION
            || ext_id == CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT
        {
            return (&raw const extensions.audio_ports_activation).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_SURROUND || ext_id == CLAP_EXT_SURROUND_COMPAT {
            return (&raw const extensions.surround).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_NOTE_PORTS {
            return (&raw const extensions.note_ports).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_GUI {
            return (&raw const extensions.gui).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_LATENCY {
            return (&raw const extensions.latency).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_TAIL {
            return (&raw const extensions.tail).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_RENDER {
            return (&raw const extensions.render).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_TRACK_INFO || ext_id == CLAP_EXT_TRACK_INFO_COMPAT {
            return (&raw const extensions.track_info).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_NOTE_NAME {
            return (&raw const extensions.note_name).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_VOICE_INFO {
            return if data.voice_info_supported.load(Ordering::Relaxed) {
                (&raw const extensions.voice_info).cast::<c_void>()
            } else {
                ptr::null()
            };
        }
        if ext_id == CLAP_EXT_PARAM_INDICATION || ext_id == CLAP_EXT_PARAM_INDICATION_COMPAT {
            return (&raw const extensions.param_indication).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_CONTEXT_MENU || ext_id == CLAP_EXT_CONTEXT_MENU_COMPAT {
            return (&raw const extensions.context_menu).cast::<c_void>();
        }
        #[cfg(all(target_os = "linux", feature = "clap-draft"))]
        if let Some(ext) = wayland_nested::extension::<P>(ext_id) {
            return ext;
        }
        run_extern_callback_with::<P, *const c_void>(
            "CLAP",
            "custom extension",
            ptr::null(),
            || {
                P::clap_extension(ext_id)
                    .map_or(ptr::null(), moose_core::export::CustomClapExtension::as_ptr)
            },
        )
    }
}

/// Read an instance's shared parameter store without taking the DSP cell.
///
/// This remains available while processing. Panics are contained. The callback
/// must follow the thread and realtime rules of the operations it performs.
///
/// # Safety
/// `plugin` must be null or a live CLAP instance created for `P`. The host must
/// not destroy the instance until this call returns.
pub unsafe fn try_with_params<P: PluginExport, R>(
    plugin: *const clap_plugin,
    callback: impl FnOnce(&P::Params) -> R,
) -> Option<R> {
    if plugin.is_null() {
        return None;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: The caller guarantees the pointer and its lifetime.
        let data = unsafe { data_from_plugin::<P>(plugin) };
        callback(&data.params_arc)
    }))
    .ok()
}

/// Access an inactive instance's concrete DSP state without blocking.
///
/// The callback must run on the CLAP main thread. Active, busy, dynamic-shell,
/// and mismatched-state instances return `None`. Panics are contained.
///
/// # Safety
/// `plugin` must be null or a live CLAP instance created for `P`. The host must
/// serialize this call with its main-thread lifecycle callbacks and destruction.
pub unsafe fn try_with_dsp_state<P: PluginExport, T: 'static, R>(
    plugin: *const clap_plugin,
    callback: impl FnOnce(&mut T) -> R,
) -> Option<R> {
    // SAFETY: the caller supplies the matching live instance and serialization.
    unsafe { access_dsp_state::<P, T, R>(plugin, DspAccess::Inactive, callback) }
}

/// Access concrete DSP state while processing is stopped, without blocking.
///
/// Active instances are accepted after `stop_processing`, allowing worker
/// settlement before a same-activation reset. Running, busy, dynamic-shell,
/// and mismatched-state instances return `None`. Panics are contained.
///
/// # Safety
/// `plugin` must be null or a live CLAP instance created for `P`. Call on the
/// CLAP main thread, serialized with every lifecycle callback and destruction.
/// The host must not restart processing until the callback returns.
pub unsafe fn try_with_stopped_dsp_state<P: PluginExport, T: 'static, R>(
    plugin: *const clap_plugin,
    callback: impl FnOnce(&mut T) -> R,
) -> Option<R> {
    // SAFETY: the caller supplies the matching live instance and serialization.
    unsafe { access_dsp_state::<P, T, R>(plugin, DspAccess::Stopped, callback) }
}

#[derive(Clone, Copy)]
pub(crate) enum DspAccess {
    Inactive,
    Stopped,
}

pub(crate) unsafe fn access_dsp_state<P: PluginExport, T: 'static, R>(
    plugin: *const clap_plugin,
    access: DspAccess,
    callback: impl FnOnce(&mut T) -> R,
) -> Option<R> {
    if plugin.is_null() {
        return None;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: The caller guarantees the pointer and host lifecycle contract.
        let data = unsafe { data_from_plugin::<P>(plugin) };
        let unavailable = match access {
            DspAccess::Inactive => data.active.load(Ordering::Acquire),
            DspAccess::Stopped => data.processing.load(Ordering::Acquire),
        };
        if unavailable {
            return None;
        }
        let mut instance = data.plugin.try_enter()?;
        let mut callback = Some(callback);
        let mut result = None;
        instance.with_dsp_state(&mut |state| {
            if let (Some(state), Some(callback)) = (state.downcast_mut::<T>(), callback.take()) {
                result = Some(callback(state));
            }
        });
        result
    }))
    .ok()
    .flatten()
}
