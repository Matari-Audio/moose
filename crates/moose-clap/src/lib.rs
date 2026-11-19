//! CLAP format wrapper for the moose framework.
//!
//! Provides the [`export_clap!`] macro to expose any
//! `PluginExport` implementation as a CLAP plugin.

// Several `extern "C" fn`s in the CLAP vtable carry a `<P: PluginExport>`
// type parameter even though they don't use `P`. The vtable is built per-`P`
// inside the `export_clap!` macro and uniformity across the table simplifies
// the macro; removing `P` from individual entries would make the macro
// branch on which functions are generic.
#![allow(clippy::extra_unused_type_parameters)]
// CLAP event headers are 4-byte aligned but the extended event types
// (note, param_value, transport, …) require 8-byte alignment. The
// host guarantees the underlying buffer is allocated with the
// extended event's alignment; the `header.cast::<…>()` is reading
// through that promise.
#![allow(clippy::cast_ptr_alignment)]

use crate::data::ClapAudio;
use crate::data::ClapGui;
use crate::data::ClapPluginData;
use crate::data::GUI_QUEUE_CAPACITY;
use crate::data::GuiChangeQueue;
use crate::data::GuiMode;
use crate::events::SoundingNotes;
use crate::gui::Extensions;
use crate::gui::clap_plugin_get_extension;
use crate::plugin::clap_plugin_activate;
use crate::plugin::clap_plugin_deactivate;
use crate::plugin::clap_plugin_destroy;
use crate::plugin::clap_plugin_init;
use crate::plugin::clap_plugin_on_main_thread;
use crate::plugin::clap_plugin_reset;
use crate::plugin::clap_plugin_start_processing;
use crate::plugin::clap_plugin_stop_processing;
use crate::process::clap_plugin_process;
use crate::remote_controls::presented_remote_control_pages;
use crate::state::StateLoadQueue;
use crate::state_ext::request_presentation_refresh;

#[doc(hidden)]
pub mod __macro_deps {
    pub use moose_core;
}

/// The CLAP version moose implements. clap-sys 0.5 says 1.2.2; the stable
/// ABI is unchanged through 1.2.10 (only drafts, docs and two surround
/// constants were added), so the descriptors report the headers we track.
pub const CLAP_VERSION: clap_sys::version::clap_version = clap_sys::version::clap_version {
    major: 1,
    minor: 2,
    revision: 10,
};

mod catalog_sync;
mod context_menu;
mod data;
#[cfg(feature = "clap-draft")]
pub mod draft;
mod events;
mod gui;
mod note_name;
mod param_indication;
mod params_ext;
mod plugin;
mod ports;
pub mod presets;
mod process;
mod remote_controls;
mod state_ext;
mod track_info;
mod voice_info;
#[cfg(all(target_os = "linux", feature = "clap-draft"))]
mod wayland_nested;

pub use gui::{try_with_dsp_state, try_with_params, try_with_stopped_dsp_state};
pub use process::rt_paranoid_smoke;

// The inline test modules below `use super::*` and reach every split module.
#[cfg(test)]
use crate::{
    data::*, events::*, gui::*, params_ext::*, ports::*, process::*, remote_controls::*,
    state_ext::*,
};

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};

#[cfg(test)]
use clap_sys::ext::params::{
    CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_TEXT, CLAP_PARAM_RESCAN_VALUES,
};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::plugin_features::{
    CLAP_PLUGIN_FEATURE_ANALYZER, CLAP_PLUGIN_FEATURE_AUDIO_EFFECT, CLAP_PLUGIN_FEATURE_INSTRUMENT,
    CLAP_PLUGIN_FEATURE_NOTE_EFFECT, CLAP_PLUGIN_FEATURE_SYNTHESIZER, CLAP_PLUGIN_FEATURE_UTILITY,
};

use moose_core::TransportSlot;
use moose_core::bus_routing::{BusRouting, bus_layouts_fit_routing};
use moose_core::config::ProcessMode;
use moose_core::editor::SendPtr;
use moose_core::events::{EVENT_LIST_PREALLOC, EventList};
use moose_core::export::PluginExport;
use moose_core::info::{PluginCategory, PluginInfo, resolve_name_override};
use moose_core::main_thread::HostNotifier;
use moose_core::state;
use moose_core::thread::MainThreadCell;
use moose_core::wrapper::{PluginCell, copy_c_str, run_extern_callback_with, shared_plugin};
use moose_params::Params;

#[cfg(test)]
use moose_core as core;
#[cfg(test)]
use moose_params as params;
#[cfg(test)]
use moose_test_fixtures::host_value_fixture;

#[cfg(test)]
mod host_value_tests {
    use super::host_value_fixture::TestPlugin;
    use super::*;
    use moose_params::Params;
    use std::sync::atomic::{AtomicU32, Ordering};

