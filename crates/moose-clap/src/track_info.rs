//! `clap.track-info`: the host's track name, colour and channel count,
//! passed to [`MainThreadHooks::track_info_changed`] at init and on every
//! `changed`.
//!
//! [`MainThreadHooks::track_info_changed`]: moose_core::main_thread::MainThreadHooks::track_info_changed

use std::ffi::CStr;

use clap_sys::color::clap_color;
use clap_sys::ext::track_info::{
    CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL, CLAP_TRACK_INFO_HAS_TRACK_COLOR,
    CLAP_TRACK_INFO_HAS_TRACK_NAME, CLAP_TRACK_INFO_IS_FOR_BUS, CLAP_TRACK_INFO_IS_FOR_MASTER,
    CLAP_TRACK_INFO_IS_FOR_RETURN_TRACK, clap_plugin_track_info, clap_track_info,
};
use clap_sys::plugin::clap_plugin;
use moose_core::export::PluginExport;
use moose_core::main_thread::{HostColor, TrackInfo, TrackKind};
use moose_core::thread::MainThread;
use moose_core::wrapper::run_extern_callback_with;

use crate::{ClapPluginData, data_from_plugin};

pub(crate) fn make_extension<P: PluginExport>() -> clap_plugin_track_info {
    clap_plugin_track_info {
        changed: Some(changed::<P>),
    }
}

pub(crate) fn host_color(color: clap_color) -> HostColor {
    HostColor {
        r: color.red,
        g: color.green,
        b: color.blue,
        a: color.alpha,
    }
}

fn track_info(raw: &clap_track_info) -> TrackInfo {
    let has = |flag| raw.flags & flag != 0;
    let mut info = TrackInfo::new();
    if has(CLAP_TRACK_INFO_HAS_TRACK_NAME) {
        // SAFETY: `name` is a CLAP_NAME_SIZE buffer whose last byte `notify`
        // forces to NUL, so the scan stays inside it.
        let name = unsafe { CStr::from_ptr(raw.name.as_ptr()) };
        info.name = Some(name.to_string_lossy().into_owned());
    }
    if has(CLAP_TRACK_INFO_HAS_TRACK_COLOR) {
        info.color = Some(host_color(raw.color));
    }
    if has(CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL) {
        info.audio_channels = u32::try_from(raw.audio_channel_count).ok();
    }
    info.kind = if has(CLAP_TRACK_INFO_IS_FOR_MASTER) {
        TrackKind::Master
    } else if has(CLAP_TRACK_INFO_IS_FOR_RETURN_TRACK) {
        TrackKind::Return
    } else if has(CLAP_TRACK_INFO_IS_FOR_BUS) {
        TrackKind::Bus
    } else {
        TrackKind::Track
    };
    info
}

/// [main-thread] Read the host's track info and hand it to the hooks.
///
/// # Safety
/// `data` belongs to a live instance; `host_track_info` is null or valid.
pub(crate) unsafe fn notify<P: PluginExport>(data: &ClapPluginData<P>, main: &MainThread) {
    if data.host_track_info.is_null() {
        return;
    }
    // SAFETY: non-null host extension, valid for the instance's lifetime.
    let Some(get) = (unsafe { (*data.host_track_info).get }) else {
        return;
    };
    // SAFETY: `clap_track_info` is plain C data; all-zero is a valid value.
    let mut raw: clap_track_info = unsafe { std::mem::zeroed() };
    // SAFETY: `get` is [main-thread]; `raw` is writable for the call.
    if !unsafe { get(data.host, &raw mut raw) } {
        return;
    }
    if let Some(last) = raw.name.last_mut() {
        *last = 0;
    }
    let info = track_info(&raw);
    run_extern_callback_with::<P, ()>("CLAP", "track_info_changed", (), || {
        data.with_hooks(main, |hooks| hooks.track_info_changed(main, &info));
    });
}

unsafe extern "C" fn changed<P: PluginExport>(plugin: *const clap_plugin) {
    // SAFETY: CLAP passes the live instance created for `P`.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    // SAFETY: `clap_plugin_track_info.changed` is [main-thread] (track-info.h).
    let main = unsafe { MainThread::assume() };
    // SAFETY: live instance; the host pointer was fetched at init.
    unsafe { notify(data, &main) };
}
