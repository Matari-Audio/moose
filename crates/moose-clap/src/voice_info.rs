//! `clap.voice-info`: voice count and capacity from
//! [`MainThreadHooks::voice_info`].
//!
//! The extension is offered only when the hooks report `Some` at init:
//! `get_extension` can't call the hooks (it is `[thread-safe]`), and hosts
//! query extensions after `init`. A plugin that later returns `None` makes
//! `get` fail instead.
//!
//! [`MainThreadHooks::voice_info`]: moose_core::main_thread::MainThreadHooks::voice_info

use clap_sys::ext::voice_info::{
    CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES, clap_plugin_voice_info, clap_voice_info,
};
use clap_sys::plugin::clap_plugin;
use moose_core::export::PluginExport;
use moose_core::main_thread::VoiceInfo;
use moose_core::thread::MainThread;
use moose_core::wrapper::run_extern_callback_with;

use crate::{ClapPluginData, data_from_plugin};

pub(crate) fn make_extension<P: PluginExport>() -> clap_plugin_voice_info {
    clap_plugin_voice_info {
        get: Some(get::<P>),
    }
}

/// [main-thread] The hooks' current voice info.
pub(crate) fn query<P: PluginExport>(
    data: &ClapPluginData<P>,
    main: &MainThread,
) -> Option<VoiceInfo> {
    run_extern_callback_with::<P, _>("CLAP", "voice_info", None, || {
        data.with_hooks(main, |hooks| hooks.voice_info(main))
            .flatten()
    })
}

unsafe extern "C" fn get<P: PluginExport>(
    plugin: *const clap_plugin,
    out: *mut clap_voice_info,
) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: CLAP passes the live instance created for `P`.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    // SAFETY: `clap_plugin_voice_info.get` is [main-thread] (voice-info.h).
    let main = unsafe { MainThread::assume() };
    let Some(info) = query(data, &main) else {
        return false;
    };
    // SAFETY: the host passes a writable `clap_voice_info`, checked non-null.
    let out = unsafe { &mut *out };
    out.voice_count = info.count;
    out.voice_capacity = info.capacity;
    out.flags = if info.overlapping_notes {
        CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES
    } else {
        0
    };
    true
}