    static RESCAN: AtomicU32 = AtomicU32::new(0);
    unsafe extern "C" fn rescan(_: *const clap_host, flags: u32) {
        RESCAN.fetch_or(flags, Ordering::Relaxed);
    }
    unsafe extern "C" fn extension(_: *const clap_host, id: *const c_char) -> *const c_void {
        static PARAMS: clap_host_params = clap_host_params {
            rescan: Some(rescan),
            clear: None,
            request_flush: None,
        };
        // SAFETY: host extensions are queried with a terminated CLAP identifier.
        if unsafe { CStr::from_ptr(id) } == CLAP_EXT_PARAMS {
            (&raw const PARAMS).cast::<c_void>()
        } else {
            ptr::null()
        }
    }

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "unchanged storage values must compare exactly"
    )]
    fn ffi_text_hooks_and_rescan_preserve_declared_slot_range() {
        // SAFETY: own the plugin, descriptor, host, and all callback buffers.
        unsafe {
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            host.get_extension = Some(extension);
            let descriptor = DescriptorHolder::new(&TestPlugin::info());
            let plugin = create_plugin_instance::<TestPlugin>(
                &raw const descriptor.descriptor,
                &raw const host,
            );
            assert!(!plugin.is_null());
            assert!(clap_plugin_init::<TestPlugin>(plugin));
            let params = data_from_plugin::<TestPlugin>(plugin).params_arc.clone();
            let mut info: clap_param_info = std::mem::zeroed();
            assert!(params_get_info::<TestPlugin>(plugin, 1, &raw mut info));
            assert_eq!((info.id, info.min_value, info.max_value), (1, 0., 1.));
            let normalized = 49. / 96.;
            params.set_plain(1, normalized);
            let mut stored = 0.;
            assert!(params_get_value::<TestPlugin>(plugin, 1, &raw mut stored));
            assert_eq!(stored, normalized);
            let mut buffer = [0_i8; 128];
            assert!(params_value_to_text::<TestPlugin>(
                plugin,
                1,
                stored,
                buffer.as_mut_ptr(),
                128
            ));
            assert_eq!(CStr::from_ptr(buffer.as_ptr()).to_str().unwrap(), "+1 st");
            let mut parsed = -999.;
            assert!(params_text_to_value::<TestPlugin>(
                plugin,
                1,
                c"+1 st".as_ptr(),
                &raw mut parsed
            ));
            assert_eq!(parsed, normalized);
            for invalid in [c"0.5", c"nan"] {
                assert!(!params_text_to_value::<TestPlugin>(
                    plugin,
                    1,
                    invalid.as_ptr(),
                    &raw mut parsed
                ));
            }
            assert!(params_text_to_value::<TestPlugin>(
                plugin,
                0,
                c"-6 dB".as_ptr(),
                &raw mut parsed
            ));
            assert_eq!(parsed, -6.);
            RESCAN.store(0, Ordering::Relaxed);
            params.rebound.store(true, Ordering::Relaxed);
            params.generation.fetch_add(1, Ordering::Relaxed);
            clap_plugin_on_main_thread::<TestPlugin>(plugin);
            assert_eq!(
                RESCAN.load(Ordering::Relaxed),
                CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_TEXT
            );
            assert!(params_value_to_text::<TestPlugin>(
                plugin,
                1,
                stored,
                buffer.as_mut_ptr(),
                128
            ));
            assert_eq!(CStr::from_ptr(buffer.as_ptr()).to_str().unwrap(), "51%");
            assert!(params_text_to_value::<TestPlugin>(
                plugin,
                1,
                c"25%".as_ptr(),
                &raw mut parsed
            ));
            assert_eq!(parsed, 0.25);
            assert_eq!(params.get_plain(1), Some(normalized));
            clap_plugin_destroy::<TestPlugin>(plugin);
        }
    }

    static PUSHED: AtomicU32 = AtomicU32::new(0);
    unsafe extern "C" fn accept(_: *const clap_output_events, _: *const clap_event_header) -> bool {
        PUSHED.fetch_add(1, Ordering::Relaxed);
        true
    }
    unsafe extern "C" fn refuse(_: *const clap_output_events, _: *const clap_event_header) -> bool {
        false
    }

    #[test]
    #[allow(
        unused_assignments,
        reason = "the ABI reader follows ctx after each event write"
    )]
    fn structural_flush_and_gui_edits_request_discovery_without_numeric_rescans() {
        #[derive(Default)]
        struct Calls {
            callbacks: AtomicU32,
            rescans: AtomicU32,
        }
        unsafe extern "C" fn request(host: *const clap_host) {
            // SAFETY: the test retains Calls until after plugin destruction.
            let calls = unsafe { &*((*host).host_data.cast::<Calls>()) };
            calls.callbacks.fetch_add(1, Ordering::Relaxed);
        }
        unsafe extern "C" fn rescan(host: *const clap_host, flags: u32) {
            // SAFETY: same owned host/counter lifetime as request.
            let calls = unsafe { &*((*host).host_data.cast::<Calls>()) };
            calls.rescans.fetch_or(flags, Ordering::Relaxed);
        }
        unsafe extern "C" fn extension(_: *const clap_host, name: *const c_char) -> *const c_void {
            static PARAMS: clap_host_params = clap_host_params {
                rescan: Some(rescan),
                clear: None,
                request_flush: None,
            };
            // SAFETY: the wrapper supplies a terminated CLAP identifier.
            if unsafe { CStr::from_ptr(name) } == CLAP_EXT_PARAMS {
                (&raw const PARAMS).cast()
            } else {
                ptr::null()
            }
        }
        unsafe extern "C" fn count(_: *const clap_input_events) -> u32 {
            1
        }
        unsafe extern "C" fn event(
            events: *const clap_input_events,
            _: u32,
        ) -> *const clap_event_header {
            // SAFETY: ctx points to the value event retained through flush.
            unsafe { (*events).ctx.cast() }
        }
        // SAFETY: this test owns every ABI pointer and destroys the instance
        // before its descriptor, host, counters and event buffers go away.
        unsafe {
            let calls = Calls::default();
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            host.host_data = (&raw const calls).cast_mut().cast();
            host.get_extension = Some(extension);
            host.request_callback = Some(request);
            let descriptor = DescriptorHolder::new(&TestPlugin::info());
            let plugin = create_plugin_instance::<TestPlugin>(
                &raw const descriptor.descriptor,
                &raw const host,
            );
            assert!(clap_plugin_init::<TestPlugin>(plugin));
            let params = data_from_plugin::<TestPlugin>(plugin).params_arc.clone();
            params.set_plain(0, -60.);
            params.structural_gain.store(true, Ordering::Relaxed);
            clap_plugin_on_main_thread::<TestPlugin>(plugin);
            let mut value = clap_event_param_value {
                header: clap_event_header {
                    size: size_of_u32::<clap_event_param_value>(),
                    time: 0,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_PARAM_VALUE,
                    flags: 0,
                },
                param_id: 1,
                cookie: ptr::null_mut(),
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value: 0.25,
            };
            let inputs = clap_input_events {
                ctx: (&raw mut value).cast(),
                size: Some(count),
                get: Some(event),
            };
            for _ in 0..128 {
                params_flush::<TestPlugin>(plugin, &raw const inputs, ptr::null());
            }
            assert_eq!(calls.callbacks.load(Ordering::Relaxed), 0);
            assert_eq!(calls.rescans.load(Ordering::Relaxed), 0);
            value.param_id = 0;
            value.value = -12.;
            params_flush::<TestPlugin>(plugin, &raw const inputs, ptr::null());
            value.value = -6.;
            params_flush::<TestPlugin>(plugin, &raw const inputs, ptr::null());
            assert_eq!(
                calls.callbacks.load(Ordering::Relaxed),
                1,
                "coalesced without metadata polling"
            );
            assert_eq!(
                calls.rescans.load(Ordering::Relaxed),
                0,
                "host notification waits for main"
            );
            clap_plugin_on_main_thread::<TestPlugin>(plugin);
            assert!(params.rebound.load(Ordering::Relaxed));
            assert_eq!(
                calls.rescans.swap(0, Ordering::Relaxed),
                CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_TEXT
            );
            let context = editor_context::<TestPlugin>(plugin);
            context.set_param(0_u32, 0.);
            assert_eq!(calls.callbacks.load(Ordering::Relaxed), 2);
            clap_plugin_on_main_thread::<TestPlugin>(plugin);
            assert!(!params.rebound.load(Ordering::Relaxed));
            assert_eq!(
                calls.rescans.load(Ordering::Relaxed),
                CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_TEXT
            );
            drop(context);
            clap_plugin_destroy::<TestPlugin>(plugin);
        }
    }

    /// GUI values reach the host as events. A values rescan is only the
    /// fallback for a value the host refused, never a per-edit request.
    #[test]
    fn gui_values_flush_as_events_and_rescan_only_when_refused() {
        // SAFETY: own the plugin, descriptor, host, and output-event shims.
        unsafe {
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            host.get_extension = Some(extension);
            let descriptor = DescriptorHolder::new(&TestPlugin::info());
            let plugin = create_plugin_instance::<TestPlugin>(
                &raw const descriptor.descriptor,
                &raw const host,
            );
            assert!(clap_plugin_init::<TestPlugin>(plugin));
            let data = data_from_plugin::<TestPlugin>(plugin);
            for value in [0.25, 0.5] {
                data.gui_changes
                    .push(GuiParamChange::Value(1, value))
                    .unwrap_or_else(|_| panic!("queue full"));
            }
            let out = clap_output_events {
                ctx: ptr::null_mut(),
                try_push: Some(accept),
            };
            PUSHED.store(0, Ordering::Relaxed);
            assert!(flush_gui_changes::<TestPlugin>(data, &raw const out));
            assert_eq!(PUSHED.load(Ordering::Relaxed), 2);
            assert!(!data.needs_rescan.load(Ordering::Relaxed));
            data.gui_changes
                .push(GuiParamChange::Value(1, 0.75))
                .unwrap_or_else(|_| panic!("queue full"));
            let full = clap_output_events {
                ctx: ptr::null_mut(),
                try_push: Some(refuse),
            };
            assert!(!flush_gui_changes::<TestPlugin>(data, &raw const full));
            assert!(data.needs_rescan.load(Ordering::Relaxed));
            clap_plugin_destroy::<TestPlugin>(plugin);
        }
    }

    // Exercise the actual editor bridge as well as the queue-draining helper.
    // A fake X11 parent is safe because this editor never calls a window API.
    #[cfg(target_os = "linux")]
    #[test]
    fn gui_set_param_keeps_rescans_fallback_only_and_epochs_instance_local() {
        use moose_core::editor::{Editor, PluginContext, RawWindowHandle};
        use std::sync::Mutex;

        struct CaptureEditor(Arc<Mutex<Option<PluginContext>>>);
        struct DefaultScaleEditor;
        impl Editor for DefaultScaleEditor {
            fn size(&self) -> (u32, u32) {
                (100, 100)
            }
            fn open(&mut self, _: RawWindowHandle, _: PluginContext) {}
            fn close(&mut self) {}
        }
        impl Editor for CaptureEditor {
            fn size(&self) -> (u32, u32) {
                (100, 100)
            }
            fn open(&mut self, _: RawWindowHandle, context: PluginContext) {
                *self.0.lock().unwrap() = Some(context);
            }
            fn close(&mut self) {}
        }
        #[derive(Default)]
        struct Calls {
            callbacks: AtomicU32,
            rescans: AtomicU32,
            flushes: AtomicU32,
        }
        unsafe fn calls<'a>(host: *const clap_host) -> &'a Calls {
            // SAFETY: each shim receives the live host owned below.
            unsafe { &*((*host).host_data.cast::<Calls>()) }
        }
        unsafe extern "C" fn callback(host: *const clap_host) {
            unsafe { calls(host) }
                .callbacks
                .fetch_add(1, Ordering::Relaxed);
        }
        unsafe extern "C" fn flush(host: *const clap_host) {
            unsafe { calls(host) }
                .flushes
                .fetch_add(1, Ordering::Relaxed);
        }
        unsafe extern "C" fn rescan(host: *const clap_host, flags: u32) {
            assert_eq!(flags, CLAP_PARAM_RESCAN_VALUES);
            unsafe { calls(host) }
                .rescans
                .fetch_add(1, Ordering::Relaxed);
        }
        unsafe extern "C" fn extension(_: *const clap_host, id: *const c_char) -> *const c_void {
            static PARAMS: clap_host_params = clap_host_params {
                rescan: Some(rescan),
                clear: None,
                request_flush: Some(flush),
            };
            if unsafe { CStr::from_ptr(id) } == CLAP_EXT_PARAMS {
                (&raw const PARAMS).cast()
            } else {
                ptr::null()
            }
        }
        unsafe extern "C" fn accept(
            _: *const clap_output_events,
            _: *const clap_event_header,
        ) -> bool {
            true
        }

        // SAFETY: all host/plugin/window/event pointers remain live until destruction.
        unsafe {
            let calls = Calls::default();
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            host.host_data = (&raw const calls).cast_mut().cast();
            host.get_extension = Some(extension);
            host.request_callback = Some(callback);
            let descriptor = DescriptorHolder::new(&TestPlugin::info());
            let a = create_plugin_instance::<TestPlugin>(
                &raw const descriptor.descriptor,
                &raw const host,
            );
            let b = create_plugin_instance::<TestPlugin>(
                &raw const descriptor.descriptor,
                &raw const host,
            );
            assert!(clap_plugin_init::<TestPlugin>(a));
            assert!(clap_plugin_init::<TestPlugin>(b));
            let data = data_from_plugin::<TestPlugin>(a);
            let other = data_from_plugin::<TestPlugin>(b);
            data.gui.enter().editor = Some(Box::new(DefaultScaleEditor));
            let gui =
                &*((*a).get_extension.unwrap()(a, CLAP_EXT_GUI.as_ptr()).cast::<clap_plugin_gui>());
            assert!(!gui.create.unwrap()(a, ptr::null(), false));
            assert!(gui.create.unwrap()(a, CLAP_WINDOW_API_X11.as_ptr(), false));
            let mut window: clap_window = std::mem::zeroed();
            window.api = CLAP_WINDOW_API_X11.as_ptr();
            window.specific.ptr = ptr::dangling_mut::<c_void>();
            assert!(!gui.set_parent.unwrap()(a, ptr::null()));
            assert!(gui.set_parent.unwrap()(a, &raw const window));
            gui.destroy.unwrap()(a);
            let captured = Arc::new(Mutex::new(None));
            data.gui.enter().editor = Some(Box::new(CaptureEditor(captured.clone())));
            assert!(gui_create::<TestPlugin>(
                a,
                CLAP_WINDOW_API_X11.as_ptr(),
                false
            ));
            assert!(gui_set_parent::<TestPlugin>(a, &raw const window));
            let context = captured.lock().unwrap().take().unwrap();
            let out = clap_output_events {
                ctx: ptr::null_mut(),
                try_push: Some(accept),
            };
            let epoch = data.params_arc.parameter_edit_epoch();
            let foreign_epoch = other.params_arc.parameter_edit_epoch();
            for i in 0..128 {
                context.set_param(1_u32, if i % 2 == 0 { 0.25 } else { 0.75 });
                assert!(flush_gui_changes::<TestPlugin>(data, &raw const out));
            }
            assert_eq!(data.params_arc.parameter_edit_epoch(), epoch + 128);
            assert_eq!(other.params_arc.parameter_edit_epoch(), foreign_epoch);
            assert_eq!(calls.flushes.load(Ordering::Relaxed), 128);
            assert_eq!(calls.callbacks.load(Ordering::Relaxed), 0);
            clap_plugin_on_main_thread::<TestPlugin>(a);
            assert_eq!(calls.rescans.load(Ordering::Relaxed), 0);

            context.set_param(1_u32, 0.5);
            let full = clap_output_events {
                ctx: ptr::null_mut(),
                try_push: Some(refuse),
            };
            assert!(!flush_gui_changes::<TestPlugin>(data, &raw const full));
            assert_eq!(calls.callbacks.load(Ordering::Relaxed), 1);
            clap_plugin_on_main_thread::<TestPlugin>(a);
            assert_eq!(calls.rescans.load(Ordering::Relaxed), 1);

            for _ in 0..GUI_QUEUE_CAPACITY + 2 {
                context.set_param(1_u32, 0.25);
            }
            assert_eq!(
                calls.callbacks.load(Ordering::Relaxed),
                2,
                "overflow callbacks coalesce"
            );
            assert!(flush_gui_changes::<TestPlugin>(data, &raw const out));
            clap_plugin_on_main_thread::<TestPlugin>(a);
            assert_eq!(calls.rescans.load(Ordering::Relaxed), 2);
            assert_eq!(other.params_arc.parameter_edit_epoch(), foreign_epoch);
            drop(context);
            clap_plugin_destroy::<TestPlugin>(a);
            clap_plugin_destroy::<TestPlugin>(b);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod floating_gui_tests {
    use super::host_value_fixture::TestPlugin;
    use super::*;
    type Close = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;
    struct Probe(Close);
    impl Editor for Probe {
        fn size(&self) -> (u32, u32) {
            (320, 240)
        }
        fn open(&mut self, _: RawWindowHandle, _: PluginContext) {
            panic!("must not embed Wayland");
        }
        fn close(&mut self) {
            if let Some(closed) = self.0.lock().unwrap().take() {
                closed();
            }
        }
        fn supports_floating_x11(&self) -> bool {
            true
        }
        fn open_floating_x11(
            &mut self,
            _: PluginContext,
            _: &str,
            closed: Arc<dyn Fn() + Send + Sync>,
        ) -> bool {
            *self.0.lock().unwrap() = Some(closed);
            true
        }
        fn supports_floating_wayland(&self) -> bool {
            true
        }
        fn open_floating_wayland(
            &mut self,
            _: PluginContext,
            _: &str,
            closed: Arc<dyn Fn() + Send + Sync>,
        ) -> bool {
            *self.0.lock().unwrap() = Some(closed);
            true
        }
    }
    unsafe extern "C" fn request(host: *const clap_host) {
        // SAFETY: test retains its two counters through plugin destruction.
        unsafe {
            (&*(*host).host_data.cast::<[AtomicU32; 2]>())[0].fetch_add(1, Ordering::Relaxed);
        }
    }
    unsafe extern "C" fn closed(host: *const clap_host, destroyed: bool) {
        assert!(!destroyed);
        // SAFETY: test retains its two counters through plugin destruction.
        unsafe {
            (&*(*host).host_data.cast::<[AtomicU32; 2]>())[1].fetch_add(1, Ordering::Relaxed);
        }
    }
    unsafe extern "C" fn extension(_: *const clap_host, name: *const c_char) -> *const c_void {
        static GUI: clap_host_gui = clap_host_gui {
            resize_hints_changed: None,
            request_resize: None,
            request_show: None,
            request_hide: None,
            closed: Some(closed),
        };
        // SAFETY: CLAP passes a valid extension name.
        if unsafe { CStr::from_ptr(name) } == CLAP_EXT_GUI {
            (&raw const GUI).cast()
        } else {
            ptr::null()
        }
    }
    #[test]
    fn wayland_negotiation_and_close_work_with_a_deferred_host() {
        let counters = [AtomicU32::new(0), AtomicU32::new(0)];
        // SAFETY: own host, descriptor, plugin and fake editor; callbacks retain
        // no native resources and all borrowed test state outlives the instance.
        unsafe {
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            host.host_data = (&raw const counters).cast_mut().cast();
            host.get_extension = Some(extension);
            host.request_callback = Some(request);
            let descriptor = DescriptorHolder::new(&TestPlugin::info());
            let plugin = create_plugin_instance::<TestPlugin>(
                &raw const descriptor.descriptor,
                &raw const host,
            );
            assert!(clap_plugin_init::<TestPlugin>(plugin));
            let data = data_from_plugin::<TestPlugin>(plugin);
            assert!(!gui_is_api_supported::<TestPlugin>(
                plugin,
                CLAP_WINDOW_API_WAYLAND.as_ptr(),
                true
            ));
            let hook: Close = Arc::default();
            data.gui.enter().editor = Some(Box::new(Probe(hook.clone())));
            assert!(gui_is_api_supported::<TestPlugin>(
                plugin,
                CLAP_WINDOW_API_WAYLAND.as_ptr(),
                true
            ));
            assert!(!gui_is_api_supported::<TestPlugin>(
                plugin,
                CLAP_WINDOW_API_WAYLAND.as_ptr(),
                false
            ));
            assert!(gui_is_api_supported::<TestPlugin>(
                plugin,
                CLAP_WINDOW_API_X11.as_ptr(),
                false
            ));
            assert!(gui_is_api_supported::<TestPlugin>(
                plugin,
                CLAP_WINDOW_API_X11.as_ptr(),
                true
            ));
            assert!(!gui_create::<TestPlugin>(
                plugin,
                CLAP_WINDOW_API_WAYLAND.as_ptr(),
                false
            ));
            assert!(!gui_create::<TestPlugin>(plugin, ptr::null(), true));
            assert!(gui_create::<TestPlugin>(
                plugin,
                CLAP_WINDOW_API_WAYLAND.as_ptr(),
                true
            ));
            assert!(!gui_create::<TestPlugin>(
                plugin,
                CLAP_WINDOW_API_X11.as_ptr(),
                true
            ));
            assert!(!gui_set_scale::<TestPlugin>(plugin, 2.));
            assert!(gui_show::<TestPlugin>(plugin));
            let close = hook.lock().unwrap().take().unwrap();
            close();
            assert_eq!(counters[0].load(Ordering::Relaxed), 1);
            assert_eq!(counters[1].load(Ordering::Relaxed), 0);
            clap_plugin_on_main_thread::<TestPlugin>(plugin);
            assert_eq!(counters[1].load(Ordering::Relaxed), 1);
            assert!(gui_show::<TestPlugin>(plugin));
            assert!(gui_hide::<TestPlugin>(plugin));
            assert_eq!(
                counters[0].load(Ordering::Relaxed),
                1,
                "programmatic hide must not notify close"
            );
            gui_destroy::<TestPlugin>(plugin);
            data.gui.enter().editor = Some(Box::new(Probe(hook.clone())));
            assert!(gui_create::<TestPlugin>(
                plugin,
                CLAP_WINDOW_API_X11.as_ptr(),
                true
            ));
            assert!(gui_show::<TestPlugin>(plugin));
            assert!(gui_hide::<TestPlugin>(plugin));
            assert!(gui_show::<TestPlugin>(plugin));
            hook.lock().unwrap().take().unwrap()();
            clap_plugin_on_main_thread::<TestPlugin>(plugin);
            assert_eq!(counters[1].load(Ordering::Relaxed), 2);
            gui_destroy::<TestPlugin>(plugin);
            clap_plugin_destroy::<TestPlugin>(plugin);
        }
    }
}

#[cfg(test)]
mod inactive_recall_tests {
    use super::host_value_fixture::TestPlugin;
    use super::*;
    struct Stream {
        bytes: Vec<u8>,
        offset: usize,
    }
    unsafe extern "C" fn write(stream: *const clap_ostream, bytes: *const c_void, len: u64) -> i64 {
        // SAFETY: the test retains its stream storage and CLAP supplies len bytes.
        unsafe {
            let stream = &mut *(*stream).ctx.cast::<Stream>();
            stream.bytes.extend_from_slice(std::slice::from_raw_parts(
                bytes.cast(),
                usize::try_from(len).expect("test buffer length fits usize"),
            ));
            i64::try_from(len).expect("test byte count fits i64")
        }
    }
    unsafe extern "C" fn read(stream: *const clap_istream, bytes: *mut c_void, len: u64) -> i64 {
        // SAFETY: the test retains its stream storage; CLAP supplies len writable bytes.
        unsafe {
            let stream = &mut *(*stream).ctx.cast::<Stream>();
            let len = (usize::try_from(len).expect("test buffer length fits usize"))
                .min(stream.bytes.len() - stream.offset);
            ptr::copy_nonoverlapping(stream.bytes.as_ptr().add(stream.offset), bytes.cast(), len);
            stream.offset += len;
            i64::try_from(len).expect("test byte count fits i64")
        }
    }
    unsafe extern "C" fn no_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
        ptr::null()
    }
    #[test]
    fn inactive_recall_discards_a_stopped_transport_flush() {
        // SAFETY: own the plugin, host, descriptor and stream through destruction.
        unsafe {
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            host.get_extension = Some(no_extension);
            let descriptor = DescriptorHolder::new(&TestPlugin::info());
            let plugin = create_plugin_instance::<TestPlugin>(
                &raw const descriptor.descriptor,
                &raw const host,
            );
            assert!(clap_plugin_init::<TestPlugin>(plugin));
            let data = data_from_plugin::<TestPlugin>(plugin);
            let original = data.params_arc.get_plain(0).unwrap();
            let mut storage = Stream {
                bytes: Vec::new(),
                offset: 0,
            };
            let output = clap_ostream {
                ctx: (&raw mut storage).cast(),
                write: Some(write),
            };
            assert!(state_save::<TestPlugin>(plugin, &raw const output));
            data.params_arc.set_plain(0, 6.);
            defer_param_change(&mut data.audio.enter().deferred_params, 0, 6.);
            let input = clap_istream {
                ctx: (&raw mut storage).cast(),
                read: Some(read),
            };
            assert!(state_load::<TestPlugin>(plugin, &raw const input));
            assert_eq!(data.params_arc.get_plain(0), Some(original));
            assert!(
                data.audio.enter().deferred_params.is_empty(),
                "stale flush would overwrite recall on the next block"
            );
            clap_plugin_destroy::<TestPlugin>(plugin);
        }
    }
}

#[cfg(test)]
use clap_sys::audio_buffer::clap_audio_buffer;
#[cfg(test)]
use clap_sys::events::CLAP_CORE_EVENT_SPACE_ID;
#[cfg(test)]
use clap_sys::events::CLAP_EVENT_PARAM_VALUE;
#[cfg(test)]
use clap_sys::events::CLAP_NOTE_EXPRESSION_TUNING;
#[cfg(test)]
use clap_sys::events::CLAP_NOTE_EXPRESSION_VOLUME;
#[cfg(test)]
use clap_sys::events::CLAP_TRANSPORT_HAS_SECONDS_TIMELINE;
#[cfg(test)]
use clap_sys::events::clap_event_header;
#[cfg(test)]
use clap_sys::events::clap_event_note;
#[cfg(test)]
use clap_sys::events::clap_event_note_expression;
#[cfg(test)]
use clap_sys::events::clap_event_param_value;
#[cfg(test)]
use clap_sys::events::clap_event_transport;
#[cfg(test)]
use clap_sys::events::clap_input_events;
#[cfg(test)]
use clap_sys::events::clap_output_events;
#[cfg(test)]
use clap_sys::ext::audio_ports::CLAP_AUDIO_PORT_IS_MAIN;
#[cfg(test)]
use clap_sys::ext::audio_ports::CLAP_AUDIO_PORTS_RESCAN_LIST;
#[cfg(test)]
use clap_sys::ext::audio_ports::CLAP_EXT_AUDIO_PORTS;
#[cfg(test)]
use clap_sys::ext::audio_ports::CLAP_PORT_MONO;
#[cfg(test)]
use clap_sys::ext::audio_ports::CLAP_PORT_STEREO;
#[cfg(test)]
use clap_sys::ext::audio_ports::clap_audio_port_info;
#[cfg(test)]
use clap_sys::ext::audio_ports::clap_host_audio_ports;
#[cfg(test)]
use clap_sys::ext::audio_ports_activation::CLAP_EXT_AUDIO_PORTS_ACTIVATION;
#[cfg(test)]
use clap_sys::ext::audio_ports_activation::clap_plugin_audio_ports_activation;
#[cfg(test)]
use clap_sys::ext::configurable_audio_ports::CLAP_EXT_CONFIGURABLE_AUDIO_PORTS;
#[cfg(test)]
use clap_sys::ext::configurable_audio_ports::clap_audio_port_configuration_request;
#[cfg(test)]
use clap_sys::ext::configurable_audio_ports::clap_plugin_configurable_audio_ports;
#[cfg(test)]
use clap_sys::ext::context_menu::CLAP_EXT_CONTEXT_MENU;
#[cfg(test)]
use clap_sys::ext::context_menu::clap_plugin_context_menu;
#[cfg(test)]
use clap_sys::ext::gui::CLAP_EXT_GUI;
#[cfg(test)]
use clap_sys::ext::gui::CLAP_WINDOW_API_WAYLAND;
#[cfg(test)]
use clap_sys::ext::gui::CLAP_WINDOW_API_X11;
#[cfg(test)]
use clap_sys::ext::gui::clap_host_gui;
#[cfg(test)]
use clap_sys::ext::gui::clap_plugin_gui;
#[cfg(test)]
use clap_sys::ext::gui::clap_window;
#[cfg(test)]
use clap_sys::ext::latency::CLAP_EXT_LATENCY;
#[cfg(test)]
use clap_sys::ext::latency::clap_host_latency;
#[cfg(test)]
use clap_sys::ext::note_name::CLAP_EXT_NOTE_NAME;
#[cfg(test)]
use clap_sys::ext::note_name::clap_plugin_note_name;
#[cfg(test)]
use clap_sys::ext::note_ports::CLAP_EXT_NOTE_PORTS;
#[cfg(test)]
use clap_sys::ext::note_ports::CLAP_NOTE_DIALECT_CLAP;
#[cfg(test)]
use clap_sys::ext::note_ports::clap_note_port_info;
#[cfg(test)]
use clap_sys::ext::note_ports::clap_plugin_note_ports;
#[cfg(test)]
use clap_sys::ext::param_indication::CLAP_EXT_PARAM_INDICATION;
#[cfg(test)]
use clap_sys::ext::param_indication::clap_plugin_param_indication;
#[cfg(test)]
use clap_sys::ext::params::CLAP_EXT_PARAMS;
#[cfg(test)]
use clap_sys::ext::params::clap_host_params;
#[cfg(test)]
use clap_sys::ext::params::clap_param_info;
#[cfg(test)]
use clap_sys::ext::remote_controls::CLAP_EXT_REMOTE_CONTROLS;
#[cfg(test)]
use clap_sys::ext::remote_controls::CLAP_EXT_REMOTE_CONTROLS_COMPAT;
#[cfg(test)]
use clap_sys::ext::remote_controls::clap_plugin_remote_controls;
#[cfg(test)]
use clap_sys::ext::remote_controls::clap_remote_controls_page;
#[cfg(test)]
use clap_sys::ext::state::CLAP_EXT_STATE;
#[cfg(test)]
use clap_sys::ext::state::clap_host_state;
#[cfg(test)]
use clap_sys::ext::surround::CLAP_EXT_SURROUND;
#[cfg(test)]
use clap_sys::ext::surround::CLAP_PORT_SURROUND;
#[cfg(test)]
use clap_sys::ext::surround::clap_plugin_surround;
#[cfg(test)]
use clap_sys::ext::track_info::CLAP_EXT_TRACK_INFO;
#[cfg(test)]
use clap_sys::ext::track_info::clap_host_track_info;
#[cfg(test)]
use clap_sys::ext::track_info::clap_plugin_track_info;
#[cfg(test)]
use clap_sys::ext::voice_info::CLAP_EXT_VOICE_INFO;
#[cfg(test)]
use clap_sys::ext::voice_info::clap_plugin_voice_info;
#[cfg(test)]
use clap_sys::fixedpoint::CLAP_SECTIME_FACTOR;
#[cfg(test)]
use clap_sys::id::clap_id;
#[cfg(test)]
use clap_sys::process::CLAP_PROCESS_CONTINUE;
#[cfg(test)]
use clap_sys::process::CLAP_PROCESS_ERROR;
#[cfg(test)]
use clap_sys::process::clap_process;
#[cfg(test)]
use clap_sys::stream::clap_istream;
#[cfg(test)]
use clap_sys::stream::clap_ostream;
#[cfg(test)]
use moose_core::buffer::AudioBuffer;
#[cfg(test)]
use moose_core::bus::BusKind;
#[cfg(test)]
use moose_core::bus::ChannelConfig;
#[cfg(test)]
use moose_core::bus_routing::BusActivation;
#[cfg(test)]
use moose_core::cast::len_u32;
#[cfg(test)]
use moose_core::cast::size_of_u32;
#[cfg(test)]
use moose_core::config::AudioConfig;
#[cfg(test)]
use moose_core::editor::Editor;
#[cfg(test)]
use moose_core::editor::PluginContext;
#[cfg(test)]
use moose_core::editor::RawWindowHandle;
#[cfg(test)]
use moose_core::events::Event;
#[cfg(test)]
use moose_core::events::EventBody;
#[cfg(test)]
use moose_core::main_thread::MainThreadHooks;
#[cfg(test)]
use moose_core::main_thread::NoteName;
#[cfg(test)]
use moose_core::meters::MeterStore;
#[cfg(test)]
use moose_core::plugin::PluginRuntime;
#[cfg(test)]
use moose_core::process::ProcessStatus;
#[cfg(test)]
use moose_core::rt::RtSection;
#[cfg(test)]
use moose_core::snapshot::SnapshotSlot;
#[cfg(test)]
use moose_core::thread::MainThread;
#[cfg(test)]
use moose_core::wrapper::enter_plugin;
#[cfg(test)]
use moose_params::ParamFlags;
#[cfg(test)]
use moose_params::ParamInfo;
#[cfg(test)]
use moose_params::ParamRange;
#[cfg(test)]
use moose_test_fixtures::large_state_fixture;
#[cfg(test)]
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// Descriptor management
// ---------------------------------------------------------------------------

/// Holds all the C strings and the descriptor itself. Lives for the process
/// lifetime via a `static` produced by the macro.
pub struct DescriptorHolder {
    pub descriptor: clap_plugin_descriptor,
    // Prevent dropping CStrings that the descriptor points into.
    _id: CString,
    _name: CString,
    _vendor: CString,
    _url: CString,
    _manual_url: Option<CString>,
    _support_url: Option<CString>,
    _version: CString,
    _description: Option<CString>,
    _features: Vec<*const c_char>,
    _features_storage: Vec<CString>,
}

unsafe impl Send for DescriptorHolder {}
unsafe impl Sync for DescriptorHolder {}

/// Plugin display-name in host browsers. Reads `moose.toml`'s
/// `clap_name` (baked into `PluginInfo` by `moose::plugin_info!`),
/// falling back to `PluginInfo::name`.
fn resolved_name(info: &PluginInfo) -> &'static str {
    resolve_name_override(info.clap_name, info.name)
}

