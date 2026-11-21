//! Per-instance live parameter catalog & clump table for AU v2 and AU v3.
//!
//! Replaces the legacy process-global C `g_parameter_catalog` so multiple
//! instances of the same plugin with dynamic presentation or `DynamicParams`
//! never alias or corrupt each other.

use crate::ffi::{
    AU_PARAM_FLAG_CAN_RAMP, AU_PARAM_FLAG_HIDDEN, AU_PARAM_FLAG_IS_BYPASS, AU_PARAM_FLAG_READ_ONLY,
    AuParamInfoOut,
};
use crate::midi_status_byte;
use moose_core::cast::len_u32;
use moose_core::info::Automation;
use moose_core::wrapper::copy_c_str;
use moose_params::{CatalogChange, MidiSource, ParamCatalog, ParamFlags, ParamInfo, Params};
use std::collections::HashMap;
use std::os::raw::c_char;
use std::sync::Arc;

struct ExposedEntry {
    catalog_index: usize,
    name: String,
    group: String,
    clump_id: u32,
    hidden: bool,
}

/// Per-instance exposed parameter list, O(1) ID lookup, and clump names.
/// Owned behind a `Mutex` on `AuInstance`; queried only from main/control
/// thread property callbacks and rebuilt on `PRESENTATION` / catalog changes.
pub(crate) struct AuExposedParams {
    catalog: Arc<ParamCatalog>,
    entries: Vec<ExposedEntry>,
    by_id: HashMap<u32, usize>,
    clump_names: Vec<String>,
    automation: Automation,
    #[cfg_attr(
        not(any(target_vendor = "apple", test)),
        allow(
            dead_code,
            reason = "read by AuExposedParams::sync on Apple targets and in tests"
        )
    )]
    presentation_revision: u64,
}

impl AuExposedParams {
    pub(crate) fn new(
        catalog: Arc<ParamCatalog>,
        params: &dyn Params,
        automation: Automation,
    ) -> Self {
        let presentation_revision = params.parameter_presentation_revision();
        Self::build(catalog, params, automation, presentation_revision)
    }

    fn build(
        catalog: Arc<ParamCatalog>,
        params: &dyn Params,
        automation: Automation,
        presentation_revision: u64,
    ) -> Self {
        let mut entries = Vec::with_capacity(catalog.len());
        let mut by_id = HashMap::with_capacity(catalog.len());
        let mut clump_by_name: HashMap<String, u32> = HashMap::new();
        let mut clump_names: Vec<String> = Vec::new();

        for (catalog_index, info) in catalog.iter().enumerate() {
            let pres = params.parameter_presentation(info.id);
            if pres.as_ref().is_some_and(|p| !p.available) {
                continue;
            }
            let name = pres
                .as_ref()
                .filter(|p| !p.name.is_empty())
                .map_or_else(|| info.name.to_owned(), |p| p.name.clone());
            let group = pres
                .as_ref()
                .map_or_else(|| info.group.to_owned(), |p| p.group.clone());
            let hidden =
                info.flags.contains(ParamFlags::HIDDEN) || pres.as_ref().is_some_and(|p| p.hidden);
            let clump_id = if group.is_empty() {
                0
            } else if let Some(&id) = clump_by_name.get(&group) {
                id
            } else {
                let id = len_u32(clump_names.len() + 1);
                clump_names.push(group.clone());
                clump_by_name.insert(group.clone(), id);
                id
            };
            let entry_idx = entries.len();
            entries.push(ExposedEntry {
                catalog_index,
                name,
                group,
                clump_id,
                hidden,
            });
            by_id.insert(info.id, entry_idx);
        }

        Self {
            catalog,
            entries,
            by_id,
            clump_names,
            automation,
            presentation_revision,
        }
    }

