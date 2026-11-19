//! Publishing parameter-set changes to the host under the CLAP `params.h`
//! rules.
//!
//! The parameter set wakes the wrapper through a [`CatalogSignal`]; the wake
//! only schedules `request_callback`, and `on_main_thread` runs
//! [`sync_parameters`]. Value and info changes rescan at once. A structure
//! change (a slot bound, freed or retyped) needs the instance inactive:
//! while active it requests a restart and keeps the published list, and
//! `deactivate` then swaps the catalog, clears the removed ids and rescans
//! everything.
//!
//! The catalog has two owners. Main-thread callbacks (`params.count`,
//! `params.get_info`, remote controls) read [`ExposedParams`]; the audio
//! thread reads its own `Arc` in the audio cell. Both are replaced together,
//! only while the instance is inactive, so `process` never sees a catalog
//! change under it and never takes a lock.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use clap_sys::ext::params::{
    CLAP_PARAM_CLEAR_ALL, CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_TEXT,
    CLAP_PARAM_RESCAN_VALUES,
};
use moose_core::editor::SendPtr;
use moose_core::export::PluginExport;
use moose_core::thread::MainThread;
use moose_params::{CatalogChange, CatalogSignal, ParamCatalog, ParamInfo, Params};

use crate::{ClapPluginData, refresh_remote_control_pages, request_presentation_refresh};

/// The parameter list the host sees: the catalog and the indices of its
/// available entries. One value, so the two can't disagree.
pub(crate) struct ExposedParams {
    catalog: Arc<ParamCatalog>,
    indices: Vec<usize>,
}

impl ExposedParams {
    pub(crate) fn new(catalog: Arc<ParamCatalog>, params: &dyn Params) -> Self {
        let indices = parameter_indices(&catalog, |id| params.parameter_presentation(id));
        Self { catalog, indices }
    }

    pub(crate) fn len(&self) -> usize {
        self.indices.len()
    }

    /// The info at host index `index`.
    pub(crate) fn info(&self, index: u32) -> Option<&ParamInfo> {
        let index = *self.indices.get(usize::try_from(index).ok()?)?;
        Some(&self.catalog[index])
    }

    /// The exposed infos in host order.
    pub(crate) fn infos(&self) -> impl Iterator<Item = &ParamInfo> {
        self.indices.iter().map(|&i| &self.catalog[i])
    }
}

pub(crate) fn lock_exposed(exposed: &Mutex<ExposedParams>) -> MutexGuard<'_, ExposedParams> {
    exposed.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Indices of the infos whose presentation is available.
pub(crate) fn parameter_indices(
    infos: &[ParamInfo],
    mut presentation: impl FnMut(u32) -> Option<moose_params::ParameterPresentation>,
) -> Vec<usize> {
    infos
        .iter()
        .enumerate()
        .filter_map(|(index, info)| {
            presentation(info.id)
                .is_none_or(|p| p.available)
                .then_some(index)
        })
        .collect()
}

/// [main-thread, init] Bind the parameter set's change signal. Its wake
/// coalesces into one `request_callback` per `on_main_thread`, on the same
/// flag as presentation refreshes.
pub(crate) fn install_catalog_signal<P: PluginExport>(
    data: &ClapPluginData<P>,
) -> Arc<CatalogSignal> {
    // SAFETY: the host outlives the instance, and its `request_callback` is
    // [thread-safe] (host.h), so the pointer may be used from any thread.
    let host = unsafe { SendPtr::new(data.host) };
    let pending = Arc::clone(&data.presentation_pending);
    let signal = Arc::new(CatalogSignal::new(move || {
        // SAFETY: the host outlives the instance, and `destroy` closes the
        // signal before the instance is freed.
        unsafe { request_presentation_refresh(&pending, host.as_ptr()) };
    }));
    data.params_arc.set_catalog_signal(Arc::downgrade(&signal));
    signal
}

