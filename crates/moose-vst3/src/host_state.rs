//! State save and load (component and controller chunks).

use crate::ffi;
use crate::instance::Vst3Instance;

use moose_core::export::PluginExport;
use moose_core::host_queue::HostSignal;
use moose_core::state;
use moose_core::wrapper::{apply_or_queue_state, custom_state_for_save, run_extern_callback_with};
use moose_params::Params;
use std::slice;
use std::sync::atomic::Ordering;

pub(crate) unsafe extern "C" fn cb_state_save<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
    out_len: *mut u32,
) {
    // Pre-zero the out pointers so a panic anywhere in the body below
    // leaves the host seeing an empty blob rather than a stale buffer
    // pointer paired with whatever length was last written. The body
    // overwrites these on the happy path.
    unsafe {
        *out_data = std::ptr::null_mut();
        *out_len = 0;
    }
    run_extern_callback_with::<P, ()>("vst3", "save_state", (), || unsafe {
        // Own exactly one contiguous host buffer. RAII frees failed or panicked
        // partial writes; successful transfer retains the original libc ABI.
        struct Buffer(*mut u8);
        impl Drop for Buffer {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe {
                        libc_free(self.0.cast());
                    }
                }
            }
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let (ids, values) = inst.params_arc.collect_values();
        let extra = save_custom_state(inst);
        // Allocator pin: this wrapper allocates with `libc_malloc` and
        // the C++ shim frees with `libc::free`. The Rust global
        // allocator must not appear on either side. (VST2 uses the
        // Rust global allocator for both save + free; do not cross
        // wires when refactoring `_save_state` paths together.)
        let Ok(persist) = inst.params_arc.prepare_persist_write() else {
            return;
        };
        let Some(len) = state::plugin_state_stream_size::<P>(
            &inst.params_arc,
            &ids,
            &values,
            &extra,
            persist.byte_len(),
            P::max_host_state_bytes(),
        ) else {
            return;
        };
        let Ok(host_len) = u32::try_from(len) else {
            return;
        };
        let mut buffer = Buffer(libc_malloc(len).cast::<u8>());
        if buffer.0.is_null() {
            return;
        }
        let mut offset = 0;
        let ok = state::write_plugin_state_stream::<P>(
            inst.plugin_id_hash,
            &inst.params_arc,
            &ids,
            &values,
            &extra,
            persist.byte_len(),
            P::max_host_state_bytes(),
            |output| moose_params::write_prepared_persist(persist.as_ref(), output),
            |bytes| {
                if bytes.len() > len - offset {
                    return -1;
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.0.add(offset), bytes.len());
                offset += bytes.len();
                i64::try_from(bytes.len()).unwrap_or(-1)
            },
        );
        if !ok {
            return;
        }
        *out_data = buffer.0;
        *out_len = host_len;
        buffer.0 = std::ptr::null_mut();
    });
}

pub(crate) unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) -> i32 {
    run_extern_callback_with::<P, i32>("vst3", "load_state", 0, || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        // `slice::from_raw_parts(null, n)` for `n > 0` is UB. Treat
        // `(null, *)` and `(_, 0)` the same as "host gave us nothing".
        if data.is_null() || len == 0 || len as usize > P::max_host_state_bytes() {
            return 0;
        }
        let blob = slice::from_raw_parts(data, len as usize);
        let backup = state::ParamsBackup::capture(&*inst.params_arc);
        // Not this plugin's envelope? Offer the bytes to the plugin's
        // `migrate_state` hook (legacy sessions from a pre-moose
        // build); `None` fails the load honestly.
        let Some(mut deserialized) = state::parse_or_migrate::<P>(
            blob,
            inst.plugin_id_hash,
            state::PluginFormat::Vst3,
            None,
        ) else {
            return 0;
        };
        if !deserialized.named_params.is_empty() {
            state::resolve_named_params(&mut deserialized, &inst.params_arc.param_catalog());
        }
        apply_host_state(inst, deserialized, &backup, false)
    })
}