impl DescriptorHolder {
    #[must_use]
    pub fn new(info: &PluginInfo) -> Self {
        let id = CString::new(info.clap_id).unwrap_or_default();
        let name = CString::new(resolved_name(info)).unwrap_or_default();
        let vendor = CString::new(info.vendor).unwrap_or_default();
        let url = CString::new(info.url).unwrap_or_default();
        let manual_url = info
            .clap_manual_url
            .and_then(|value| CString::new(value).ok());
        let support_url = info
            .clap_support_url
            .and_then(|value| CString::new(value).ok());
        let version = CString::new(info.version).unwrap_or_default();
        let description = info.description.and_then(|value| CString::new(value).ok());

        let default_features: &[&CStr] = match info.category {
            PluginCategory::Instrument => &[
                CLAP_PLUGIN_FEATURE_INSTRUMENT,
                CLAP_PLUGIN_FEATURE_SYNTHESIZER,
            ],
            PluginCategory::NoteEffect => &[CLAP_PLUGIN_FEATURE_NOTE_EFFECT],
            PluginCategory::Effect => &[CLAP_PLUGIN_FEATURE_AUDIO_EFFECT],
            // Analyzer / Tool still process audio (passthrough), so keep
            // AUDIO_EFFECT for insert menus, but lead with the specific
            // feature so a feature-filtered browser surfaces them.
            PluginCategory::Analyzer => &[
                CLAP_PLUGIN_FEATURE_ANALYZER,
                CLAP_PLUGIN_FEATURE_AUDIO_EFFECT,
            ],
            PluginCategory::Tool => &[
                CLAP_PLUGIN_FEATURE_UTILITY,
                CLAP_PLUGIN_FEATURE_AUDIO_EFFECT,
            ],
        };
        let configured_features = (!info.clap_features.is_empty()).then(|| {
            info.clap_features
                .iter()
                .map(|feature| CString::new(*feature))
                .collect::<Result<Vec<_>, _>>()
                .ok()
        });
        let features_storage = configured_features.flatten().unwrap_or_else(|| {
            default_features
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect()
        });

        let mut features: Vec<*const c_char> =
            features_storage.iter().map(|f| f.as_ptr()).collect();
        features.push(ptr::null());

        let descriptor = clap_plugin_descriptor {
            clap_version: CLAP_VERSION,
            id: id.as_ptr(),
            name: name.as_ptr(),
            vendor: vendor.as_ptr(),
            url: url.as_ptr(),
            manual_url: manual_url
                .as_ref()
                .map_or(ptr::null(), |value| value.as_ptr()),
            support_url: support_url
                .as_ref()
                .map_or(url.as_ptr(), |value| value.as_ptr()),
            version: version.as_ptr(),
            description: description
                .as_ref()
                .map_or(ptr::null(), |value| value.as_ptr()),
            features: features.as_ptr(),
        };

        Self {
            descriptor,
            _id: id,
            _name: name,
            _vendor: vendor,
            _url: url,
            _manual_url: manual_url,
            _support_url: support_url,
            _version: version,
            _description: description,
            _features: features,
            _features_storage: features_storage,
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: copy a Rust &str into a fixed-size [c_char; N] array
// ---------------------------------------------------------------------------

/// Copy `src` into the fixed-size C-string array `dst` (name / vendor /
/// module buffers), truncating on a UTF-8 char boundary and NUL-terminating.
/// Thin slice-typed front for [`copy_c_str`] so the truncation rule lives in
/// one place.
fn copy_str_to_buf(dst: &mut [c_char], src: &str) {
    debug_assert!(!dst.is_empty(), "copy_str_to_buf needs room for the NUL");
    // SAFETY: `dst` is a valid, non-empty slice of `dst.len()` `c_char`.
    unsafe {
        let _ = copy_c_str(dst.as_mut_ptr(), dst.len(), src);
    }
}

// ---------------------------------------------------------------------------
// Helper: get &mut ClapPluginData<P> from a *const clap_plugin
// ---------------------------------------------------------------------------

unsafe fn data_from_plugin<P: PluginExport>(
    plugin: *const clap_plugin,
) -> &'static ClapPluginData<P> {
    unsafe { &*(*plugin).plugin_data.cast::<ClapPluginData<P>>() }
}

// ---------------------------------------------------------------------------
// Factory: create_plugin
// ---------------------------------------------------------------------------

/// Create a `clap_plugin` instance for the given plugin type.
///
/// # Safety
/// Called by the host through the factory. The descriptor must remain valid
/// for the lifetime of the returned plugin.
#[must_use]
pub unsafe fn create_plugin_instance<P: PluginExport>(
    descriptor: *const clap_plugin_descriptor,
    host: *const clap_host,
) -> *const clap_plugin {
    // Author `create` runs here (its `init` is guarded separately in
    // `clap_plugin_init`). Guard the constructor too - the factory's
    // extern "C" caller is bare, so a panic would abort the host. A null
    // return tells the host construction failed.
    run_extern_callback_with::<P, *const clap_plugin>("CLAP", "create", ptr::null(), || {
        let layouts = P::bus_layouts();
        if !bus_layouts_fit_routing(&layouts) {
            eprintln!(
                "[moose CLAP] {} declares an audio-bus topology beyond BusRouting's limit of 32 \
                 buses per direction and 65,535 channels per bus - instance creation refused.",
                std::any::type_name::<P>(),
            );
            return ptr::null();
        }
        let instance = P::create();
        let info = P::info();
        let plugin_id_hash = state::hash_plugin_id(info.clap_id);
        let param_infos = instance.params().param_catalog();
        let param_count = param_infos.len();
        let params_arc = instance.params_arc();
        let hooks = MainThreadCell::new(instance.main_thread());
        let presentation_pending = Arc::new(AtomicBool::new(false));
        let host_notifier = HostNotifier::new({
            let pending = Arc::clone(&presentation_pending);
            // SAFETY: the host outlives the instance, and its
            // `request_callback` is [thread-safe] (host.h).
            let host = unsafe { SendPtr::new(host) };
            move || {
                // SAFETY: `destroy` closes the notifier before the instance
                // and its host pointer go away.
                unsafe { request_presentation_refresh(&pending, host.as_ptr()) };
            }
        });
        let exposed = Mutex::new(catalog_sync::ExposedParams::new(
            Arc::clone(&param_infos),
            params_arc.as_ref(),
        ));
        let remote_pages = Mutex::new(presented_remote_control_pages(param_infos.iter(), |id| {
            params_arc.parameter_presentation(id)
        }));
        let meter_store = instance.meter_store();
        let snapshot = instance.snapshot_slot();
        let task_spawner = instance.task_spawner();
        let editor_builder = instance.editor_builder();
        let latency_cache = AtomicU32::new(instance.latency());
        let latency_live = AtomicU32::new(instance.latency());
        let tail_cache = AtomicU32::new(instance.tail());

        // Pre-size the per-block channel-slice scratch from the worst-case
        // bus layout the plugin advertises. Without this, the first
        // `clap_plugin_process` call after activate hits the global
        // allocator on the audio thread for every channel push; this
        // amortizes the cost into instance creation, where it belongs.
        // Read before `info` is moved into the struct literal below.
        let midi_input_ports = info.midi_input_ports;
        let max_in = layouts
            .iter()
            .map(|l| l.total_input_channels() as usize)
            .max()
            .unwrap_or(0);
        let max_out = layouts
            .iter()
            .map(|l| l.total_output_channels() as usize)
            .max()
            .unwrap_or(0);

        let data = Box::new(ClapPluginData::<P> {
            persist_signal: None,
            persist_pending: Arc::new(AtomicBool::new(false)),
            host_state: ptr::null(),
            plugin: shared_plugin(instance),
            params_arc,
            meter_store,
            snapshot,
            task_spawner,
            editor_builder,
            latency_cache,
            latency_live,
            tail_cache,
            selected_config: AtomicU32::new(0),
            ports_inactive: [AtomicU32::new(0), AtomicU32::new(0)],
            ports_activation_used: AtomicBool::new(false),
            exposed,
            parameter_restart_pending: AtomicBool::new(false),
            catalog_signal: None,
            pending_structure: MainThreadCell::new(None),
            hooks,
            host_notifier,
            note_names: MainThreadCell::new(Vec::new()),
            voice_info_supported: AtomicBool::new(false),
            info,
            plugin_id_hash,
            host,
            host_params: ptr::null(),
            host_latency: ptr::null(),
            host_tail: ptr::null(),
            host_track_info: ptr::null(),
            latency_dirty: AtomicBool::new(false),
            ports_rescan: AtomicBool::new(false),
            gui_changes: Arc::new(GuiChangeQueue::new(GUI_QUEUE_CAPACITY)),
            pending_state: Arc::new(StateLoadQueue::new(1)),
            panic_latch: moose_core::wrapper::PanicLatch::new(),
            active: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            render_mode: AtomicU8::new(ProcessMode::Realtime.as_u8()),
            needs_rescan: Arc::new(AtomicBool::new(false)),
            floating_mode: AtomicBool::new(false),
            floating_visible: Arc::new(AtomicBool::new(false)),
            floating_closed: Arc::new(AtomicBool::new(false)),
            dirty_pending: Arc::new(AtomicBool::new(false)),
            presentation_revision: AtomicU64::new(0),
            presentation_pending,
            remote_pages,
            transport_slot: TransportSlot::new(),
            host_scale: AtomicU64::new(0),
            window_scale: AtomicU64::new(0),
            pending_resize: AtomicU64::new(0),
            extensions: Extensions::<P>::new(),
            audio: PluginCell::new(ClapAudio {
                event_list: EventList::with_capacity(EVENT_LIST_PREALLOC + param_count),
                sounding_notes: SoundingNotes::new(midi_input_ports),
                output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
                sub_event_scratch: EventList::with_capacity(EVENT_LIST_PREALLOC + param_count),
                deferred_params: Vec::with_capacity(param_count),
                sample_rate: 44100.0,
                max_block_size: 1024,
                input_slices: Vec::with_capacity(max_in),
                output_slices: Vec::with_capacity(max_out),
                input_widen: Vec::with_capacity(max_in),
                output_narrow: Vec::with_capacity(max_out),
                host_out_ptrs: Vec::with_capacity(max_out),
                bus_routing: BusRouting::new(),
                catalog: param_infos,
            }),
            gui: PluginCell::new(ClapGui {
                editor: None,
                gui_created: false,
                mode: GuiMode::Embedded,
                visible: false,
                title: String::new(),
                #[cfg(all(target_os = "linux", feature = "clap-draft"))]
                nested: wayland_nested::Session::default(),
            }),
        });

        let clap = Box::new(clap_plugin {
            desc: descriptor,
            plugin_data: Box::into_raw(data).cast::<c_void>(),
            init: Some(clap_plugin_init::<P>),
            destroy: Some(clap_plugin_destroy::<P>),
            activate: Some(clap_plugin_activate::<P>),
            deactivate: Some(clap_plugin_deactivate::<P>),
            start_processing: Some(clap_plugin_start_processing::<P>),
            stop_processing: Some(clap_plugin_stop_processing::<P>),
            reset: Some(clap_plugin_reset::<P>),
            process: Some(clap_plugin_process::<P>),
            get_extension: Some(clap_plugin_get_extension::<P>),
            on_main_thread: Some(clap_plugin_on_main_thread::<P>),
        });

        Box::into_raw(clap).cast_const()
    })
}

// ---------------------------------------------------------------------------
// export_clap! macro
// ---------------------------------------------------------------------------

/// Export a CLAP plugin entry point.
///
/// Usage:
/// ```ignore
/// export_clap!(MyPlugin);
/// ```
///
/// Where `MyPlugin` implements `PluginExport`.
#[macro_export]
macro_rules! export_clap {
    ($plugin_type:ty) => {
        mod _clap_entry {
            use super::*;
            use std::ffi::{CStr, c_char, c_void};
            use std::path::Path;
            use std::ptr;
            use std::sync::OnceLock;

            use ::clap_sys::entry::clap_plugin_entry;
            use ::clap_sys::factory::plugin_factory::{
                CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
            };
            use ::clap_sys::factory::preset_discovery::{
                CLAP_PRESET_DISCOVERY_FACTORY_ID, CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT,
            };
            use ::clap_sys::host::clap_host;
            use ::clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
            use ::moose_clap::CLAP_VERSION;

            use ::moose_clap::__macro_deps::moose_core::plugin::PluginRuntime;
            use ::moose_clap::DescriptorHolder;

            static DESCRIPTOR: OnceLock<DescriptorHolder> = OnceLock::new();

            fn get_descriptor() -> &'static DescriptorHolder {
                DESCRIPTOR.get_or_init(|| {
                    let info = <$plugin_type as PluginRuntime>::info();
                    DescriptorHolder::new(&info)
                })
            }

            static FACTORY: clap_plugin_factory = clap_plugin_factory {
                get_plugin_count: Some(factory_get_plugin_count),
                get_plugin_descriptor: Some(factory_get_plugin_descriptor),
                create_plugin: Some(factory_create_plugin),
            };

            unsafe extern "C" fn factory_get_plugin_count(
                _factory: *const clap_plugin_factory,
            ) -> u32 {
                1
            }

            unsafe extern "C" fn factory_get_plugin_descriptor(
                _factory: *const clap_plugin_factory,
                index: u32,
            ) -> *const clap_plugin_descriptor {
                if index == 0 {
                    &get_descriptor().descriptor as *const clap_plugin_descriptor
                } else {
                    ptr::null()
                }
            }

            unsafe extern "C" fn factory_create_plugin(
                _factory: *const clap_plugin_factory,
                host: *const clap_host,
                plugin_id: *const c_char,
            ) -> *const clap_plugin {
                if plugin_id.is_null()
                    || host.is_null()
                    || !clap_sys::version::clap_version_is_compatible((*host).clap_version)
                {
                    return ptr::null();
                }
                let requested_id = CStr::from_ptr(plugin_id);
                let info = <$plugin_type as PluginRuntime>::info();
                let our_id = match std::ffi::CString::new(info.clap_id) {
                    Ok(s) => s,
                    Err(_) => return ptr::null(),
                };
                if requested_id != our_id.as_c_str() {
                    return ptr::null();
                }
                let descriptor = &get_descriptor().descriptor as *const clap_plugin_descriptor;
                ::moose_clap::create_plugin_instance::<$plugin_type>(descriptor, host)
            }

            unsafe extern "C" fn entry_init(plugin_path: *const c_char) -> bool {
                // Force descriptor initialization.
                let _ = get_descriptor();
                // Remember where the host loaded us from - the preset
                // discovery factory derives the factory-preset
                // location from it.
                ::moose_clap::presets::set_plugin_path(plugin_path);
                true
            }

            unsafe extern "C" fn entry_deinit() {}

            unsafe extern "C" fn entry_get_factory(factory_id: *const c_char) -> *const c_void {
                if factory_id.is_null() {
                    return ptr::null();
                }
                let id = CStr::from_ptr(factory_id);
                if id == CLAP_PLUGIN_FACTORY_ID {
                    return &FACTORY as *const clap_plugin_factory as *const c_void;
                }
                if id == CLAP_PRESET_DISCOVERY_FACTORY_ID
                    || id == CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT
                {
                    return ::moose_clap::presets::discovery_factory::<$plugin_type>()
                        as *const c_void;
                }
                ptr::null()
            }

            #[unsafe(no_mangle)]
            #[allow(non_upper_case_globals)]
            pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
                clap_version: CLAP_VERSION,
                init: Some(entry_init),
                deinit: Some(entry_deinit),
                get_factory: Some(entry_get_factory),
            };
        }
    };
}