/// [main-thread] Publish what changed in the parameter set since the last
/// call: rescan values and info, and handle a structure change by restart
/// (active) or by swapping the catalog (inactive).
///
/// # Safety
/// `data` belongs to a live instance whose host pointer is valid.
pub(crate) unsafe fn sync_parameters<P: PluginExport>(data: &ClapPluginData<P>, main: &MainThread) {
    data.presentation_pending.store(false, Ordering::Release);
    let changes = data.params_arc.take_catalog_changes();
    if changes.flags.contains(CatalogChange::STRUCTURE) {
        data.pending_structure
            .borrow_mut(main)
            .get_or_insert_with(Vec::new)
            .extend(changes.removed);
    }
    let revision = data.params_arc.parameter_presentation_revision();
    let revised = data.presentation_revision.swap(revision, Ordering::Relaxed) != revision;
    let list_dirty = revised
        || data.pending_structure.borrow(main).is_some()
        || data.parameter_restart_pending.load(Ordering::Relaxed);
    // SAFETY: forwarded caller contract.
    let republished = list_dirty && unsafe { publish_list(data, main) };
    if revised && !republished {
        // SAFETY: forwarded caller contract.
        unsafe { refresh_remote_control_pages(data) };
    }

    let mut flags = 0;
    if republished {
        flags |= CLAP_PARAM_RESCAN_ALL;
    }
    if data.needs_rescan.swap(false, Ordering::Relaxed)
        || changes.flags.contains(CatalogChange::VALUES)
    {
        flags |= CLAP_PARAM_RESCAN_VALUES;
    }
    if revised || changes.flags.contains(CatalogChange::INFO) {
        flags |= CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_TEXT;
    }
    if flags != 0
        && !data.host_params.is_null()
        && !data.host.is_null()
        // SAFETY: a non-null host extension stays valid for the instance.
        && let Some(rescan) = unsafe { (*data.host_params).rescan }
    {
        // SAFETY: `rescan` is [main-thread]; RESCAN_ALL is set only after
        // `publish_list` ran inactive.
        unsafe { rescan(data.host, flags) };
    }
}

/// Bring the exposed list up to date. Active: request one restart when the
/// list or the structure changed, and keep the published list. Inactive:
/// rebuild (with a fresh catalog after a structure change), clear removed
/// ids, and return `true` when the host must rescan everything.
unsafe fn publish_list<P: PluginExport>(data: &ClapPluginData<P>, main: &MainThread) -> bool {
    let params = data.params_arc.as_ref() as &dyn Params;
    if data.active.load(Ordering::Relaxed) {
        let structure = data.pending_structure.borrow(main).is_some();
        let list_changed = {
            let exposed = lock_exposed(&data.exposed);
            ExposedParams::new(Arc::clone(&exposed.catalog), params).indices != exposed.indices
        };
        if (structure || list_changed)
            && !data.parameter_restart_pending.swap(true, Ordering::Relaxed)
            && !data.host.is_null()
            // SAFETY: the host outlives the instance.
            && let Some(restart) = unsafe { (*data.host).request_restart }
        {
            // SAFETY: `request_restart` is [thread-safe].
            unsafe { restart(data.host) };
        }
        return false;
    }

    data.parameter_restart_pending
        .store(false, Ordering::Relaxed);
    let removed = data.pending_structure.borrow_mut(main).take();
    {
        let mut exposed = lock_exposed(&data.exposed);
        let catalog = if removed.is_some() {
            params.param_catalog()
        } else {
            Arc::clone(&exposed.catalog)
        };
        let next = ExposedParams::new(catalog, params);
        if removed.is_none() && next.indices == exposed.indices {
            return false;
        }
        if removed.is_some() {
            // Inactive, on the main thread: no `process` or audio-thread
            // `flush` can hold the audio cell (CLAP: both need an active
            // instance), the same ownership `activate` relies on.
            data.audio.enter().catalog = Arc::clone(&next.catalog);
        }
        *exposed = next;
    }
    // SAFETY: main thread, live instance.
    unsafe { refresh_remote_control_pages(data) };
    if let Some(removed) = removed
        && !data.host_params.is_null()
        // SAFETY: a non-null host extension stays valid for the instance.
        && let Some(clear) = unsafe { (*data.host_params).clear }
    {
        for id in removed {
            // SAFETY: `clear` is [main-thread].
            unsafe { clear(data.host, id, CLAP_PARAM_CLEAR_ALL) };
        }
    }
    true
}
