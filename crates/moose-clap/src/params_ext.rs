//! Extension: `clap.params`.

use crate::catalog_sync;
use crate::copy_str_to_buf;
use crate::data_from_plugin;
use crate::events::convert_input_events;
use crate::events::defer_param_change;
use crate::process::flush_gui_changes;
use crate::state_ext::request_changed_presentation;

use clap_sys::events::{clap_input_events, clap_output_events};
use clap_sys::ext::params::{
    CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_BYPASS, CLAP_PARAM_IS_ENUM, CLAP_PARAM_IS_HIDDEN,
    CLAP_PARAM_IS_MODULATABLE, CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID, CLAP_PARAM_IS_READONLY,
    CLAP_PARAM_IS_STEPPED, clap_param_info, clap_plugin_params,
};
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;
use clap_sys::string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE};
use moose_core::cast::len_u32;
use moose_core::events::EventBody;
use moose_core::export::PluginExport;
use moose_core::wrapper::{copy_c_str, run_extern_callback_with};
use moose_params::Params;
use moose_params::{ParamCatalog, ParamFlags, ParamRange};
use std::ffi::{CStr, c_char};
use std::ptr;

// ---------------------------------------------------------------------------
// Extension: params
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn params_count<P: PluginExport>(plugin: *const clap_plugin) -> u32 {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        len_u32(catalog_sync::lock_exposed(&data.exposed).len())
    }
}

pub(crate) unsafe extern "C" fn params_get_info<P: PluginExport>(
    plugin: *const clap_plugin,
    param_index: u32,
    out: *mut clap_param_info,
) -> bool {
    // Author `parameter_presentation` can panic; firewall it so a panic
    // cannot unwind across this `extern "C"` boundary.
    run_extern_callback_with::<P, bool>("CLAP", "params_get_info", false, || unsafe {
        if out.is_null() {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        let exposed = catalog_sync::lock_exposed(&data.exposed);
        let Some((catalog_index, info)) = exposed.info_with_index(param_index) else {
            return false;
        };
        let out = &mut *out;

        out.id = info.id;
        // Strict-provenance non-null cookie encoding the dense catalog slot
        // (`catalog_index + 1`). Validated with `ParamCatalog::validate_cookie`
        // so stale cookies across a dynamic rescan degrade safely to ID lookup.
        out.cookie = ptr::without_provenance_mut(ParamCatalog::cookie_for_index(catalog_index));

        // Convert flags
        let mut flags: u32 = 0;
        if info.flags.contains(ParamFlags::AUTOMATABLE) {
            flags |= CLAP_PARAM_IS_AUTOMATABLE;
        }
        if info.flags.contains(ParamFlags::HIDDEN) {
            flags |= CLAP_PARAM_IS_HIDDEN;
        }
        if info.flags.contains(ParamFlags::READONLY) {
            flags |= CLAP_PARAM_IS_READONLY;
        }
        if info.flags.contains(ParamFlags::IS_BYPASS) {
            flags |= CLAP_PARAM_IS_BYPASS;
        }
        if info.flags.contains(ParamFlags::MODULATABLE) {
            flags |= CLAP_PARAM_IS_MODULATABLE;
        }
        if info.flags.contains(ParamFlags::MODULATABLE_PER_NOTE) {
            flags |= CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID;
        }
        flags |= range_flags(&info.range);
        let presentation = data.params_arc.parameter_presentation(info.id);
        out.flags = presented_flags(flags, presentation.as_ref());

        out.min_value = info.range.min();
        out.max_value = info.range.max();
        out.default_value = info.default_plain;

        // Name
        out.name = [0; CLAP_NAME_SIZE];
        copy_str_to_buf(
            &mut out.name,
            presentation.as_ref().map_or(info.name, |p| p.name.as_str()),
        );

        // Module path (use group if non-empty)
        out.module = [0; CLAP_PATH_SIZE];
        copy_str_to_buf(
            &mut out.module,
            presentation
                .as_ref()
                .map_or(info.group, |p| p.group.as_str()),
        );

        true
    })
}

/// Flags determined solely by the plain value range.
pub(crate) fn range_flags(range: &ParamRange) -> u32 {
    match range.base() {
        ParamRange::Enum { .. } => CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_ENUM,
        ParamRange::Discrete { .. } => CLAP_PARAM_IS_STEPPED,
        ParamRange::Stepped { min, max, step }
            if [min, max, step]
                .iter()
                .all(|v| v.is_finite() && v.fract() == 0.0) =>
        {
            CLAP_PARAM_IS_STEPPED
        }
        _ => 0,
    }
}

/// A runtime presentation's `hidden` overrides the static flag.
pub(crate) fn presented_flags(
    flags: u32,
    presentation: Option<&moose_params::ParameterPresentation>,
) -> u32 {
    match presentation {
        Some(p) if p.hidden => flags | CLAP_PARAM_IS_HIDDEN,
        Some(_) => flags & !CLAP_PARAM_IS_HIDDEN,
        None => flags,
    }
}

pub(crate) unsafe extern "C" fn params_get_value<P: PluginExport>(
    plugin: *const clap_plugin,
    param_id: clap_id,
    out_value: *mut f64,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let idx = catalog_sync::lock_exposed(&data.exposed).index_of(param_id);
        let value = match idx {
            Some(index) => data.params_arc.get_plain_by_index(index, param_id),
            None => data.params_arc.get_plain(param_id),
        };
        match value {
            Some(v) => {
                *out_value = v;
                true
            }
            None => false,
        }
    }
}

