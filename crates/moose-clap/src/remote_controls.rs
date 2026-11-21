//! Extension: `clap.remote-controls`.

use crate::catalog_sync;
use crate::copy_str_to_buf;
use crate::data::ClapPluginData;
use crate::data_from_plugin;

use clap_sys::ext::remote_controls::{
    CLAP_REMOTE_CONTROLS_COUNT, clap_plugin_remote_controls, clap_remote_controls_page,
};
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::clap_plugin;
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use moose_core::cast::len_u32;
use moose_core::export::PluginExport;
use moose_params::Params;
use moose_params::{ParamFlags, ParamInfo};

// ---------------------------------------------------------------------------
// Extension: remote_controls
// ---------------------------------------------------------------------------

/// Splits a `ParamInfo::group` into a CLAP remote-controls
/// `(section_name, page_name)` pair on its first `/`, mirroring the
/// convention the `params` extension's `module` path already uses
/// ("/" as a tree separator). `"EQ/Lo Shelf"` becomes section `"EQ"`,
/// page `"Lo Shelf"` - a host can offer one "EQ" button that cycles
/// through each band's page. A group with no `/` is its own single-page
/// section, e.g. `"Compressor"` stays as-is. Only the first slash
/// splits, so a page name may itself contain one.
pub(crate) fn split_group(group: &str) -> (&str, &str) {
    match group.split_once('/') {
        Some((section, page)) => (section, page),
        None => (group, group),
    }
}

/// Chunks a plugin's params into pages of at most
/// `CLAP_REMOTE_CONTROLS_COUNT` (8), one run of pages per distinct
/// non-empty `ParamInfo::group`. A remote-control slot is a scarce
/// performance control (8 per page), so a param is skipped when it has
/// no group (opt-out; the host's generic list covers it) or is `HIDDEN`
/// / `READONLY`. `group` has two consumers - the `params` extension's
/// `module` path (pure tree organization, sensible even for a hidden
/// param) and these slots - so grouping a hidden param for tidiness must
/// not silently burn one of the user's knobs on something they can't
/// see or move.
#[cfg(test)]
pub(crate) fn remote_control_pages<'a>(
    infos: impl IntoIterator<Item = &'a ParamInfo>,
) -> Vec<(&'a str, Vec<u32>)> {
    let mut pages: Vec<(&str, Vec<u32>)> = Vec::new();
    for info in infos {
        if info.group.is_empty()
            || info
                .flags
                .intersects(ParamFlags::HIDDEN | ParamFlags::READONLY)
        {
            continue;
        }
        let existing = pages
            .iter()
            .rposition(|page| page.0 == info.group && page.1.len() < CLAP_REMOTE_CONTROLS_COUNT);
        match existing {
            Some(idx) => pages[idx].1.push(info.id),
            None => pages.push((info.group, vec![info.id])),
        }
    }
    pages
}

/// Resolve runtime names, groups and visibility before paginating. IDs and
/// storage remain static; a hidden/unavailable control never consumes a knob.
pub(crate) fn presented_remote_control_pages(
    infos: impl IntoIterator<Item = impl std::borrow::Borrow<ParamInfo>>,
    mut presentation: impl FnMut(u32) -> Option<moose_params::ParameterPresentation>,
) -> Vec<(String, Vec<u32>)> {
    let mut pages: Vec<(String, Vec<u32>)> = Vec::new();
    for info in infos {
        let info = info.borrow();
        let runtime = presentation(info.id);
        let group = runtime.as_ref().map_or(info.group, |p| p.group.as_str());
        let hidden = runtime.as_ref().map_or_else(
            || info.flags.contains(ParamFlags::HIDDEN),
            |p| p.hidden || !p.available,
        );
        if group.is_empty() || hidden || info.flags.contains(ParamFlags::READONLY) {
            continue;
        }
        if let Some(page) = pages
            .iter_mut()
            .rfind(|p| p.0 == group && p.1.len() < CLAP_REMOTE_CONTROLS_COUNT)
        {
            page.1.push(info.id);
        } else {
            pages.push((group.to_owned(), vec![info.id]));
        }
    }
    pages
}

/// Only structural presentation revisions rebuild the cache, never value flushes.
pub(crate) unsafe fn refresh_remote_control_pages<P: PluginExport>(
    data: &ClapPluginData<P>,
    main: &moose_core::thread::MainThread,
) {
    let exposed = catalog_sync::lock_exposed(&data.exposed);
    let pages = presented_remote_control_pages(exposed.infos(), |id| {
        data.params_arc.parameter_presentation(id)
    });
    drop(exposed);
    let mut current = data
        .remote_pages
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *current == pages {
        return;
    }
    *current = pages;
    drop(current);
    // SAFETY: caller guarantees `data.host` is live; `changed` is `[main-thread]`.
    unsafe { data.host.on_main_thread(main).remote_controls_changed() };
}

/// Deterministic page id from the group name and its chunk index among
/// same-named pages, so a page's id survives an unrelated reorder of
/// `Params::param_infos()`. Hand-rolled FNV-1a rather than `std`'s
/// `DefaultHasher` - that one is keyed by `RandomState` and isn't
/// guaranteed stable across processes, which a host may use `page_id`
/// to remember a user's page choice across.
pub(crate) fn remote_controls_page_id(group: &str, chunk_index: usize) -> clap_id {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in group.as_bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    for &b in &chunk_index.to_le_bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

pub(crate) unsafe extern "C" fn remote_controls_count<P: PluginExport>(
    plugin: *const clap_plugin,
) -> u32 {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        len_u32(
            data.remote_pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
        )
    }
}

pub(crate) unsafe extern "C" fn remote_controls_get<P: PluginExport>(
    plugin: *const clap_plugin,
    page_index: u32,
    out: *mut clap_remote_controls_page,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let pages = data
            .remote_pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let page_index = page_index as usize;
        if page_index >= pages.len() {
            return false;
        }
        let group: &str = &pages[page_index].0;
        let ids: &[u32] = &pages[page_index].1;

        // Chunk index among pages sharing this group name, so two pages
        // from the same >8-param group get distinct, stable ids.
        let chunk_index = pages[..page_index]
            .iter()
            .filter(|page| page.0 == group)
            .count();

        let (section_name, page_name) = split_group(group);

        let out = &mut *out;
        out.section_name = [0; CLAP_NAME_SIZE];
        copy_str_to_buf(&mut out.section_name, section_name);
        out.page_id = remote_controls_page_id(group, chunk_index);
        out.page_name = [0; CLAP_NAME_SIZE];
        copy_str_to_buf(&mut out.page_name, page_name);
        out.param_ids = [CLAP_INVALID_ID; CLAP_REMOTE_CONTROLS_COUNT];
        for (slot, &id) in out.param_ids.iter_mut().zip(ids.iter()) {
            *slot = id;
        }
        out.is_for_preset = false;

        true
    }
}

pub(crate) fn make_remote_controls_extension<P: PluginExport>() -> clap_plugin_remote_controls {
    clap_plugin_remote_controls {
        count: Some(remote_controls_count::<P>),
        get: Some(remote_controls_get::<P>),
    }
}
