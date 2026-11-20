//! Extensions: `clap.state` and `clap.preset-load`.

use crate::data::ClapPluginData;
use crate::data_from_plugin;

use clap_sys::ext::preset_load::clap_plugin_preset_load;
use clap_sys::ext::state::clap_plugin_state;
use clap_sys::ext::state_context::clap_plugin_state_context_type;
use clap_sys::factory::preset_discovery::{
    CLAP_PRESET_DISCOVERY_LOCATION_FILE, clap_preset_discovery_location_kind,
};
use clap_sys::plugin::clap_plugin;
use clap_sys::stream::{clap_istream, clap_ostream};
use moose_core::events::{EventBody, EventList};
use moose_core::export::PluginExport;
use moose_core::host_queue::HostSignal;
use moose_core::presets::parse_preset_file;
use moose_core::state;
use moose_core::state::PluginFormat;
use moose_core::wrapper::{apply_or_queue_state, custom_state_for_save, run_extern_callback_with};
use moose_params::Params;
use std::ffi::{CStr, c_char, c_void};
use std::path::Path;
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// Extension: state
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn state_save<P: PluginExport>(
    plugin: *const clap_plugin,
    stream: *const clap_ostream,
) -> bool {
    // Guard the user's `save_state()` against panics so a stray
    // `unwrap` in custom-state code degrades to "save failed" instead
    // of aborting the host across this `extern "C"` boundary.
    run_extern_callback_with::<P, bool>("CLAP", "save_state", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        let (ids, values) = data.params_arc.collect_values();
        // Live custom state: serialized here when processing is stopped
        // (the host thread owns the plugin, and drains any queued load
        // first), otherwise requested from the audio thread's next block.
        let extra = custom_state_for_save(
            &data.plugin,
            &data.snapshot,
            &data.pending_state,
            !data.processing.load(Ordering::Acquire),
        );
        let Ok(persist) = data.params_arc.prepare_persist_write() else {
            return false;
        };
        let Some(write_fn) = (*stream).write else {
            return false;
        };
        state::write_plugin_state_stream::<P>(
            data.plugin_id_hash,
            &data.params_arc,
            &ids,
            &values,
            &extra,
            persist.byte_len(),
            P::max_host_state_bytes(),
            |output| moose_params::write_prepared_persist(persist.as_ref(), output),
            |bytes| write_fn(stream, bytes.as_ptr().cast::<c_void>(), bytes.len() as u64),
        )
    })
}

/// Largest state blob a host stream may hand us. Anything bigger is a
/// misbehaving stream (or an unrelated file), not a moose state.
#[cfg(test)]
pub(crate) const MAX_HOST_STATE_BYTES: usize = 32 * 1024 * 1024;

/// Drain a host `clap_istream` into one bounded blob. `read` fills the
/// buffer and returns the host's byte count: `0` is end of stream, a
/// negative count is a read error. A count larger than the buffer, or a
/// stream past [`MAX_HOST_STATE_BYTES`], fails the load before any
/// further allocation.
#[cfg(test)]
pub(crate) fn read_host_state(mut read: impl FnMut(&mut [u8]) -> i64) -> Option<Vec<u8>> {
    let mut blob = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = usize::try_from(read(&mut buf)).ok()?;
        if n == 0 {
            return Some(blob);
        }
        if n > buf.len() || blob.len() + n > MAX_HOST_STATE_BYTES {
            return None;
        }
        blob.extend_from_slice(&buf[..n]);
    }
}

