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
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

struct ExposedEntry {
    catalog_index: usize,
    name: String,
    group: String,
    clump_id: u32,
    hidden: bool,
    choice_labels: Vec<String>,
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
    presentation_revision: u64,
}

impl AuExposedParams {
    #[cfg(test)]
    pub(crate) fn capture(params: &dyn Params, automation: Automation) -> Self {
        Self::try_capture(params, automation)
            .expect("presentation revision did not stabilize during test capture")
    }

    pub(crate) fn try_capture(params: &dyn Params, automation: Automation) -> Option<Self> {
        const MAX_CAPTURE_ATTEMPTS: usize = 8;
        for _ in 0..MAX_CAPTURE_ATTEMPTS {
            let revision = params.parameter_presentation_revision();
            let built = Self::build(params.param_catalog(), params, automation, revision);
            if revision == params.parameter_presentation_revision() {
                return Some(built);
            }
        }
        None
    }

    pub(crate) fn render_catalog(&self) -> Arc<ParamCatalog> {
        Arc::clone(&self.catalog)
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
            let choice_labels = info
                .range
                .step_count()
                .map(std::num::NonZero::get)
                .filter(|&count| count <= 1024)
                .map_or_else(Vec::new, |count| {
                    let min = info.range.min();
                    let width = info.range.max() - min;
                    (0..=count)
                        .map(|step| {
                            params
                                .format_value(
                                    info.id,
                                    min + width * (f64::from(step) / f64::from(count)),
                                )
                                .unwrap_or_default()
                        })
                        .collect()
                });
            entries.push(ExposedEntry {
                catalog_index,
                name,
                group,
                clump_id,
                hidden,
                choice_labels,
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

    /// Consume changes and capture the replacement generation, when needed.
    pub(crate) fn sync(&self, params: &dyn Params) -> (bool, Option<Arc<Self>>, bool) {
        let changes = params.take_catalog_changes();
        let revised = params.parameter_presentation_revision() != self.presentation_revision;
        let refresh = changes
            .flags
            .intersects(CatalogChange::STRUCTURE | CatalogChange::INFO);
        let (fresh, retry) = if refresh || revised {
            match Self::try_capture(params, self.automation) {
                Some(snapshot) => (Some(Arc::new(snapshot)), false),
                None => (None, true),
            }
        } else {
            (None, false)
        };
        (changes.flags.contains(CatalogChange::VALUES), fresh, retry)
    }

    pub(crate) fn choice_label(&self, id: u32, value: f64) -> Option<&str> {
        let entry = &self.entries[*self.by_id.get(&id)?];
        let info = &self.catalog[entry.catalog_index];
        let count = info.range.step_count()?.get();
        let frac = (value - info.range.min()) / (info.range.max() - info.range.min());
        let index = (frac * f64::from(count))
            .round()
            .clamp(0.0, f64::from(count));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let index = index as usize;
        entry.choice_labels.get(index).map(String::as_str)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Write up to `cap` parameter IDs to `out_ids`; return the total count.
    ///
    /// # Safety
    /// `out_ids` is null or valid for writes of `cap` `u32`s.
    pub(crate) unsafe fn write_ids(&self, out_ids: *mut u32, cap: u32) -> u32 {
        if !out_ids.is_null() && cap > 0 {
            let n = self.entries.len().min(cap as usize);
            for (i, entry) in self.entries.iter().take(n).enumerate() {
                // SAFETY: `i < n <= cap`, and the caller guarantees `out_ids`
                // holds `cap` writable slots.
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
        // SAFETY: each destination is a fixed, non-empty array owned by `out`,
        // and its own length is passed as the capacity.
        unsafe {
            let _ = copy_c_str(out.name.as_mut_ptr(), out.name.len(), &entry.name);
            let _ = copy_c_str(out.unit.as_mut_ptr(), out.unit.len(), info.unit.as_str());
            let _ = copy_c_str(out.group.as_mut_ptr(), out.group.len(), &entry.group);
        }
        true
    }

    /// Copy clump `clump_id`'s name into `out` as a NUL-terminated string.
    ///
    /// # Safety
    /// `out` is null or valid for writes of `out_len` bytes.
    pub(crate) unsafe fn write_clump_name(
        &self,
        clump_id: u32,
        out: *mut c_char,
        out_len: u32,
    ) -> u32 {
        if clump_id == 0 || out.is_null() || out_len == 0 {
            return 0;
        }
        let Some(name) = self.clump_names.get((clump_id - 1) as usize) else {
            return 0;
        };
        // SAFETY: `out` is non-null with `out_len > 0` (checked above), and
        // the caller guarantees it holds `out_len` writable bytes.
        len_u32(unsafe { copy_c_str(out, out_len as usize, name) })
    }
}

struct ControlSnapshot {
    ctx: *const c_void,
    exposed: Option<Arc<AuExposedParams>>,
}

thread_local! {
    static CONTROL_SNAPSHOTS: RefCell<Vec<ControlSnapshot>> = const { RefCell::new(Vec::new()) };
}

/// Start a control-thread enumeration without changing the v14 callback table.
#[unsafe(no_mangle)]
extern "C" fn moose_au_rust_param_snapshot_begin(ctx: *const c_void) {
    CONTROL_SNAPSHOTS.with_borrow_mut(|snapshots| {
        snapshots.push(ControlSnapshot { ctx, exposed: None });
    });
}

#[unsafe(no_mangle)]
extern "C" fn moose_au_rust_param_snapshot_end() {
    CONTROL_SNAPSHOTS.with_borrow_mut(Vec::pop);
}

pub(crate) fn scoped_snapshot(
    ctx: *const c_void,
    exposed: &Mutex<Arc<AuExposedParams>>,
) -> Option<Arc<AuExposedParams>> {
    CONTROL_SNAPSHOTS.with_borrow_mut(|snapshots| {
        let snapshot = snapshots
            .last_mut()
            .filter(|snapshot| snapshot.ctx == ctx)?;
        Some(Arc::clone(
            snapshot
                .exposed
                .get_or_insert_with(|| current_snapshot(exposed)),
        ))
    })
}

pub(crate) fn current_snapshot(exposed: &Mutex<Arc<AuExposedParams>>) -> Arc<AuExposedParams> {
    Arc::clone(&exposed.lock().unwrap_or_else(PoisonError::into_inner))
}

pub(crate) fn control_snapshot(
    ctx: *const c_void,
    exposed: &Mutex<Arc<AuExposedParams>>,
) -> Arc<AuExposedParams> {
    scoped_snapshot(ctx, exposed).unwrap_or_else(|| current_snapshot(exposed))
}

/// Latest catalog from control and one retired render catalog. Only the
/// exclusive audio owner installs a catalog, and only control drains retire it.
#[derive(Default)]
pub(crate) struct RenderCatalog {
    pending: AtomicPtr<ParamCatalog>,
    retired: AtomicPtr<ParamCatalog>,
}

impl RenderCatalog {
    pub(crate) fn publish(&self, catalog: Arc<ParamCatalog>) {
        let previous = self
            .pending
            .swap(Arc::into_raw(catalog).cast_mut(), Ordering::AcqRel);
        if !previous.is_null() {
            // SAFETY: the swap transferred exclusive ownership of this Arc reference.
            drop(unsafe { Arc::from_raw(previous) });
        }
    }

    pub(crate) fn install(&self, current: &mut Arc<ParamCatalog>) -> bool {
        if self.pending.load(Ordering::Relaxed).is_null()
            || !self.retired.load(Ordering::Acquire).is_null()
        {
            return false;
        }
        let pending = self.pending.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if pending.is_null() {
            return false;
        }
        // SAFETY: the swap removed this Arc reference from pending. This call
        // holds the audio cell, so no other installer can write retired.
        let old = std::mem::replace(current, unsafe { Arc::from_raw(pending) });
        self.retired
            .store(Arc::into_raw(old).cast_mut(), Ordering::Release);
        true
    }

    pub(crate) fn reclaim(&self) {
        let retired = self.retired.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !retired.is_null() {
            // SAFETY: the swap transferred exclusive ownership of this Arc reference.
            drop(unsafe { Arc::from_raw(retired) });
        }
    }
}

impl Drop for RenderCatalog {
    fn drop(&mut self) {
        self.reclaim();
        let pending = *self.pending.get_mut();
        if !pending.is_null() {
            // SAFETY: exclusive access to the mailbox, owning this Arc reference.
            drop(unsafe { Arc::from_raw(pending) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moose_params::{DynamicParams, SlotDef, SlotRange};

    #[test]
    fn an_enumeration_pins_info_and_choice_labels_across_a_catalog_update() {
        let params = DynamicParams::<1>::new();
        let slot = params.slot(0).unwrap();
        let id = params.id(slot);
        let bind = |name, first, second| {
            params.edit(|edit| {
                edit.bind(
                    slot,
                    SlotDef::new(name, SlotRange::Choice(vec![first, second])),
                );
            });
        };
        bind("Original", "Dry".into(), "Wet".into());
        let exposed = Mutex::new(Arc::new(AuExposedParams::capture(
            &params,
            Automation::PerSample,
        )));
        let ctx = std::ptr::from_ref(&exposed).cast();
        moose_au_rust_param_snapshot_begin(ctx);
        let mut ids = [u32::MAX];
        // SAFETY: `ids` holds exactly the one slot requested.
        let written = unsafe { control_snapshot(ctx, &exposed).write_ids(ids.as_mut_ptr(), 1) };
        assert_eq!(written, 1);
        assert_eq!(ids, [id]);

        bind("Replacement", "Clean".into(), "Driven".into());
        let (_, fresh, retry) = current_snapshot(&exposed).sync(&params);
        assert!(!retry);
        *exposed.lock().unwrap() = fresh.unwrap();

        let pinned = control_snapshot(ctx, &exposed);
        let mut info = AuParamInfoOut::default();
        assert!(pinned.fill_info(id, &mut info));
        assert_eq!(info.name[0], c_char::try_from(b'O').unwrap());
        assert_eq!(pinned.choice_label(id, 1.0), Some("Wet"));
        moose_au_rust_param_snapshot_end();

        let latest = control_snapshot(ctx, &exposed);
        assert!(latest.fill_info(id, &mut info));
        assert_eq!(info.name[0], c_char::try_from(b'R').unwrap());
        assert_eq!(latest.choice_label(id, 1.0), Some("Driven"));
    }
}