#[cfg(test)]
mod transport_tests {
    use super::{
        CLAP_SECTIME_FACTOR, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, build_transport_info,
        clap_event_transport,
    };

    #[test]
    fn position_samples_derived_from_seconds_timeline() {
        // SAFETY: clap_event_transport is a repr(C) POD; all-zero is a
        // valid value (no flags set, all positions 0).
        let mut t: clap_event_transport = unsafe { std::mem::zeroed() };
        t.flags = CLAP_TRANSPORT_HAS_SECONDS_TIMELINE;
        // 1.5 seconds, in CLAP's power-of-two fixed point (no float cast).
        t.song_pos_seconds = CLAP_SECTIME_FACTOR + CLAP_SECTIME_FACTOR / 2;
        let info = build_transport_info(&t, 48_000.0);
        assert_eq!(info.position_samples, 72_000); // 1.5 s * 48 kHz
        assert!((info.position_seconds - 1.5).abs() < 1e-9);
    }

    #[test]
    fn position_samples_zero_without_seconds_timeline() {
        // SAFETY: see above - zeroed POD, no seconds-timeline flag.
        let t: clap_event_transport = unsafe { std::mem::zeroed() };
        assert_eq!(build_transport_info(&t, 48_000.0).position_samples, 0);
    }
}

