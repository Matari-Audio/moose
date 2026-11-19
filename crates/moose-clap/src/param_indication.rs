//! `clap.param-indication`: the host's controller mappings and automation
//! state per parameter, passed to [`MainThreadHooks::param_indication`].
//!
//! [`MainThreadHooks::param_indication`]: moose_core::main_thread::MainThreadHooks::param_indication

use std::ffi::{CStr, c_char};

use clap_sys::color::clap_color;
use clap_sys::ext::param_indication::{
    CLAP_PARAM_INDICATION_AUTOMATION_OVERRIDING, CLAP_PARAM_INDICATION_AUTOMATION_PLAYING,
    CLAP_PARAM_INDICATION_AUTOMATION_PRESENT, CLAP_PARAM_INDICATION_AUTOMATION_RECORDING,
    clap_plugin_param_indication,
};
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;
use moose_core::export::PluginExport;
use moose_core::main_thread::{AutomationState, HostColor, HostMapping, ParamIndication};
use moose_core::thread::MainThread;
use moose_core::wrapper::run_extern_callback_with;

use crate::data_from_plugin;
use crate::track_info::host_color;

pub(crate) fn make_extension<P: PluginExport>() -> clap_plugin_param_indication {
    clap_plugin_param_indication {
        set_mapping: Some(set_mapping::<P>),
        set_automation: Some(set_automation::<P>),
    }
}

/// # Safety
/// `color` is null or points to a readable `clap_color`.
unsafe fn color(color: *const clap_color) -> Option<HostColor> {
    // SAFETY: caller contract.
    unsafe { color.as_ref() }.copied().map(host_color)
}

/// # Safety
/// `text` is null or a NUL-terminated string.
unsafe fn text(text: *const c_char) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: caller contract.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

fn automation_state(state: u32) -> AutomationState {
    match state {
        CLAP_PARAM_INDICATION_AUTOMATION_PRESENT => AutomationState::Present,
        CLAP_PARAM_INDICATION_AUTOMATION_PLAYING => AutomationState::Playing,
        CLAP_PARAM_INDICATION_AUTOMATION_RECORDING => AutomationState::Recording,
        CLAP_PARAM_INDICATION_AUTOMATION_OVERRIDING => AutomationState::Overriding,
        _ => AutomationState::None,
    }
}

/// # Safety
/// Call only from a param-indication callback with its `plugin`.
unsafe fn deliver<P: PluginExport>(
    plugin: *const clap_plugin,
    id: clap_id,
    indication: ParamIndication,
) {
    // SAFETY: CLAP passes the live instance created for `P`.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    // SAFETY: both param-indication callbacks are [main-thread]
    // (param-indication.h); `deliver` runs only inside them.
    let main = unsafe { MainThread::assume() };
    run_extern_callback_with::<P, ()>("CLAP", "param_indication", (), || {
        data.with_hooks(&main, |hooks| {
            hooks.param_indication(&main, id, indication);
        });
    });
}

unsafe extern "C" fn set_mapping<P: PluginExport>(
    plugin: *const clap_plugin,
    param_id: clap_id,
    has_mapping: bool,
    color_ptr: *const clap_color,
    label: *const c_char,
    description: *const c_char,
) {
    let mapping = has_mapping.then(|| HostMapping {
        // SAFETY: the host passes null or valid pointers (param-indication.h).
        color: unsafe { color(color_ptr) },
        // SAFETY: as above.
        label: unsafe { text(label) },
        // SAFETY: as above.
        description: unsafe { text(description) },
    });
    // SAFETY: inside the callback, with its plugin.
    unsafe { deliver::<P>(plugin, param_id, ParamIndication::Mapping(mapping)) };
}

unsafe extern "C" fn set_automation<P: PluginExport>(
    plugin: *const clap_plugin,
    param_id: clap_id,
    automation_state_raw: u32,
    color_ptr: *const clap_color,
) {
    let indication = ParamIndication::Automation {
        state: automation_state(automation_state_raw),
        // SAFETY: the host passes null or a valid colour (param-indication.h).
        color: unsafe { color(color_ptr) },
    };
    // SAFETY: inside the callback, with its plugin.
    unsafe { deliver::<P>(plugin, param_id, indication) };
}