pub(crate) unsafe extern "C" fn params_value_to_text<P: PluginExport>(
    plugin: *const clap_plugin,
    param_id: clap_id,
    value: f64,
    out_buffer: *mut c_char,
    out_buffer_capacity: u32,
) -> bool {
    // Author `format_value` can panic; firewall it (false = no display text).
    run_extern_callback_with::<P, bool>("CLAP", "value_to_text", false, || unsafe {
        // A zero capacity makes `cap - 1` underflow and a null `out_buffer`
        // plus non-zero capacity would still write the trailing NUL. Treat
        // both as "host wants nothing" and return.
        if out_buffer_capacity == 0 || out_buffer.is_null() {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        let idx = catalog_sync::lock_exposed(&data.exposed).index_of(param_id);
        let formatted = match idx {
            Some(index) => data
                .params_arc
                .format_value_by_index(index, param_id, value),
            None => data.params_arc.format_value(param_id, value),
        };
        match formatted {
            Some(text) => {
                let _ = copy_c_str(out_buffer, out_buffer_capacity as usize, &text);
                true
            }
            None => false,
        }
    })
}

pub(crate) unsafe extern "C" fn params_text_to_value<P: PluginExport>(
    plugin: *const clap_plugin,
    param_id: clap_id,
    param_value_text: *const c_char,
    out_value: *mut f64,
) -> bool {
    // Author `parse_value` can panic; firewall it (false = "not parsed").
    run_extern_callback_with::<P, bool>("CLAP", "text_to_value", false, || unsafe {
        if param_value_text.is_null() {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        let Ok(text) = CStr::from_ptr(param_value_text).to_str() else {
            return false;
        };
        let idx = catalog_sync::lock_exposed(&data.exposed).index_of(param_id);
        let parsed = match idx {
            Some(index) => data.params_arc.parse_value_by_index(index, param_id, text),
            None => data.params_arc.parse_value(param_id, text),
        };
        match parsed {
            Some(v) => {
                *out_value = v;
                true
            }
            None => false,
        }
    })
}

pub(crate) unsafe extern "C" fn params_flush<P: PluginExport>(
    plugin: *const clap_plugin,
    in_events: *const clap_input_events,
    out_events: *const clap_output_events,
) {
    // Event conversion + `set_plain` run here; firewall the whole sweep so a
    // panic (a hostile-host event, a param edge) can't unwind across the C ABI
    // - matching the other `clap_plugin_params` callbacks.
    run_extern_callback_with::<P, ()>("CLAP", "params_flush", (), || unsafe {
        let data = data_from_plugin::<P>(plugin);
        // `flush` is never concurrent with `process` (CLAP contract), so it
        // owns the audio scratch through the cell for its event conversion.
        let mut audio = data.audio.enter();
        let scr = &mut *audio;
        // params_flush only forwards param values to the plugin and
        // sweeps GUI-driven changes outward; it doesn't iterate the
        // event list in time order, so skip the sort.
        // params_flush is a non-audio-thread param sweep; no state-load
        // race possible here, so the drain flag stays false.
        convert_input_events::<P>(scr, &data.info, in_events, false, false, None);
        // `flush` doesn't enter `process()`, so the chunker's deferred
        // `set_plain` apply pass never runs. Apply `ParamChange` events
        // synchronously here so host-thread param sweeps (preset
        // recalls, host automation while transport is stopped, the
        // clap-validator `state-reproducibility-flush` check) take
        // effect. Mirrors what the audio-thread chunker does inside
        // `apply_pending_events`.
        let params = data.params_arc.as_ref() as &dyn Params;
        for ev in scr.event_list.iter() {
            match ev.body {
                EventBody::ParamChange { id, value } => {
                    match scr.catalog.index_of(id) {
                        Some(index) => params.set_plain_by_index(index, id, value),
                        None => params.set_plain(id, value),
                    }
                    defer_param_change(&mut scr.deferred_params, id, value);
                }
                // Conversion accepts wildcard note IDs only for fully global
                // modulation. Flush has no sample timeline: update the shared
                // offset now so cancellation survives the next conversion.
                EventBody::ParamMod {
                    id,
                    note_id: -1,
                    value,
                } => {
                    params.set_modulation(id, value);
                }
                _ => {}
            }
        }
        request_changed_presentation(data, &scr.event_list);
        let _ = flush_gui_changes::<P>(data, out_events);
    });
}

pub(crate) fn make_params_extension<P: PluginExport>() -> clap_plugin_params {
    clap_plugin_params {
        count: Some(params_count::<P>),
        get_info: Some(params_get_info::<P>),
        get_value: Some(params_get_value::<P>),
        value_to_text: Some(params_value_to_text::<P>),
        text_to_value: Some(params_text_to_value::<P>),
        flush: Some(params_flush::<P>),
    }
}

#[cfg(test)]
mod presentation_tests {
    use super::*;
    use moose_params::ParameterPresentation;

    #[test]
    fn presentation_hidden_overrides_static_flag() {
        let shown = ParameterPresentation {
            name: "A".into(),
            group: String::new(),
            hidden: false,
            available: true,
        };
        let hidden = ParameterPresentation {
            hidden: true,
            ..shown.clone()
        };
        let base = CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_HIDDEN;
        assert_eq!(presented_flags(base, None), base);
        assert_eq!(
            presented_flags(base, Some(&shown)),
            CLAP_PARAM_IS_AUTOMATABLE
        );
        assert_eq!(
            presented_flags(CLAP_PARAM_IS_AUTOMATABLE, Some(&hidden)),
            base
        );
    }

    #[test]
    fn cookie_round_trips_strict_provenance_and_rejects_stale_id() {
        use moose_params::{ParamInfo, ParamUnit, ParamValueKind};
        let info_a = ParamInfo {
            id: 101,
            name: "Gain",
            short_name: "Gain",
            group: "",
            range: ParamRange::Linear { min: 0.0, max: 1.0 },
            default_plain: 0.5,
            unit: ParamUnit::None,
            flags: ParamFlags::AUTOMATABLE,
            kind: ParamValueKind::Float,
            midi_map: None,
            midi_channel: None,
        };
        let info_b = ParamInfo {
            id: 42,
            name: "Mix",
            short_name: "Mix",
            group: "",
            ..info_a
        };
        let catalog = ParamCatalog::new(vec![info_a, info_b]);
        let cookie_b: *mut std::ffi::c_void =
            ptr::without_provenance_mut(ParamCatalog::cookie_for_index(1));
        assert!(!cookie_b.is_null());
        let (idx, info) = catalog
            .validate_cookie(cookie_b.addr(), 42)
            .expect("valid cookie");
        assert_eq!(idx, 1);
        assert_eq!(info.id, 42);
        // Stale cookie pointing at slot 1 when host passes id 101 is rejected,
        // and `resolve_cookie` falls back cleanly to slot 0.
        assert!(catalog.validate_cookie(cookie_b.addr(), 101).is_none());
        let (fallback_idx, fallback_info) = catalog
            .resolve_cookie(cookie_b.addr(), 101)
            .expect("fallback by id");
        assert_eq!(fallback_idx, 0);
        assert_eq!(fallback_info.id, 101);
        // Null cookie also falls back cleanly.
        let (null_idx, _) = catalog
            .resolve_cookie(ptr::null_mut::<std::ffi::c_void>().addr(), 42)
            .expect("null cookie fallback");
        assert_eq!(null_idx, 1);
    }
}