pub(crate) unsafe extern "C" fn state_load<P: PluginExport>(
    plugin: *const clap_plugin,
    stream: *const clap_istream,
) -> bool {
    // Guard the user's `load_state()` / custom deserialize against
    // panics so a malformed blob degrades to "load failed" instead of
    // aborting the host across this `extern "C"` boundary.
    run_extern_callback_with::<P, bool>("CLAP", "load_state", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);

        let Some(read_fn) = (*stream).read else {
            return false;
        };

        let backup = state::ParamsBackup::capture(&*data.params_arc);
        let Some(deserialized) = state::read_and_apply_params::<P>(
            |buf| read_fn(stream, buf.as_mut_ptr().cast::<c_void>(), buf.len() as u64),
            data.plugin_id_hash,
            state::PluginFormat::Clap,
            None,
            &*data.params_arc,
            P::max_host_state_bytes(),
        ) else {
            return false;
        };

        // Apply params synchronously on the host thread (atomic-safe)
        // so host queries that read parameter values right after
        // `clap_plugin_state.load` see the restored values without
        // first running a process block - clap-validator reads back
        // immediately after a load round-trip.

        if !load_custom_state(data, deserialized, &backup) {
            return false;
        }

        if let Some(ref mut editor) = data.gui.enter().editor {
            editor.state_changed();
        }
        request_value_rescan(data);

        true
    })
}

/// Apply a recalled state's custom part: synchronously on the host thread
/// while processing is stopped (inactive, or active between
/// `stop_processing` and `start_processing`), else through the audio
/// thread with a bounded wait. Either way the editor's `state_changed`,
/// which the callers fire next, sees the applied state (or, if no block ran
/// in time, reads the queued bytes through `custom_state_view`).
/// `false`: the plugin's `load_state` rejected it and params and
/// `#[persist]` were put back; the caller reports failure to the host.
pub(crate) fn load_custom_state<P: PluginExport>(
    data: &ClapPluginData<P>,
    deserialized: state::DeserializedState,
    backup: &state::ParamsBackup,
) -> bool {
    let can_own = !data.processing.load(Ordering::Acquire);
    if can_own && let Some(mut audio) = data.audio.try_enter() {
        // A stopped-transport params.flush may have queued a value for the
        // next block. Recall supersedes it just as active recall supersedes
        // deferred events in convert_input_events.
        audio.deferred_params.clear();
    }
    match apply_or_queue_state(
        &data.plugin,
        &*data.params_arc,
        &data.snapshot,
        &data.pending_state,
        deserialized,
        backup,
        can_own,
    ) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("moose: CLAP state load rejected by load_state: {e}");
            false
        }
    }
}

/// Metadata work belongs on the host main thread, including when an active
/// plugin receives `params.flush` on its audio thread (CLAP 1.2.7 params.h).
pub(crate) unsafe fn request_changed_presentation<P: PluginExport>(
    data: &ClapPluginData<P>,
    events: &EventList,
) {
    if events.iter().any(|event| {
        matches!(event.body, EventBody::ParamChange { id, .. }
            if data.params_arc.parameter_affects_presentation(id))
    }) {
        data.host_queue.signal(HostSignal::PRESENTATION);
    }
}

/// A recalled state may change every parameter behind the host's back, and
/// the host must hear it through `clap_host_params::rescan`. Coalesce onto
/// `on_main_thread` like editor-originated changes, so a host that loads
/// state from another thread still gets the rescan on the main thread.
pub(crate) unsafe fn request_value_rescan<P: PluginExport>(data: &ClapPluginData<P>) {
    data.host_queue.signal(HostSignal::PARAM_RESCAN);
}

/// `state-context`: every context saves and loads the full state. Moose
/// state has no project-only or preset-only parts to leave out.
pub(crate) unsafe extern "C" fn state_context_save<P: PluginExport>(
    plugin: *const clap_plugin,
    stream: *const clap_ostream,
    _context: clap_plugin_state_context_type,
) -> bool {
    unsafe { state_save::<P>(plugin, stream) }
}

pub(crate) unsafe extern "C" fn state_context_load<P: PluginExport>(
    plugin: *const clap_plugin,
    stream: *const clap_istream,
    _context: clap_plugin_state_context_type,
) -> bool {
    unsafe { state_load::<P>(plugin, stream) }
}

pub(crate) fn make_state_extension<P: PluginExport>() -> clap_plugin_state {
    clap_plugin_state {
        save: Some(state_save::<P>),
        load: Some(state_load::<P>),
    }
}