#[cfg(test)]
mod channel_slice_tests {
    use super::{input_channel_slice, output_channel_slice};

    /// An unconnected CLAP input port (null channel pointer) reaches the
    /// plugin as block-length silence, never the empty slice it would read
    /// out of range.
    #[test]
    fn null_input_channel_reads_block_length_silence() {
        let nf = 256usize;
        let mut scratch = Vec::<f32>::new();
        // SAFETY: the null pointer is the unconnected-port case under test;
        // `scratch` outlives the returned slice within this call.
        let slice =
            unsafe { input_channel_slice::<f32, f32>(std::ptr::null(), nf, &mut scratch, false) };
        assert_eq!(slice.len(), nf);
        assert!(slice.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn in_place_host_input_survives_output_writes() {
        use super::{clap_audio_buffer, clap_process, input_overlaps_outputs};
        let mut samples = [0.25_f32, -0.75, 0.5];
        let input_ptr = samples.as_mut_ptr();
        let mut outputs = [input_ptr];
        let mut bus = clap_audio_buffer {
            data32: outputs.as_mut_ptr(),
            data64: std::ptr::null_mut(),
            channel_count: 1,
            latency: 0,
            constant_mask: 0,
        };
        // SAFETY: all-zero is valid for the unused CLAP fields, which are not read here.
        let mut process: clap_process = unsafe { std::mem::zeroed() };
        process.audio_outputs = &raw mut bus;
        process.audio_outputs_count = 1;
        process.frames_count = u32::try_from(samples.len()).unwrap();
        let mut scratch = Vec::with_capacity(samples.len());
        // SAFETY: the bus, channel pointer, and sample array are alive for this call.
        let copy = unsafe {
            input_overlaps_outputs(input_ptr.cast(), std::mem::size_of_val(&samples), &process)
        };
        let input = unsafe {
            input_channel_slice::<f32, f32>(input_ptr, samples.len(), &mut scratch, copy)
        };
        // SAFETY: input now refers to scratch, not the host output array.
        let output =
            unsafe { output_channel_slice::<f32, f32>(input_ptr, samples.len(), &mut Vec::new()) };
        for (dst, src) in output.iter_mut().zip(input) {
            *dst = *src * 2.0;
        }
        assert_eq!(
            samples.map(f32::to_bits),
            [0.5_f32, -1.5, 1.0].map(f32::to_bits)
        );
    }

    /// A null output channel is a block-length discard buffer the plugin
    /// can write without an out-of-range panic.
    #[test]
    fn null_output_channel_is_block_length_discard() {
        let nf = 256usize;
        let mut scratch = Vec::<f32>::new();
        // SAFETY: the null pointer is the unconnected-port case under test.
        let slice =
            unsafe { output_channel_slice::<f32, f32>(std::ptr::null_mut(), nf, &mut scratch) };
        assert_eq!(slice.len(), nf);
        for s in slice.iter_mut() {
            *s = 1.0;
        }
    }
}

#[cfg(test)]
mod note_expression_tests {
    use super::{
        CLAP_NOTE_EXPRESSION_TUNING, CLAP_NOTE_EXPRESSION_VOLUME, clap_event_note_expression,
        clap_note_expression_of, note_expression_body,
    };
    use moose_core::events::EventBody;

    fn expression(id: i32, value: f64) -> clap_event_note_expression {
        // SAFETY: clap_event_note_expression is a repr(C) POD; all-zero
        // is valid (channel 0, key 0).
        let mut ne: clap_event_note_expression = unsafe { std::mem::zeroed() };
        ne.expression_id = id;
        ne.value = value;
        ne
    }

    #[test]
    fn volume_crosses_in_the_gain_domain() {
        // CLAP `VOLUME` is plain linear gain `0..=4`; wire full-scale
        // must land on gain 4 (+12 dB), unity on the quarter point.
        let (id, _, _, value) = clap_note_expression_of(&EventBody::PerNoteCC {
            group: 0,
            channel: 0,
            note: 60,
            cc: 7,
            value: u32::MAX,
            registered: true,
        })
        .expect("volume maps");
        assert_eq!(id, CLAP_NOTE_EXPRESSION_VOLUME);
        assert!((value - 4.0).abs() < 1e-9);

        // Host unity gain decodes to the wire's quarter point.
        let body = note_expression_body(&expression(CLAP_NOTE_EXPRESSION_VOLUME, 1.0), 0, 60)
            .expect("volume decodes");
        let EventBody::PerNoteCC { cc: 7, value, .. } = body else {
            panic!("expected volume PerNoteCC, got {body:?}");
        };
        assert!((f64::from(value) / f64::from(u32::MAX) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn assignable_per_note_cc_is_not_an_expression() {
        // Only registered per-note indices carry the predefined
        // expression semantics; an assignable index 7 is not volume.
        assert!(
            clap_note_expression_of(&EventBody::PerNoteCC {
                group: 0,
                channel: 0,
                note: 60,
                cc: 7,
                value: u32::MAX,
                registered: false,
            })
            .is_none()
        );
    }

    #[test]
    fn tuning_full_scale_is_48_semitones() {
        let (_, _, _, semis) = clap_note_expression_of(&EventBody::PerNotePitchBend {
            group: 0,
            channel: 0,
            note: 60,
            value: u32::MAX,
        })
        .expect("tuning maps");
        assert!((semis - 48.0).abs() < 1e-6);

        // A host bend beyond the wire's range saturates.
        let body = note_expression_body(&expression(CLAP_NOTE_EXPRESSION_TUNING, 120.0), 0, 60)
            .expect("tuning decodes");
        assert!(matches!(
            body,
            EventBody::PerNotePitchBend {
                value: u32::MAX,
                ..
            }
        ));
    }
}

#[cfg(test)]
mod sounding_notes_tests {
    use super::{NoteAxis, SoundingNotes};

    #[test]
    fn wildcard_axis_parses_all_concrete_and_junk() {
        assert_eq!(NoteAxis::parse(-1, 16), NoteAxis::All); // wildcard
        assert_eq!(NoteAxis::parse(3, 16), NoteAxis::One(3)); // concrete
        assert_eq!(NoteAxis::parse(16, 16), NoteAxis::Invalid); // out of domain
        assert_eq!(NoteAxis::parse(-2, 128), NoteAxis::Invalid); // hostile
    }

    #[test]
    fn drain_matches_only_the_requested_axes() {
        let mut s = SoundingNotes::new(2);
        s.set(0, 0, 60);
        s.set(0, 3, 60);
        s.set(0, 3, 64);
        s.set(1, 3, 60); // other port: never touched below

        // Key wildcard, concrete channel: both channel-3 notes drain.
        let mut hits = Vec::new();
        s.drain_matching(0, Some(3), None, |ch, k| hits.push((ch, k)));
        assert_eq!(hits, [(3, 60), (3, 64)]);

        // Drained notes are cleared - a second wildcard finds nothing.
        hits.clear();
        s.drain_matching(0, Some(3), None, |ch, k| hits.push((ch, k)));
        assert_eq!(hits, []);

        // Channel wildcard, concrete key.
        s.set(0, 5, 60);
        hits.clear();
        s.drain_matching(0, None, Some(60), |ch, k| hits.push((ch, k)));
        assert_eq!(hits, [(0, 60), (5, 60)]);

        // Both wildcard: everything left on the port.
        s.set(0, 9, 1);
        hits.clear();
        s.drain_matching(0, None, None, |ch, k| hits.push((ch, k)));
        assert_eq!(hits, [(9, 1)]);

        // The other port's note survived it all.
        hits.clear();
        s.drain_matching(1, None, None, |ch, k| hits.push((ch, k)));
        assert_eq!(hits, [(3, 60)]);
    }

    #[test]
    fn concrete_off_clears_the_bit() {
        let mut s = SoundingNotes::new(1);
        s.set(0, 0, 60);
        s.clear(0, 0, 60);
        let mut hits = Vec::new();
        s.drain_matching(0, None, None, |ch, k| hits.push((ch, k)));
        assert_eq!(hits, []);
    }

    #[test]
    fn for_each_leaves_notes_sounding() {
        // Expression fan-out addresses voices without releasing them:
        // the same wildcard visit must keep finding the note.
        let mut s = SoundingNotes::new(1);
        s.set(0, 2, 60);
        s.set(0, 2, 64);
        for _ in 0..2 {
            let mut hits = Vec::new();
            s.for_each_matching(0, Some(2), None, |ch, k| hits.push((ch, k)));
            assert_eq!(hits, [(2, 60), (2, 64)]);
        }
    }
}

#[cfg(test)]
mod note_address_tests {
    use super::{clap_event_note, clap_note_address};

    fn note(channel: i16, key: i16) -> clap_event_note {
        // SAFETY: clap_event_note is a repr(C) POD; all-zero is valid.
        let mut ne: clap_event_note = unsafe { std::mem::zeroed() };
        ne.channel = channel;
        ne.key = key;
        ne
    }

    #[test]
    fn concrete_address_resolves() {
        assert_eq!(clap_note_address(&note(15, 127)), Some((15, 127)));
        assert_eq!(clap_note_address(&note(0, 0)), Some((0, 0)));
    }

    #[test]
    fn wildcard_and_out_of_range_are_dropped() {
        // `-1` is CLAP's "match all" wildcard; anything past the MIDI
        // domain is a hostile/buggy host. Neither may reach a plugin
        // that indexes tables by note/channel.
        assert_eq!(clap_note_address(&note(-1, 60)), None);
        assert_eq!(clap_note_address(&note(0, -1)), None);
        assert_eq!(clap_note_address(&note(16, 60)), None);
        assert_eq!(clap_note_address(&note(0, 128)), None);
    }
}

#[cfg(test)]
mod remote_controls_tests {
    use super::catalog_sync::parameter_indices;
    use super::{
        ParamFlags, ParamInfo, ParamRange, remote_control_pages, remote_controls_page_id,
        split_group,
    };
    use moose_params::{ParamUnit, ParamValueKind, ParameterPresentation};

    fn info(id: u32, group: &'static str) -> ParamInfo {
        ParamInfo {
            id,
            name: "p",
            short_name: "p",
            group,
            range: ParamRange::Linear { min: 0.0, max: 1.0 },
            default_plain: 0.0,
            flags: ParamFlags::empty(),
            unit: ParamUnit::None,
            kind: ParamValueKind::Float,
            midi_map: None,
            midi_channel: None,
        }
    }

    #[test]
    fn runtime_pages_use_live_groups_and_visibility_without_changing_ids() {
        let infos = std::sync::Arc::new(moose_params::ParamCatalog::new(vec![
            info(7, "OLD"),
            info(8, "OLD"),
            info(9, "OLD"),
        ]));
        let pages = super::presented_remote_control_pages(infos.iter(), |id| {
            Some(ParameterPresentation {
                name: format!("Control {id}"),
                group: "Modulators/LFO 1".into(),
                hidden: id == 8,
                available: id != 9,
            })
        });
        assert_eq!(pages, vec![("Modulators/LFO 1".into(), vec![7])]);
        assert_eq!(infos.iter().map(|p| p.id).collect::<Vec<_>>(), [7, 8, 9]);
        let pages = super::presented_remote_control_pages(infos.iter(), |_| None);
        assert_eq!(pages, vec![("OLD".into(), vec![7, 8, 9])]);
    }

    #[test]
    fn ungrouped_params_produce_no_page() {
        let infos = vec![info(0, ""), info(1, "")];
        assert_eq!(remote_control_pages(&infos), []);
    }

    #[test]
    fn unavailable_params_leave_stable_ids_out_of_clap_enumeration() {
        let infos = [info(10, "EQ"), info(11, "EQ"), info(12, "EQ")];
        let indices = parameter_indices(&infos, |id| {
            (id == 11).then(|| ParameterPresentation {
                available: false,
                ..ParameterPresentation::default()
            })
        });
        assert_eq!(indices, [0, 2]);
        assert_eq!(
            remote_control_pages(indices.iter().map(|&i| &infos[i])),
            [("EQ", vec![10, 12])]
        );
    }

    #[test]
    fn hidden_and_readonly_grouped_params_take_no_slot() {
        // A grouped-but-hidden / read-only param organizes the param
        // tree but isn't a performance control, so it must not burn one
        // of a page's eight slots.
        let mut hidden = info(0, "EQ");
        hidden.flags = ParamFlags::HIDDEN;
        let visible = info(1, "EQ");
        let mut readonly = info(2, "EQ");
        readonly.flags = ParamFlags::READONLY;
        let infos = [hidden, visible, readonly];
        let pages = remote_control_pages(&infos);
        assert_eq!(pages, vec![("EQ", vec![1])]);
    }

    #[test]
    fn group_over_eight_params_splits_into_two_pages() {
        let infos: Vec<ParamInfo> = (0..10).map(|id| info(id, "EQ")).collect();
        let pages = remote_control_pages(&infos);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].1.len(), 8);
        assert_eq!(pages[1].1.len(), 2);
        assert_eq!(pages[1].1, vec![8, 9]);
    }

    #[test]
    fn distinct_groups_get_distinct_pages() {
        let mut infos: Vec<ParamInfo> = (0..3).map(|id| info(id, "EQ")).collect();
        infos.extend((3..5).map(|id| info(id, "DYN")));
        let pages = remote_control_pages(&infos);
        assert_eq!(pages, vec![("EQ", vec![0, 1, 2]), ("DYN", vec![3, 4])]);
    }

    #[test]
    fn split_group_separates_section_from_page() {
        assert_eq!(split_group("EQ/Lo Shelf"), ("EQ", "Lo Shelf"));
        // Only the first '/' is a separator; a page name may itself
        // contain one without gaining a third level.
        assert_eq!(split_group("EQ/Lo Shelf/Extra"), ("EQ", "Lo Shelf/Extra"));
    }

    #[test]
    fn split_group_without_slash_is_its_own_section() {
        assert_eq!(split_group("Compressor"), ("Compressor", "Compressor"));
    }

    #[test]
    fn page_id_is_stable_and_distinguishes_chunks() {
        // Deterministic: identical inputs hash identically (a host
        // persists this across processes).
        assert_eq!(
            remote_controls_page_id("EQ", 0),
            remote_controls_page_id("EQ", 0)
        );
        // The two pages of a >8-param group get distinct ids.
        assert_ne!(
            remote_controls_page_id("EQ", 0),
            remote_controls_page_id("EQ", 1)
        );
        // Distinct group names get distinct ids.
        assert_ne!(
            remote_controls_page_id("EQ", 0),
            remote_controls_page_id("DYN", 0)
        );
    }
}

#[cfg(test)]
mod host_state_tests {
    use super::{MAX_HOST_STATE_BYTES, read_host_state};

    #[test]
    fn reads_until_end_of_stream() {
        let mut chunks = vec![3_i64, 2, 0].into_iter();
        let blob = read_host_state(|buf| {
            let n = chunks.next().unwrap();
            buf[..usize::try_from(n).unwrap()].fill(7);
            n
        });
        assert_eq!(blob, Some(vec![7; 5]));
    }

    #[test]
    fn rejects_read_errors_and_overlong_counts() {
        assert_eq!(read_host_state(|_| -1), None);
        // A host claiming more bytes than the buffer holds is lying.
        assert_eq!(
            read_host_state(|buf| i64::try_from(buf.len()).unwrap() + 1),
            None
        );
    }

    #[test]
    fn rejects_streams_past_the_cap() {
        let mut total = 0usize;
        let blob = read_host_state(|buf| {
            total += buf.len();
            assert!(total <= MAX_HOST_STATE_BYTES + buf.len(), "kept reading");
            i64::try_from(buf.len()).unwrap()
        });
        assert_eq!(blob, None);
    }
}

#[cfg(test)]
mod deferred_param_tests {
    use super::{Event, EventBody, EventList, defer_param_change, replay_deferred_params};

    fn change(offset: u32, id: u32, value: f64) -> Event {
        Event::new(offset, EventBody::ParamChange { id, value })
    }

    #[test]
    fn flushed_changes_are_latest_wins_and_bounded() {
        let mut deferred = Vec::with_capacity(2);
        defer_param_change(&mut deferred, 7, 0.2);
        defer_param_change(&mut deferred, 7, 0.6);
        defer_param_change(&mut deferred, 8, 0.1);
        defer_param_change(&mut deferred, 9, 0.9); // over capacity: dropped
        assert_eq!(deferred, vec![(7, 0.6), (8, 0.1)]);
    }

    #[test]
    fn replayed_changes_precede_the_block_and_sort_stays_stable() {
        let mut deferred = vec![(7, 0.6)];
        let mut list = EventList::with_capacity(8);
        replay_deferred_params(&mut list, &mut deferred, false);
        list.push(change(32, 7, 0.7));
        list.push(change(0, 8, 0.1));
        list.ensure_sorted_by_offset();
        let seen: Vec<_> = list
            .iter()
            .map(|e| match e.body {
                EventBody::ParamChange { id, value } => (e.sample_offset, id, value),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(seen, vec![(0, 7, 0.6), (0, 8, 0.1), (32, 7, 0.7)]);
        assert_eq!(deferred, []);
    }

    #[test]
    fn state_load_discards_deferred_changes() {
        let mut deferred = vec![(7, 0.6)];
        let mut list = EventList::with_capacity(8);
        replay_deferred_params(&mut list, &mut deferred, true);
        assert_eq!(list.iter().count(), 0);
        assert_eq!(deferred, []);
    }
}

#[cfg(test)]
mod bus_kind_tests {
    use super::{BusKind, CLAP_AUDIO_PORT_IS_MAIN, audio_port_main_flag};

    #[test]
    fn only_a_main_bus_at_index_zero_is_clap_main() {
        assert_eq!(
            audio_port_main_flag(0, BusKind::Main),
            CLAP_AUDIO_PORT_IS_MAIN
        );
        // Main bus disabled: the first enabled port is the sidechain.
        assert_eq!(audio_port_main_flag(0, BusKind::Sidechain), 0);
        assert_eq!(audio_port_main_flag(1, BusKind::Sidechain), 0);
    }
}

// The derive names `::moose::{core, params}`; test-only `#[derive(Params)]`s resolve here.
#[cfg(test)]
extern crate self as moose;
#[cfg(test)]
mod large_state_tests {
    use super::large_state_fixture::{LargePlugin, payload};
    use super::*;
    type Large = LargePlugin<{ 64 * 1024 * 1024 }>;
    struct Stream {
        bytes: Vec<u8>,
        offset: usize,
    }
    unsafe extern "C" fn write(s: *const clap_ostream, bytes: *const c_void, size: u64) -> i64 {
        let s = unsafe { &mut *((*s).ctx.cast::<Stream>()) };
        let n = (usize::try_from(size).expect("test buffer length fits usize")).min(317);
        s.bytes
            .extend_from_slice(unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), n) });
        i64::try_from(n).expect("test byte count fits i64")
    }
    unsafe extern "C" fn read(s: *const clap_istream, dst: *mut c_void, size: u64) -> i64 {
        let s = unsafe { &mut *((*s).ctx.cast::<Stream>()) };
        let n = (usize::try_from(size).expect("test buffer length fits usize"))
            .min(311)
            .min(s.bytes.len() - s.offset);
        unsafe { std::ptr::copy_nonoverlapping(s.bytes.as_ptr().add(s.offset), dst.cast(), n) };
        s.offset += n;
        i64::try_from(n).expect("test byte count fits i64")
    }
    #[test]
    fn large_host_state_roundtrips_and_default_bound_rejects() {
        unsafe {
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            let descriptor = DescriptorHolder::new(&Large::info());
            let plugin =
                create_plugin_instance::<Large>(&raw const descriptor.descriptor, &raw const host);
            assert!(clap_plugin_init::<Large>(plugin));
            let data = data_from_plugin::<Large>(plugin);
            *data.params_arc.payload.lock().unwrap() = payload();
            let mut s = Stream {
                bytes: Vec::new(),
                offset: 0,
            };
            let out = clap_ostream {
                ctx: (&raw mut s).cast(),
                write: Some(write),
            };
            assert!(state_save::<Large>(plugin, &raw const out));
            assert!(s.bytes.len() > 32 * 1024 * 1024);
            data.params_arc.payload.lock().unwrap().clear();
            let input = clap_istream {
                ctx: (&raw mut s).cast(),
                read: Some(read),
            };
            assert!(state_load::<Large>(plugin, &raw const input));
            assert_eq!(*data.params_arc.payload.lock().unwrap(), payload());
            s.offset = 0;
            s.bytes.pop();
            assert!(!state_load::<Large>(plugin, &raw const input));
            clap_plugin_destroy::<Large>(plugin);
        }
    }
}

#[cfg(test)]
mod persist_notify_tests;

#[cfg(test)]
mod host_features_tests;
#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod surround_tests {
    use super::host_value_fixture::{TestParams, TestPlugin};
    use super::*;
    use moose_core::bus::BusLayout;

    struct Surround(TestPlugin);
    impl PluginExport for Surround {
        type Params = TestParams;
        fn create() -> Self {
            Self(TestPlugin::create())
        }
        fn params(&self) -> &TestParams {
            self.0.params()
        }
        fn params_arc(&self) -> Arc<TestParams> {
            self.0.params_arc()
        }
        fn meter_store(&self) -> Arc<MeterStore> {
            self.0.meter_store()
        }
        fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
            self.0.snapshot_slot()
        }
    }
    impl PluginRuntime for Surround {
        type Sample = f32;
        fn reset(&mut self, _: &AudioConfig) {}
        fn info() -> PluginInfo {
            TestPlugin::info()
        }
        fn bus_layouts() -> Vec<BusLayout> {
            [4, 6, 8, 5]
                .into_iter()
                .map(|count| {
                    BusLayout::new()
                        .with_optional_sidechain_input("disabled", ChannelConfig::Stereo, false)
                        .with_input("main", ChannelConfig::Custom(count))
                        .with_output("main", ChannelConfig::Custom(count))
                })
                .collect()
        }
        fn process(
            &mut self,
            _: &mut AudioBuffer,
            _: &EventList,
            _: &mut moose_core::process::ProcessContext,
        ) -> ProcessStatus {
            ProcessStatus::Normal
        }
    }

