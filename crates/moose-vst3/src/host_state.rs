//! Streaming state save and load (zero `libc::malloc`/`free` staging copies).

use crate::com;
use crate::instance::Vst3Instance;

use moose_core::export::PluginExport;
use moose_core::host_queue::HostSignal;
use moose_core::state;
use moose_core::wrapper::{apply_or_queue_state, custom_state_for_save, run_extern_callback_with};
use moose_params::Params;
use std::sync::atomic::Ordering;

pub(crate) fn apply_host_state<P: PluginExport>(
    inst: &Vst3Instance<P>,
    deserialized: state::DeserializedState,
    backup: &state::ParamsBackup,
    params_applied: bool,
) -> bool {
    if !params_applied {
        state::apply_params(&*inst.params_arc, &deserialized);
    }
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
        return false;
    }
    // Isolate the editor notification: a panic in `state_changed` must not turn
    // an already-accepted load into `false` or skip `PARAM_RESCAN`.
    // `try_enter`, not `enter`: a host can deliver `setState` synchronously
    // from a component-handler callback while an outer GUI callback holds the cell.
    run_extern_callback_with::<P, ()>("vst3", "editor_state_changed", (), || {
        if let Some(mut gui) = inst.gui.try_enter()
            && let Some(ref mut editor) = gui.editor
        {
            editor.state_changed();
        }
    });
    inst.host_queue.signal(HostSignal::PARAM_RESCAN);
    com::flush_host_queue(inst.host_id());
    true
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

impl<P: PluginExport> Vst3Instance<P> {
    pub(crate) fn state_save_stream_impl(&self, write: &mut dyn FnMut(&[u8]) -> i64) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "save_state_stream", false, || {
            let (ids, values) = self.params_arc.collect_values();
            let extra = save_custom_state(self);
            let Ok(persist) = self.params_arc.prepare_persist_write() else {
                return false;
            };
            state::write_plugin_state_stream::<P>(
                self.plugin_id_hash,
                &self.params_arc,
                &ids,
                &values,
                &extra,
                persist.byte_len(),
                P::max_host_state_bytes(),
                |output| moose_params::write_prepared_persist(persist.as_ref(), output),
                write,
            )
        })
    }

    pub(crate) fn state_load_stream_impl(&self, read: &mut dyn FnMut(&mut [u8]) -> i64) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "load_state_stream", false, || {
            let backup = state::ParamsBackup::capture(&*self.params_arc);
            let Some(deserialized) = state::read_and_apply_params::<P>(
                read,
                self.plugin_id_hash,
                state::PluginFormat::Vst3,
                None,
                &*self.params_arc,
                P::max_host_state_bytes(),
            ) else {
                return false;
            };
            apply_host_state(self, deserialized, &backup, true)
        })
    }

    #[cfg(test)]
    pub(crate) fn save_state_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(self.state_save_stream_impl(&mut |chunk| {
            out.extend_from_slice(chunk);
            i64::try_from(chunk.len()).unwrap_or(-1)
        }));
        out
    }

    #[cfg(test)]
    pub(crate) fn load_state_bytes(&self, blob: &[u8]) -> bool {
        let mut slice = blob;
        self.state_load_stream_impl(&mut |dst| {
            let take = slice.len().min(dst.len());
            dst[..take].copy_from_slice(&slice[..take]);
            slice = &slice[take..];
            i64::try_from(take).unwrap_or(-1)
        })
    }
}