    /// Synchronize against `params` on the main thread. Returns
    /// `(values_changed, presentation_changed)` plus the fresh `Arc<ParamCatalog>`
    /// whenever the underlying catalog structure changed.
    #[cfg_attr(
        not(any(target_vendor = "apple", test)),
        allow(
            dead_code,
            reason = "called by drain_host_queue on Apple targets and in tests"
        )
    )]
    pub(crate) fn sync(&mut self, params: &dyn Params) -> (bool, bool, Option<Arc<ParamCatalog>>) {
        let changes = params.take_catalog_changes();
        let revision = params.parameter_presentation_revision();
        let revised = revision != self.presentation_revision;
        let structure_changed = changes.flags.contains(CatalogChange::STRUCTURE);
        let info_changed = changes.flags.contains(CatalogChange::INFO) || revised;
        let values_changed = changes.flags.contains(CatalogChange::VALUES);

        let mut new_catalog = None;
        if structure_changed || info_changed {
            // Re-fetch `param_catalog()` on INFO changes too (e.g. dynamic
            // slot default updates) so `fill_info` and audio-thread metadata
            // never keep stale `ParamInfo` fields.
            let fresh = params.param_catalog();
            new_catalog = Some(Arc::clone(&fresh));
            *self = Self::build(fresh, params, self.automation, revision);
        }

        (
            values_changed,
            structure_changed || info_changed,
            new_catalog,
        )
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn write_ids(&self, out_ids: *mut u32, cap: u32) -> u32 {
        if !out_ids.is_null() && cap > 0 {
            let n = self.entries.len().min(cap as usize);
            for (i, entry) in self.entries.iter().take(n).enumerate() {
                unsafe {
                    *out_ids.add(i) = self.catalog[entry.catalog_index].id;
                }
            }
        }
        len_u32(self.entries.len())
    }

    pub(crate) fn fill_info(&self, id: u32, out: &mut AuParamInfoOut) -> bool {
        let Some(&idx) = self.by_id.get(&id) else {
            return false;
        };
        let entry = &self.entries[idx];
        let info: &ParamInfo = &self.catalog[entry.catalog_index];
        let step_count = info.range.step_count().map_or(0, std::num::NonZero::get);
        let mut flags = 0_u32;
        if info.flags.contains(ParamFlags::READONLY) {
            flags |= AU_PARAM_FLAG_READ_ONLY;
        }
        if entry.hidden {
            flags |= AU_PARAM_FLAG_HIDDEN;
        }
        if info.flags.contains(ParamFlags::IS_BYPASS) {
            flags |= AU_PARAM_FLAG_IS_BYPASS;
        }
        let can_ramp = step_count == 0
            && !info.flags.contains(ParamFlags::READONLY)
            && (matches!(self.automation, Automation::PerSample)
                || info.flags.contains(ParamFlags::CHUNKED));
        if can_ramp {
            flags |= AU_PARAM_FLAG_CAN_RAMP;
        }
        *out = AuParamInfoOut {
            id: info.id,
            step_count,
            clump_id: entry.clump_id,
            flags,
            min: info.range.min(),
            max: info.range.max(),
            default_value: info.default_plain,
            midi_status: info.midi_map.map_or(0, midi_status_byte),
            midi_data1: match info.midi_map {
                Some(MidiSource::Cc(n)) => n,
                _ => 0,
            },
            midi_channel: info.midi_channel.map_or(-1, i16::from),
            _reserved: 0,
            name: [0; 128],
            unit: [0; 32],
            group: [0; 128],
        };
        unsafe {
            let _ = copy_c_str(out.name.as_mut_ptr(), out.name.len(), &entry.name);
            let _ = copy_c_str(out.unit.as_mut_ptr(), out.unit.len(), info.unit.as_str());
            let _ = copy_c_str(out.group.as_mut_ptr(), out.group.len(), &entry.group);
        }
        true
    }

    pub(crate) fn write_clump_name(&self, clump_id: u32, out: *mut c_char, out_len: u32) -> u32 {
        if clump_id == 0 || out.is_null() || out_len == 0 {
            return 0;
        }
        let Some(name) = self.clump_names.get((clump_id - 1) as usize) else {
            return 0;
        };
        len_u32(unsafe { copy_c_str(out, out_len as usize, name) })
    }
}
