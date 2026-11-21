//! Plugin registration: descriptors and the callback table handed to the COM layer.

use crate::ffi;
use crate::gui::{
    cb_gui_can_resize, cb_gui_check_size_constraint, cb_gui_close, cb_gui_event_fd,
    cb_gui_get_size, cb_gui_has_editor, cb_gui_on_fd, cb_gui_on_timer, cb_gui_open,
    cb_gui_open_wayland, cb_gui_param_at, cb_gui_set_content_scale, cb_gui_set_size,
    cb_gui_supports_wayland, cb_gui_wayland_set_sync, cb_midi_mapping_get_param_id,
};
use crate::host_state::{
    cb_state_free, cb_state_load, cb_state_load_stream, cb_state_save, cb_state_save_stream,
};
use crate::input_events::{cb_begin_input_events, cb_push_input_event, cb_push_param_change};
use crate::lifecycle::{
    cb_bind_host_queue, cb_create, cb_destroy, cb_drain_host_queue, cb_keyswitches,
    cb_layout_bus_channels, cb_match_bus_layout, cb_match_bus_layout_perbus, cb_midi_learn,
    cb_param_affects_presentation, cb_param_function_name, cb_perform_menu, cb_prepare,
    cb_program_list, cb_remap_param_id, cb_reset, cb_set_active, cb_set_audio_presentation_latency,
    cb_set_catalog_waker, cb_set_persist_waker, cb_set_processing, cb_set_program_data,
    cb_take_catalog_changes, cb_take_persist_change, cb_track_info_changed,
    cb_wants_persist_notifications, descriptor_buses, vst3_topology_consistent,
};
use crate::midi_proxy::{
    MIDI_PROXY_PER_CHANNEL, allocate_midi_proxy_ids, allocated_midi_proxy_id, midi_proxy_default,
    midi_proxy_len,
};
use crate::output_events::{
    cb_begin_output_events, cb_commit_output_event, cb_finish_output_events, cb_get_output_param,
    cb_get_output_param_count, cb_next_output_event,
};
use crate::params::{
    cb_param_count, cb_param_denormalize, cb_param_format, cb_param_get_value, cb_param_is_chunked,
    cb_param_normalize, cb_param_parse, cb_param_presentation, cb_param_presentation_revision,
    cb_param_set_value, cb_param_step_count,
};
use crate::process::{cb_get_latency, cb_get_tail, cb_process, cb_process_f64};

use crate::ffi::{Vst3Callbacks, Vst3ParamDescriptor, Vst3PluginDescriptor};
use crate::midi_proxy::{MIDI_PROXY_PITCH_BEND, MIDI_PROXY_PRESSURE};
use moose_core::bus::{BusConfig, BusLayout};
use moose_core::bus_routing::bus_layouts_fit_routing;
use moose_core::cast::len_u32;
use moose_core::export::PluginExport;
use moose_core::info::{PluginCategory, PluginInfo, resolve_name_override};
use moose_core::plugin::PluginRuntime;
use moose_core::state;
use moose_core::wrapper::{
    ParamCStrings, default_io_channels, log_missing_bus_layout, run_register,
};
use moose_params::sample::Float;
use moose_params::{ParamFlags, ParamRange};
use std::ffi::CString;

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Plugin display-name surfaced as `PClassInfo::name`. Reads
/// `moose.toml`'s `vst3_name` (baked into `PluginInfo` by
/// `moose::plugin_info!`), falling back to `PluginInfo::name`.
pub(crate) fn resolved_plugin_name(info: &PluginInfo) -> &'static str {
    resolve_name_override(info.vst3_name, info.name)
}

/// Largest structural input and output widths across all declared layouts.
///
/// Disabled optional buses still occupy fixed positions in the process
/// pointer arrays, so their declared channels count toward the width the
/// COM layer sizes them for.
pub(crate) fn max_layout_channels(layouts: &[BusLayout]) -> (u32, u32) {
    layouts.iter().fold((0, 0), |(mi, mo), l| {
        let sum = |buses: &[BusConfig]| {
            buses.iter().fold(0_u32, |channels, bus| {
                channels.saturating_add(bus.channels.channel_count())
            })
        };
        (mi.max(sum(&l.inputs)), mo.max(sum(&l.outputs)))
    })
}