// ---------------------------------------------------------------------------
// Extension: preset-load
// ---------------------------------------------------------------------------

/// Largest `.trucepreset` container `preset_load` will read from disk for
/// a plugin with the default 32 MiB host-state ceiling (state envelope plus
/// 64 KiB headroom for the `TRPS` header and `key=value` metadata block).
pub(crate) const MAX_PRESET_FILE_BYTES: usize = 32 * 1024 * 1024 + 64 * 1024;
const PRESET_METADATA_HEADROOM_BYTES: usize = 64 * 1024;

pub(crate) fn read_preset_file_bounded(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().is_ok_and(|m| m.len() > max_bytes as u64) {
        return None;
    }
    let limit = (max_bytes as u64).saturating_add(1);
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= max_bytes).then_some(bytes)
}

/// `CLAP_EXT_PRESET_LOAD.from_location`: read a `.trucepreset` file
/// the discovery provider surfaced and apply its embedded state
/// envelope - the same host-thread-apply + audio-thread-handoff path
/// `state_load` takes, so preset recall and session restore are one
/// code path from here down.
pub(crate) unsafe extern "C" fn preset_load_from_location<P: PluginExport>(
    plugin: *const clap_plugin,
    location_kind: clap_preset_discovery_location_kind,
    location: *const c_char,
    load_key: *const c_char,
) -> bool {
    // Runs author `migrate_state`, and (on the inactive branch) a
    // synchronous `load_state` + `editor.state_changed()`. Firewall it like
    // `state_load` so a panic degrades to "load failed" instead of
    // unwinding across this `extern "C"` boundary and aborting the host
    // mid-preset-browse.
    run_extern_callback_with::<P, bool>("CLAP", "preset_load", false, || unsafe {
        if location_kind != CLAP_PRESET_DISCOVERY_LOCATION_FILE || location.is_null() {
            return false;
        }
        let Ok(path) = CStr::from_ptr(location).to_str() else {
            return false;
        };
        let data = data_from_plugin::<P>(plugin);

        // Same classification as a session load: an envelope under a
        // different plugin id (a pre-identity-change save) is offered
        // to the plugin's `migrate_state` hook instead of refused
        // outright. Discovery only lists same-id presets, but a host
        // can hand any path here (drag-drop, saved location).
        let max_bytes = P::max_host_state_bytes()
            .saturating_add(PRESET_METADATA_HEADROOM_BYTES)
            .max(MAX_PRESET_FILE_BYTES);
        let Some(bytes) = read_preset_file_bounded(Path::new(path), max_bytes) else {
            return false;
        };
        let Some((_, blob)) = parse_preset_file(&bytes) else {
            return false;
        };
        let Some(mut deserialized) =
            state::parse_or_migrate::<P>(&blob, data.plugin_id_hash, PluginFormat::Clap, None)
        else {
            return false;
        };
        if !deserialized.named_params.is_empty() {
            state::resolve_named_params(&mut deserialized, &data.params_arc.param_catalog());
        }

        let backup = state::ParamsBackup::capture(&*data.params_arc);
        state::apply_params(&*data.params_arc, &deserialized);
        if !load_custom_state(data, deserialized, &backup) {
            return false;
        }
        if let Some(ref mut editor) = data.gui.enter().editor {
            editor.state_changed();
        }
        request_value_rescan(data);

        // Tell the host the preset landed so it can update its
        // preset chrome (Bitwig's preset name display reads this).
        // SAFETY: `preset_load.from_location` is `[main-thread]`.
        let main = moose_core::thread::MainThread::assume();
        data.host
            .on_main_thread(&main)
            .preset_loaded(location_kind, location, load_key);
        true
    })
}

pub(crate) fn make_preset_load_extension<P: PluginExport>() -> clap_plugin_preset_load {
    clap_plugin_preset_load {
        from_location: Some(preset_load_from_location::<P>),
    }
}
