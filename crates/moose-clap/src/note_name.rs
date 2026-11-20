//! `clap.note-name`: key names for the host's piano roll, from
//! [`MainThreadHooks::note_names`]. `count` takes a fresh list and `get`
//! reads that list, so one host scan sees one consistent set.
//!
//! The host re-reads after `clap_host_note_name.changed`, sent via
//! [`crate::host_handle::HostHandle::note_name_changed`] for a
//! [`HostNotice::NOTE_NAMES`].
//!
//! [`HostNotice::NOTE_NAMES`]: moose_core::main_thread::HostNotice::NOTE_NAMES
//! [`MainThreadHooks::note_names`]: moose_core::main_thread::MainThreadHooks::note_names

use clap_sys::ext::note_name::{clap_note_name, clap_plugin_note_name};
use clap_sys::plugin::clap_plugin;
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use moose_core::cast::len_u32;
use moose_core::export::PluginExport;
use moose_core::main_thread::NoteName;
use moose_core::thread::MainThread;
use moose_core::wrapper::run_extern_callback_with;

use crate::{copy_str_to_buf, data_from_plugin};

pub(crate) fn make_extension<P: PluginExport>() -> clap_plugin_note_name {
    clap_plugin_note_name {
        count: Some(count::<P>),
        get: Some(get::<P>),
    }
}

fn write_note_name(name: &NoteName, out: &mut clap_note_name) {
    out.name = [0; CLAP_NAME_SIZE];
    copy_str_to_buf(&mut out.name, &name.name);
    out.port = name.port.and_then(|p| i16::try_from(p).ok()).unwrap_or(-1);
    out.key = name.key.map_or(-1, i16::from);
    out.channel = name.channel.map_or(-1, i16::from);
}

unsafe extern "C" fn count<P: PluginExport>(plugin: *const clap_plugin) -> u32 {
    // SAFETY: CLAP passes the live instance created for `P`.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    // SAFETY: `clap_plugin_note_name.count` is [main-thread] (note-name.h).
    let main = unsafe { MainThread::assume() };
    run_extern_callback_with::<P, u32>("CLAP", "note_name count", 0, || {
        let names = data
            .with_hooks(&main, |hooks| hooks.note_names(&main))
            .unwrap_or_default();
        let count = len_u32(names.len());
        *data.note_names.borrow_mut(&main) = names;
        count
    })
}

unsafe extern "C" fn get<P: PluginExport>(
    plugin: *const clap_plugin,
    index: u32,
    out: *mut clap_note_name,
) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: CLAP passes the live instance created for `P`.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    // SAFETY: `clap_plugin_note_name.get` is [main-thread] (note-name.h).
    let main = unsafe { MainThread::assume() };
    let names = data.note_names.borrow(&main);
    let Some(name) = usize::try_from(index).ok().and_then(|i| names.get(i)) else {
        return false;
    };
    // SAFETY: the host passes a writable `clap_note_name`, checked non-null.
    write_note_name(name, unsafe { &mut *out });
    true
}
