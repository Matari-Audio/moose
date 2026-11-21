//! Parameter callbacks: count, values, presentation, format and parse.

use crate::ffi;
use crate::instance::Vst3Instance;
use crate::midi_proxy::allocated_midi_proxy_index;

use moose_core::cast::len_u32;
use moose_core::export::PluginExport;
use moose_core::wrapper::{copy_c_str, run_extern_callback_with};
use moose_params::{ParamFlags, Params};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::atomic::Ordering;

pub(crate) unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        // Read the cached `param_ranges.len()` rather than walking the
        // `Params` impl. The cache is built once at instantiation
        // (`Vst3Instance::new`) and never grows; trait dispatch was
        // free per-call but consistent with the cache-first pattern
        // the rest of the file uses.
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        len_u32(inst.param_ranges.len() + inst.midi_proxy_values.len())
    }
}

pub(crate) unsafe extern "C" fn cb_param_get_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(index) = allocated_midi_proxy_index(&inst.midi_proxy_ids, id)
            && let Some(slot) = inst.midi_proxy_values.get(index)
        {
            return f64::from_bits(slot.load(Ordering::Relaxed));
        }
        inst.params_arc.get_plain(id).unwrap_or(0.0)
    }
}

pub(crate) unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(index) = allocated_midi_proxy_index(&inst.midi_proxy_ids, id)
            && let Some(slot) = inst.midi_proxy_values.get(index)
        {
            slot.store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
            return;
        }
        inst.params_arc.set_plain(id, value);
    }
}

pub(crate) unsafe extern "C" fn cb_param_presentation<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    out: *mut c_char,
    capacity: u32,
) -> i32 {
    run_extern_callback_with::<P, i32>("vst3", "parameter_presentation", -1, || unsafe {
        if ctx.is_null() || out.is_null() || capacity == 0 {
            return -1;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let Some(presentation) = inst.params_arc.parameter_presentation(id) else {
            return -1;
        };
        let _ = copy_c_str(out, capacity as usize, &presentation.name);
        let mut bits = 0;
        if presentation.hidden {
            bits |= ffi::PRESENTATION_HIDDEN;
        }
        if !presentation.available {
            bits |= ffi::PRESENTATION_UNAVAILABLE;
        }
        bits
    })
}

pub(crate) unsafe extern "C" fn cb_param_presentation_revision<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u64 {
    run_extern_callback_with::<P, u64>("vst3", "parameter_presentation_revision", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        (*ctx.cast::<Vst3Instance<P>>())
            .params_arc
            .parameter_presentation_revision()
    })
}

/// Whether `id` is a `CHUNKED` param. The shim keys its block-rate
/// pre-commit on this: chunked params are committed per-offset by
/// `process_chunked`, so the shim must not pre-write their end value.
pub(crate) unsafe extern "C" fn cb_param_is_chunked<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> i32 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let chunked = inst
            .param_infos
            .by_id(id)
            .is_some_and(|info| info.flags.contains(ParamFlags::CHUNKED));
        i32::from(chunked)
    }
}

pub(crate) unsafe extern "C" fn cb_param_normalize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    plain: f64,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(range) = inst.params_arc.live_param_range(id) {
            return range.normalize(plain);
        }
        match inst.param_ranges.binary_search_by_key(&id, |(i, _)| *i) {
            Ok(idx) => inst.param_ranges[idx].1.normalize(plain),
            Err(_) => plain,
        }
    }
}

pub(crate) unsafe extern "C" fn cb_param_denormalize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    normalized: f64,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(range) = inst.params_arc.live_param_range(id) {
            return range.denormalize(normalized);
        }
        match inst.param_ranges.binary_search_by_key(&id, |(i, _)| *i) {
            Ok(idx) => inst.param_ranges[idx].1.denormalize(normalized),
            Err(_) => normalized,
        }
    }
}

/// Live VST3 `stepCount` for parameters whose range changes at runtime (such
/// as [`moose_params::ValueListParam`] after `set_labels` or `DynamicParams`),
/// or `-1` to keep the static registration descriptor's `step_count`.
///
/// When returning `>= 0` and `out_default_norm` is non-null, writes the
/// parameter's normalized default under the live range so `getParameterInfo`
/// keeps `defaultNormalizedValue` consistent with the live `stepCount`.
#[allow(clippy::cast_possible_wrap)]
pub(crate) unsafe extern "C" fn cb_param_step_count<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    out_default_norm: *mut f64,
) -> i32 {
    unsafe {
        let Some(inst) = ctx.cast::<Vst3Instance<P>>().as_ref() else {
            return -1;
        };
        let Some(range) = inst.params_arc.live_param_range(id) else {
            return -1;
        };
        if let Some(out_default) = out_default_norm.as_mut()
            && let Some(info) = inst.param_infos.by_id(id)
        {
            *out_default = range.normalize(info.default_plain);
        }
        range.step_count().map_or(0, |n| n.get() as i32)
    }
}

pub(crate) unsafe extern "C" fn cb_param_format<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    // The author's `format_value` can panic (an `unwrap` on a host value
    // outside the declared domain); firewall it so that can't abort the
    // host. On panic the host sees an empty display string.
    run_extern_callback_with::<P, u32>("vst3", "format_value", 0, || unsafe {
        // `out_len == 0` would underflow on `out_len as usize - 1`
        // and let `copy_nonoverlapping` write the full formatted
        // string into a buffer the host claimed had zero capacity.
        // Treat zero capacity as "host wants nothing" and return.
        if out_len == 0 || out.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        match inst.params_arc.format_value(id, value) {
            Some(text) => len_u32(copy_c_str(out, out_len as usize, &text)),
            None => 0,
        }
    })
}

/// Parse host text entry for a MIDI-CC proxy param: a plain number,
/// clamped to the proxy's normalized `0..=1` domain.
pub(crate) fn parse_midi_proxy_text(text: &str) -> Option<f64> {
    let v = text.trim().parse::<f64>().ok()?;
    v.is_finite().then(|| v.clamp(0.0, 1.0))
}

pub(crate) unsafe extern "C" fn cb_param_parse<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    text: *const c_char,
    out_plain: *mut f64,
) -> i32 {
    // Author `parse_value` can panic; firewall it (0 = "not parsed").
    run_extern_callback_with::<P, i32>("vst3", "parse_value", 0, || unsafe {
        if text.is_null() || out_plain.is_null() {
            return 0;
        }
        let Ok(text) = CStr::from_ptr(text).to_str() else {
            return 0;
        };
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        // MIDI-CC proxy params have no `Params` entry; their display
        // text is the shim's `%.2f` fallback of the normalized value.
        if allocated_midi_proxy_index(&inst.midi_proxy_ids, id).is_some() {
            return match parse_midi_proxy_text(text) {
                Some(v) => {
                    *out_plain = v;
                    1
                }
                None => 0,
            };
        }
        match inst.params_arc.parse_value(id, text) {
            Some(v) => {
                *out_plain = v;
                1
            }
            None => 0,
        }
    })
}