pub(crate) fn vst3_bus_widths_supported(layouts: &[BusLayout]) -> bool {
    layouts.iter().all(|layout| {
        layout
            .inputs
            .iter()
            .chain(&layout.outputs)
            .all(|bus| bus.channels.channel_count() <= 64)
    })
}

pub fn register_vst3<P: PluginExport>() {
    // Called from the export macro's `extern "C" fn init()` static
    // initializer. Catch any panic so it doesn't cross the FFI
    // boundary and abort the host process.
    run_register::<P>("VST3", || {
        let Some((num_inputs, num_outputs)) = default_io_channels::<P>() else {
            log_missing_bus_layout::<P>("VST3");
            return;
        };
        let layouts = P::bus_layouts();
        if !vst3_bus_widths_supported(&layouts) {
            eprintln!(
                "[moose VST3] {} declares a bus wider than 64 channels - plugin will not register.",
                std::any::type_name::<P>()
            );
            return;
        }
        if !bus_layouts_fit_routing(&layouts) {
            eprintln!(
                "[moose VST3] {} declares an audio-bus topology beyond BusRouting's limit of 32 \
                 buses per direction and 65,535 channels per bus - plugin will not register.",
                std::any::type_name::<P>(),
            );
            return;
        }
        if !vst3_topology_consistent(&layouts) {
            eprintln!(
                "[moose VST3] {} declares bus layouts that differ in input/output bus count or \
                 kind. VST3 fixes one bus topology per plugin - only channel widths may vary \
                 across layouts, not the number of buses or their main/sidechain kind. Give every \
                 layout the same bus structure. Plugin will not register.",
                std::any::type_name::<P>(),
            );
            return;
        }
        register_vst3_inner::<P>(num_inputs, num_outputs);
    });
}

// VST3 `ParameterInfo::ParameterFlags`. Only these bits are defined by
// the SDK (3.7); the previous code set a reserved `1 << 8`, which no host
// interprets, and left the read-only / hidden / list bits unmapped.
pub(crate) const VST3_PARAM_CAN_AUTOMATE: i32 = 1 << 0;
pub(crate) const VST3_PARAM_IS_READ_ONLY: i32 = 1 << 1;
pub(crate) const VST3_PARAM_IS_LIST: i32 = 1 << 3;
/// `kIsHidden` (SDK 3.7+): the parameter is not shown in generic editors
/// or automation pickers. Hosts still write to hidden ids through
/// `IParameterChanges`, which is how the MIDI proxy bank receives its
/// `IMidiMapping`-resolved controllers.
pub(crate) const VST3_PARAM_IS_HIDDEN: i32 = 1 << 4;
pub(crate) const VST3_PARAM_IS_BYPASS: i32 = 1 << 16;

