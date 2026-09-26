//! A CLAP host that calls `gui.set_scale` once, then closes and reopens
//! the editor, must get the reopened editor at the same scale. Every
//! `gui.create` builds a fresh editor, so the wrapper has to replay the
//! scale it stored on the instance.

#![cfg(feature = "clap")]

use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use clap_sys::ext::gui::{CLAP_EXT_GUI, CLAP_WINDOW_API_X11, clap_plugin_gui};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin_descriptor;
use clap_sys::version::CLAP_VERSION;
use moose::prelude::*;
use moose_core::editor::RawWindowHandle;

#[derive(Params)]
pub struct ProbeParams {
    #[param(name = "Gain", range = "linear(0, 1)")]
    pub gain: FloatParam,
}

/// Last scale any probe editor was told, as f64 bits (0 = none).
static EDITOR_SCALE: AtomicU64 = AtomicU64::new(0);

struct ProbeEditor;

impl Editor for ProbeEditor {
    fn size(&self) -> (u32, u32) {
        (100, 50)
    }
    fn open(&mut self, _parent: RawWindowHandle, _context: PluginContext) {}
    fn close(&mut self) {}
    fn set_scale_factor(&mut self, factor: f64) {
        EDITOR_SCALE.store(factor.to_bits(), Ordering::Relaxed);
    }
}

pub struct Probe;

impl PurePluginLogic for Probe {
    type Params = ProbeParams;

    fn process(
        _params: &ProbeParams,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }

    fn editor(_params: Arc<ProbeParams>) -> Box<dyn Editor> {
        Box::new(ProbeEditor)
    }
}

moose::plugin! {
    logic: Probe,
    params: ProbeParams,
}

unsafe extern "C" fn no_extension(_host: *const clap_host, _id: *const c_char) -> *const c_void {
    ptr::null()
}

#[test]
fn reopened_editor_keeps_the_host_scale() {
    let descriptor: &'static clap_plugin_descriptor = Box::leak(Box::new(clap_plugin_descriptor {
        clap_version: CLAP_VERSION,
        id: ptr::null(),
        name: ptr::null(),
        vendor: ptr::null(),
        url: ptr::null(),
        manual_url: ptr::null(),
        support_url: ptr::null(),
        version: ptr::null(),
        description: ptr::null(),
        features: ptr::null(),
    }));
    let host: &'static clap_host = Box::leak(Box::new(clap_host {
        clap_version: CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: ptr::null(),
        vendor: ptr::null(),
        url: ptr::null(),
        version: ptr::null(),
        get_extension: Some(no_extension),
        request_restart: None,
        request_process: None,
        request_callback: None,
    }));
    let scale = || f64::from_bits(EDITOR_SCALE.load(Ordering::Relaxed));

    // SAFETY: drives its own instance through the plugin vtable; no
    // `set_parent`, so no window is ever made.
    unsafe {
        let plugin = moose_clap::create_plugin_instance::<Plugin>(descriptor, host);
        let vt = &*plugin;
        assert!((vt.init.unwrap())(plugin));
        let gui =
            &*(vt.get_extension.unwrap())(plugin, CLAP_EXT_GUI.as_ptr()).cast::<clap_plugin_gui>();
        let api = CLAP_WINDOW_API_X11.as_ptr();

        // First open: the host sets the scale after `create`.
        assert!((gui.create.unwrap())(plugin, api, false));
        assert!((gui.set_scale.unwrap())(plugin, 1.5));
        assert!((scale() - 1.5).abs() < 1e-9);
        (gui.destroy.unwrap())(plugin);

        // Reopen without another `set_scale`.
        EDITOR_SCALE.store(0, Ordering::Relaxed);
        assert!((gui.create.unwrap())(plugin, api, false));
        assert!(
            (scale() - 1.5).abs() < 1e-9,
            "reopened editor at {}, want 1.5",
            scale()
        );
        (gui.destroy.unwrap())(plugin);

        (vt.destroy.unwrap())(plugin);
    }
}
