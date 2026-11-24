//! Plugin registration: owned descriptors and typed factory backend handed to the COM layer.

use crate::backend::{ParamDescriptor, PluginDescriptor, Vst3Backend, Vst3FactoryBackend};
use crate::instance::Vst3Instance;
use crate::lifecycle::{
    descriptor_bus_kinds, layout_bus_channels, match_bus_layout_perbus, vst3_topology_consistent,
};
use crate::midi_proxy::{
    MIDI_PROXY_PER_CHANNEL, MIDI_PROXY_PITCH_BEND, MIDI_PROXY_PRESSURE, allocate_midi_proxy_ids,
    allocated_midi_proxy_id, midi_proxy_default, midi_proxy_len,
};

use moose_core::bus::{BusConfig, BusLayout};
use moose_core::bus_routing::bus_layouts_fit_routing;
use moose_core::export::PluginExport;
use moose_core::info::{PluginCategory, PluginInfo, resolve_name_override};
use moose_core::plugin::PluginRuntime;
use moose_core::state;
use moose_core::wrapper::{default_io_channels, log_missing_bus_layout, run_register};
use moose_params::sample::Float;
use moose_params::{ParamFlags, ParamRange};
use std::marker::PhantomData;

/// Plugin display-name surfaced as `PClassInfo::name`.
pub(crate) fn resolved_plugin_name(info: &PluginInfo) -> &'static str {
    resolve_name_override(info.vst3_name, info.name)
}

/// Largest structural input and output widths across all declared layouts.
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

struct PluginFactory<P: PluginExport>(PhantomData<fn() -> P>);

impl<P: PluginExport> Vst3FactoryBackend for PluginFactory<P> {
    fn create(&self) -> Option<Box<dyn Vst3Backend>> {
        Vst3Instance::<P>::create().map(|inst| Box::new(inst) as Box<dyn Vst3Backend>)
    }

    fn layout_bus_channels(&self, layout_index: u32, is_output: bool, bus_index: u32) -> u32 {
        layout_bus_channels::<P>(layout_index, is_output, bus_index)
    }

    fn match_bus_layout_perbus(&self, in_channels: &[u32], out_channels: &[u32]) -> Option<u32> {
        match_bus_layout_perbus::<P>(in_channels, out_channels)
    }
}

pub fn register_vst3<P: PluginExport>() {
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

// VST3 `ParameterInfo::ParameterFlags`.
pub(crate) const VST3_PARAM_CAN_AUTOMATE: i32 = 1 << 0;
pub(crate) const VST3_PARAM_IS_READ_ONLY: i32 = 1 << 1;
pub(crate) const VST3_PARAM_IS_LIST: i32 = 1 << 3;
pub(crate) const VST3_PARAM_IS_HIDDEN: i32 = 1 << 4;
pub(crate) const VST3_PARAM_IS_BYPASS: i32 = 1 << 16;

#[allow(clippy::too_many_lines)]
pub(crate) fn register_vst3_inner<P: PluginExport>(num_inputs: u32, num_outputs: u32) {
    let info = P::info();
    let param_infos = P::param_infos_static();
    let midi_proxy_ids = allocate_midi_proxy_ids(&param_infos, midi_proxy_len::<P>());

    let mut param_descs: Vec<ParamDescriptor> = Vec::with_capacity(param_infos.len());
    for pi in &param_infos {
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
        if matches!(pi.range, ParamRange::Enum { .. }) {
            flags |= VST3_PARAM_IS_LIST;
        }
        let step_count = pi.range.step_count();

        param_descs.push(ParamDescriptor {
            id: pi.id,
            name: pi.name.to_owned(),
            short_name: pi.short_name.to_owned(),
            units: pi.unit.as_str().to_owned(),
            default_normalized: pi.range.normalize(pi.default_plain),
            #[allow(clippy::cast_possible_wrap)]
            step_count: step_count.map_or(0, |n| n.get() as i32),
            flags,
            group: pi.group.to_owned(),
        });
    }

    if info.accepts_midi_in {
        for port in 0..info.midi_input_ports {
            let (name_prefix, short_prefix) = if info.midi_input_ports > 1 {
                (format!("MIDI In {} ", port + 1), format!("I{}", port + 1))
            } else {
                (String::from("MIDI "), String::new())
            };
            for channel in 0u8..16 {
                for controller in 0..MIDI_PROXY_PER_CHANNEL {
                    let (name, short_name) = match controller {
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
                    param_descs.push(ParamDescriptor {
                        id: allocated_midi_proxy_id(&midi_proxy_ids, port, channel, controller)
                            .expect("VST3 MIDI proxy allocation is complete"),
                        name,
                        short_name,
                        units: String::new(),
                        default_normalized: midi_proxy_default(controller),
                        step_count: 0,
                        flags: VST3_PARAM_IS_HIDDEN,
                        group: String::from("MIDI"),
                    });
                }
            }
        }
    }

    let subcategories = match (info.category, info.vst3_subcategory) {
        (PluginCategory::Instrument, Some(sub)) => format!("Instrument|{sub}"),
        (PluginCategory::Instrument, None) => "Instrument|Synth".to_string(),
        (PluginCategory::Effect, Some(sub)) => format!("Fx|{sub}"),
        (PluginCategory::Effect, None) => "Fx".to_string(),
        (PluginCategory::NoteEffect, _) => "Fx|Event".to_string(),
        (PluginCategory::Analyzer, _) => "Fx|Analyzer".to_string(),
        (PluginCategory::Tool, _) => "Fx|Tools".to_string(),
    };

    let (input_bus_kinds, output_bus_kinds) = descriptor_bus_kinds::<P>();
    let (max_inputs, max_outputs) = max_layout_channels(&P::bus_layouts());
    let compatibility_json = P::vst3_compatibility_json()
        .filter(|json| !json.is_empty())
        .map(str::to_owned);

    let descriptor = PluginDescriptor {
        name: resolved_plugin_name(&info).to_owned(),
        vendor: info.vendor.to_owned(),
        url: info.url.to_owned(),
        email: String::new(),
        version: info.version.to_owned(),
        cid: state::resolve_vst3_cid(P::vst3_class_id(), info.vst3_id),
        category: String::from("Audio Module Class"),
        subcategories,
        num_inputs,
        num_outputs,
        input_bus_kinds,
        output_bus_kinds,
        midi_output_ports: i32::from(info.midi_output_ports),
        midi_input_ports: i32::from(info.midi_input_ports),
        supports_f64: <P as PluginRuntime>::Sample::IS_F64,
        max_inputs,
        max_outputs,
        compatibility_json,
    };

    crate::com::register(
        descriptor,
        Box::new(PluginFactory::<P>(PhantomData)),
        param_descs,
    );
}