pub(crate) unsafe fn apply_host_state<P: PluginExport>(
    inst: &Vst3Instance<P>,
    deserialized: state::DeserializedState,
    backup: &state::ParamsBackup,
    params_applied: bool,
) -> i32 {
    // Apply params synchronously on the host thread (atomic-safe)
    // so host-side queries that read parameter values right
    // after `setState` see the restored values without first
    // running a process block. pluginval / DAW preset reload
    // both observe this.
    if !params_applied {
        state::apply_params(&*inst.params_arc, &deserialized);
    }
    // Not processing (inactive, or between setProcessing(false) and
    // (true)): applied here on the host thread. Processing: queued for the
    // audio thread with a bounded wait, so the editor notify below sees it.
    // A rejected `load_state` puts params and `#[persist]` back and fails
    // the load (kResultFalse), so the host never sees half a state.
    if let Err(e) = apply_or_queue_state(
        &inst.plugin,
        &*inst.params_arc,
        &inst.snapshot,
        &inst.pending_state,
        deserialized,
        backup,
        !inst.processing.load(Ordering::Acquire),
    ) {
        eprintln!("[moose VST3] state load rejected by load_state: {e}");
        return 0;
    }
    // `try_enter`, not `enter`: a host can deliver `setState`
    // synchronously from a component-handler callback while an outer
    // GUI callback (e.g. `cb_gui_open`, which holds the cell across the
    // author's `editor.open`) still holds the cell. Re-entering with
    // `enter` would hand out a second aliasing `&mut` in release. When
    // busy, skip the notify: the editor reads live param/state values as
    // it finishes opening, so it renders the freshly-loaded state anyway.
    if let Some(mut gui) = inst.gui.try_enter()
        && let Some(ref mut editor) = gui.editor
    {
        editor.state_changed();
    }
    // Delivered now on the UI thread, else left for the next drain.
    inst.host_queue.signal(HostSignal::PARAM_RESCAN);
    ffi::moose_vst3_flush_host_queue(std::ptr::from_ref(inst).cast_mut().cast());
    1
}

/// Live custom state for `getState`; see `wrapper::custom_state_for_save`.
pub(crate) fn save_custom_state<P: PluginExport>(inst: &Vst3Instance<P>) -> Vec<u8> {
    custom_state_for_save(
        &inst.plugin,
        &inst.snapshot,
        &inst.pending_state,
        !inst.processing.load(Ordering::Acquire),
    )
}

pub(crate) unsafe extern "C" fn cb_state_save_stream<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    stream: *mut std::ffi::c_void,
    write: unsafe extern "C" fn(*mut std::ffi::c_void, *const u8, u32) -> i64,
) -> i32 {
    run_extern_callback_with::<P, i32>("vst3", "save_state_stream", 0, || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let (ids, values) = inst.params_arc.collect_values();
        let extra = save_custom_state(inst);
        let Ok(persist) = inst.params_arc.prepare_persist_write() else {
            return 0;
        };
        i32::from(state::write_plugin_state_stream::<P>(
            inst.plugin_id_hash,
            &inst.params_arc,
            &ids,
            &values,
            &extra,
            persist.byte_len(),
            P::max_host_state_bytes(),
            |output| moose_params::write_prepared_persist(persist.as_ref(), output),
            |bytes| u32::try_from(bytes.len()).map_or(-1, |len| write(stream, bytes.as_ptr(), len)),
        ))
    })
}

pub(crate) unsafe extern "C" fn cb_state_load_stream<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    stream: *mut std::ffi::c_void,
    read: unsafe extern "C" fn(*mut std::ffi::c_void, *mut u8, u32) -> i64,
) -> i32 {
    run_extern_callback_with::<P, i32>("vst3", "load_state_stream", 0, || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let backup = state::ParamsBackup::capture(&*inst.params_arc);
        let Some(deserialized) = state::read_and_apply_params::<P>(
            |buf| u32::try_from(buf.len()).map_or(-1, |len| read(stream, buf.as_mut_ptr(), len)),
            inst.plugin_id_hash,
            state::PluginFormat::Vst3,
            None,
            &*inst.params_arc,
            P::max_host_state_bytes(),
        ) else {
            return 0;
        };
        apply_host_state(inst, deserialized, &backup, true)
    })
}

pub(crate) unsafe extern "C" fn cb_state_free(data: *mut u8, _len: u32) {
    unsafe {
        if !data.is_null() {
            libc_free(data.cast::<std::ffi::c_void>());
        }
    }
}

unsafe extern "C" {
    pub(crate) fn malloc(size: usize) -> *mut std::ffi::c_void;
    pub(crate) fn free(ptr: *mut std::ffi::c_void);
}
pub(crate) unsafe fn libc_malloc(size: usize) -> *mut std::ffi::c_void {
    unsafe { malloc(size) }
}
pub(crate) unsafe fn libc_free(ptr: *mut std::ffi::c_void) {
    unsafe { free(ptr) }
}