    #[test]
    fn host_reads_selected_canonical_surround_maps() {
        unsafe {
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            let descriptor = DescriptorHolder::new(&Surround::info());
            let plugin = create_plugin_instance::<Surround>(
                &raw const descriptor.descriptor,
                &raw const host,
            );
            assert!(clap_plugin_init::<Surround>(plugin));
            let ext = clap_plugin_get_extension::<Surround>(plugin, CLAP_EXT_SURROUND.as_ptr())
                .cast::<clap_plugin_surround>();
            assert!(!ext.is_null());
            let map = (*ext).get_channel_map.unwrap();
            let supported = (*ext).is_channel_mask_supported.unwrap();
            for (index, expected) in [
                &[0, 1, 4, 5][..],
                &[0, 1, 2, 3, 4, 5],
                &[0, 1, 2, 3, 4, 5, 9, 10],
            ]
            .into_iter()
            .enumerate()
            {
                assert!(audio_ports_config_select::<Surround>(
                    plugin,
                    len_u32(index)
                ));
                let mut port_info: clap_audio_port_info = std::mem::zeroed();
                assert!(audio_ports_get::<Surround>(
                    plugin,
                    0,
                    false,
                    &raw mut port_info
                ));
                assert_eq!(CStr::from_ptr(port_info.port_type), CLAP_PORT_SURROUND);
                let mask = expected
                    .iter()
                    .fold(0u64, |bits, channel| bits | (1 << channel));
                assert!(supported(plugin, mask));
                for is_input in [true, false] {
                    let mut output = [255; 8];
                    assert_eq!(
                        map(plugin, is_input, 0, output.as_mut_ptr(), 8),
                        len_u32(expected.len())
                    );
                    assert_eq!(&output[..expected.len()], expected);
                    assert_eq!(
                        map(
                            plugin,
                            is_input,
                            0,
                            output.as_mut_ptr(),
                            len_u32(expected.len() - 1)
                        ),
                        0
                    );
                    assert_eq!(map(plugin, is_input, 1, output.as_mut_ptr(), 8), 0);
                }
            }
            assert!(!supported(plugin, 0));
            assert!(!supported(
                plugin,
                (1 << 0) | (1 << 1) | (1 << 9) | (1 << 10)
            ));
            assert!(audio_ports_config_select::<Surround>(plugin, 3));
            let mut output = [255; 8];
            assert_eq!(map(plugin, true, 0, output.as_mut_ptr(), 8), 0);
            assert_eq!(map(plugin, true, 0, ptr::null_mut(), 8), 0);
            clap_plugin_destroy::<Surround>(plugin);
        }
    }
}