// Assembles the descriptor, param descriptors, and callback table in one
// linear pass; splitting it further would scatter the one-time registration
// wiring across helpers that each read once.
#[allow(clippy::too_many_lines)]
pub(crate) fn register_vst3_inner<P: PluginExport>(num_inputs: u32, num_outputs: u32) {
    let info = P::info();
    // Static metadata path: derive emits a `LazyLock`-cached
    // `Vec<ParamInfo>` so registration skips the
    // `Self::create().params().param_infos()` walk and the plugin
    // construction it implies. Hand-written `PluginExport` impls
    // without a `Params::param_infos_static` override fall back to
    // the historical runtime path inside `PluginExport`'s default
    // impl.
    let param_infos = P::param_infos_static();
    let midi_proxy_ids = allocate_midi_proxy_ids(&param_infos, midi_proxy_len::<P>());

    let mut param_descs: Vec<Vst3ParamDescriptor> = Vec::with_capacity(param_infos.len());
    for pi in &param_infos {
        let cs = ParamCStrings::from_info(pi);

        let mut flags: i32 = 0;
        if pi.flags.contains(ParamFlags::AUTOMATABLE) {
            flags |= VST3_PARAM_CAN_AUTOMATE;
        }
        if pi.flags.contains(ParamFlags::READONLY) {
            flags |= VST3_PARAM_IS_READ_ONLY;
        }
        if pi.flags.contains(ParamFlags::HIDDEN) {
            flags |= VST3_PARAM_IS_HIDDEN;
        }
        if pi.flags.contains(ParamFlags::IS_BYPASS) {
            flags |= VST3_PARAM_IS_BYPASS;
        }
        // An enum is a named, indexed value list: `kIsList` makes the host
        // render a value dropdown (populated via `getParamStringByValue`)
        // instead of a knob. Discreteness for int / discrete-float params
        // is carried by the `step_count` descriptor field below, which is
        // what VST3 reads for step navigation - there's no separate flag.
        if matches!(pi.range, ParamRange::Enum { .. }) {
            flags |= VST3_PARAM_IS_LIST;
        }
        let step_count = pi.range.step_count();

        param_descs.push(Vst3ParamDescriptor {
            id: pi.id,
            name: cs.name.into_raw(),
            short_name: cs.short_name.into_raw(),
            units: cs.unit.into_raw(),
            min: pi.range.min(),
            max: pi.range.max(),
            default_normalized: pi.range.normalize(pi.default_plain),
            // Param step counts come from `IntParam`/`EnumParam` ranges,
            // bounded well below i32::MAX in practice.
            #[allow(clippy::cast_possible_wrap)]
            step_count: step_count.map_or(0, |n| n.get() as i32),
            flags,
            group: cs.group.into_raw(),
        });
    }

    // Hidden MIDI input proxies (see the MIDI-proxy block above):
    // appended *after* the real params so the shim's index-based
    // structures (unit table, ParameterInfo enumeration) keep their
    // positions. One bank per declared MIDI input port so multi-port
    // plugins keep controllers attributed per bus. `kIsHidden` (with
    // `kCanAutomate` clear) keeps the bank out of generic editors and
    // automation pickers - flags 0 alone left thousands of "MIDI Ch N
    // CC M" rows listed; hosts still deliver `IMidiMapping`-resolved
    // changes to hidden ids. Identity 0..=1 range, grouped under a
    // "MIDI" unit. The CStrings intentionally leak - registration
    // runs once per process, matching the real params' `into_raw`
    // pattern.
    if info.accepts_midi_in {
        let empty_units = || CString::default().into_raw();
        for port in 0..info.midi_input_ports {
            // Single-port plugins keep the unprefixed names hosts
            // already display; only multi-port names carry the bus.
            let (name_prefix, short_prefix) = if info.midi_input_ports > 1 {
                (format!("MIDI In {} ", port + 1), format!("I{}", port + 1))
            } else {
                (String::from("MIDI "), String::new())
            };
            for channel in 0u8..16 {
                for controller in 0..MIDI_PROXY_PER_CHANNEL {
                    let (name, short) = match controller {
                        MIDI_PROXY_PITCH_BEND => (
                            format!("{name_prefix}Ch {} Pitch Bend", channel + 1),
                            format!("{short_prefix}M{}PB", channel + 1),
                        ),
                        MIDI_PROXY_PRESSURE => (
                            format!("{name_prefix}Ch {} Pressure", channel + 1),
                            format!("{short_prefix}M{}Pr", channel + 1),
                        ),
                        cc => (
                            format!("{name_prefix}Ch {} CC {cc}", channel + 1),
                            format!("{short_prefix}M{}C{cc}", channel + 1),
                        ),
                    };
                    param_descs.push(Vst3ParamDescriptor {
                        id: allocated_midi_proxy_id(&midi_proxy_ids, port, channel, controller)
                            .expect("VST3 MIDI proxy allocation is complete"),
                        name: CString::new(name).unwrap_or_default().into_raw(),
                        short_name: CString::new(short).unwrap_or_default().into_raw(),
                        units: empty_units(),
                        min: 0.0,
                        max: 1.0,
                        default_normalized: midi_proxy_default(controller),
                        step_count: 0,
                        flags: VST3_PARAM_IS_HIDDEN,
                        group: CString::new("MIDI").unwrap_or_default().into_raw(),
                    });
                }
            }
        }
    }

    let name = CString::new(resolved_plugin_name(&info)).unwrap_or_default();
    let vendor = CString::new(info.vendor).unwrap_or_default();
    let url = CString::new(info.url).unwrap_or_default();
    let version = CString::new(info.version).unwrap_or_default();
    let category = CString::new("Audio Module Class").unwrap_or_default();
    // VST3 "Plugin Type Categories": Cubase (and other VST3 hosts)
    // route plugins into submenus based on a `<primary>|<secondary>`
    // pair from the SDK's published vocabulary. `Fx` alone advertises
    // the plug-in as "an effect of unspecified kind" and falls back
    // to the "Other" bucket; a secondary token like `Delay`, `Reverb`,
    // `EQ`, `Modulation`, etc. routes to the matching submenu.
    //
    // The Analyzer / NoteEffect / Tool categories already carry their
    // own implicit secondary token (`Fx|Analyzer`, `Fx|Event`,
    // `Fx|Tools`). For instruments and generic effects, the secondary
    // is opt-in via `moose.toml`'s `vst3_subcategory`. When unset the
    // wrapper ships the bare primary so the plug-in still loads, just
    // unbucketed.
    let subcategory_str = match (info.category, info.vst3_subcategory) {
        (PluginCategory::Instrument, Some(sub)) => format!("Instrument|{sub}"),
        (PluginCategory::Instrument, None) => "Instrument|Synth".to_string(),
        (PluginCategory::Effect, Some(sub)) => format!("Fx|{sub}"),
        (PluginCategory::Effect, None) => "Fx".to_string(),
        (PluginCategory::NoteEffect, _) => "Fx|Event".to_string(),
        (PluginCategory::Analyzer, _) => "Fx|Analyzer".to_string(),
        (PluginCategory::Tool, _) => "Fx|Tools".to_string(),
    };
    let subcategories = CString::new(subcategory_str).unwrap_or_default();

    // MIDI port counts are decided once on `PluginInfo` (category
    // default, overridable via `midi_input` / `midi_output` /
    // `midi_input_ports` / `midi_output_ports` in moose.toml). The shim
    // advertises this many event buses per direction.
    let midi_output_ports = i32::from(info.midi_output_ports);
    let midi_input_ports = i32::from(info.midi_input_ports);

    // Per-bus structure from the first declared layout (bus count + kind
    // are consistent across a plugin's layouts; only widths vary).
    let (num_input_buses, num_output_buses, input_bus_kinds, output_bus_kinds) =
        descriptor_buses::<P>();

    let compatibility_json = P::vst3_compatibility_json()
        .filter(|json| !json.is_empty())
        .and_then(|json| CString::new(json).ok())
        .map_or(std::ptr::null(), |s| s.into_raw().cast_const());

    let descriptor = Box::leak(Box::new(Vst3PluginDescriptor {
        name: name.into_raw(),
        vendor: vendor.into_raw(),
        url: url.into_raw(),
        email: std::ptr::null(),
        version: version.into_raw(),
        cid: state::resolve_vst3_cid(P::vst3_class_id(), info.vst3_id),
        category: category.into_raw(),
        subcategories: subcategories.into_raw(),
        num_inputs,
        num_outputs,
        num_input_buses,
        num_output_buses,
        input_bus_kinds,
        output_bus_kinds,
        midi_output_ports,
        midi_input_ports,
        supports_f64: i32::from(<P as PluginRuntime>::Sample::IS_F64),
        // Widest output width across all layouts, so the shim's per-channel
        // output discard scratch has a distinct block for every channel of
        // any negotiable layout (see the field's `ffi` doc).
        max_inputs: max_layout_channels(&P::bus_layouts()).0,
        max_outputs: max_layout_channels(&P::bus_layouts()).1,
        compatibility_json,
    }));

    let callbacks = Box::leak(Box::new(Vst3Callbacks {
        create: cb_create::<P>,
        destroy: cb_destroy::<P>,
        reset: cb_reset::<P>,
        prepare: cb_prepare::<P>,
        process: cb_process::<P>,
        process_f64: cb_process_f64::<P>,
        param_count: cb_param_count::<P>,
        param_get_value: cb_param_get_value::<P>,
        param_set_value: cb_param_set_value::<P>,
        param_normalize: cb_param_normalize::<P>,
        param_denormalize: cb_param_denormalize::<P>,
        param_format: cb_param_format::<P>,
        param_parse: cb_param_parse::<P>,
        state_save: cb_state_save::<P>,
        state_load: cb_state_load::<P>,
        state_load_stream: cb_state_load_stream::<P>,
        state_save_stream: cb_state_save_stream::<P>,
        state_free: cb_state_free,
        get_latency: cb_get_latency::<P>,
        get_tail: cb_get_tail::<P>,
        begin_input_events: cb_begin_input_events::<P>,
        push_input_event: cb_push_input_event::<P>,
        push_param_change: cb_push_param_change::<P>,
        begin_output_events: cb_begin_output_events::<P>,
        next_output_event: cb_next_output_event::<P>,
        commit_output_event: cb_commit_output_event::<P>,
        finish_output_events: cb_finish_output_events::<P>,
        gui_has_editor: cb_gui_has_editor::<P>,
        gui_get_size: cb_gui_get_size::<P>,
        gui_open: cb_gui_open::<P>,
        gui_close: cb_gui_close::<P>,
        gui_set_content_scale: cb_gui_set_content_scale::<P>,
        gui_can_resize: cb_gui_can_resize::<P>,
        gui_check_size_constraint: cb_gui_check_size_constraint::<P>,
        gui_set_size: cb_gui_set_size::<P>,
        midi_mapping_get_param_id: cb_midi_mapping_get_param_id::<P>,
        get_output_param_count: cb_get_output_param_count::<P>,
        get_output_param: cb_get_output_param::<P>,
        set_active: cb_set_active::<P>,
        match_bus_layout: cb_match_bus_layout::<P>,
        layout_bus_channels: cb_layout_bus_channels::<P>,
        match_bus_layout_perbus: cb_match_bus_layout_perbus::<P>,
        param_is_chunked: cb_param_is_chunked::<P>,
        param_presentation: cb_param_presentation::<P>,
        param_presentation_revision: cb_param_presentation_revision::<P>,
        wants_persist_notifications: cb_wants_persist_notifications::<P>,
        set_persist_waker: cb_set_persist_waker::<P>,
        take_persist_change: cb_take_persist_change::<P>,
        param_affects_presentation: cb_param_affects_presentation::<P>,
        param_function_name: cb_param_function_name::<P>,
        set_catalog_waker: cb_set_catalog_waker::<P>,
        take_catalog_changes: cb_take_catalog_changes::<P>,
        track_info_changed: cb_track_info_changed::<P>,
        midi_learn: cb_midi_learn::<P>,
        remap_param_id: cb_remap_param_id::<P>,
        set_processing: cb_set_processing::<P>,
        gui_supports_wayland: cb_gui_supports_wayland::<P>,
        gui_open_wayland: cb_gui_open_wayland::<P>,
        gui_event_fd: cb_gui_event_fd::<P>,
        gui_on_fd: cb_gui_on_fd::<P>,
        gui_on_timer: cb_gui_on_timer::<P>,
        gui_wayland_set_sync: cb_gui_wayland_set_sync::<P>,
        bind_host_queue: cb_bind_host_queue::<P>,
        drain_host_queue: cb_drain_host_queue::<P>,
        perform_menu: cb_perform_menu::<P>,
        gui_param_at: cb_gui_param_at::<P>,
        set_audio_presentation_latency: cb_set_audio_presentation_latency::<P>,
        param_step_count: cb_param_step_count::<P>,
        keyswitches: cb_keyswitches::<P>,
        program_list: cb_program_list::<P>,
        set_program_data: cb_set_program_data::<P>,
    }));

    // Unify with the `Box::leak(Box::new(...))` shape above so every
    // descriptor handed to `moose_vst3_register` lives behind the
    // same kind of leaked allocation. `Vec::leak` produces a
    // `&'static mut [T]` from a heap reallocation that may differ in
    // capacity from len; converting through `into_boxed_slice()`
    // first trims to exact len and lets us route through `Box::leak`
    // alongside `descriptor` and `callbacks`.
    let param_descs: &'static [Vst3ParamDescriptor] = Box::leak(param_descs.into_boxed_slice());

    unsafe {
        ffi::moose_vst3_register(
            std::ptr::from_ref::<Vst3PluginDescriptor>(descriptor),
            std::ptr::from_ref::<Vst3Callbacks>(callbacks),
            param_descs.as_ptr(),
            len_u32(param_descs.len()),
        );
    }
}
