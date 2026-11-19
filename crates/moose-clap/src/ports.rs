//! Audio and note ports: audio-ports, audio-ports-config, configurable-audio-ports, audio-ports-activation, surround and note-ports.

use crate::copy_str_to_buf;
use crate::data::ClapPluginData;
use crate::data_from_plugin;

use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_AUDIO_PORT_PREFERS_64BITS, CLAP_AUDIO_PORT_SUPPORTS_64BITS,
    CLAP_AUDIO_PORTS_RESCAN_LIST, CLAP_EXT_AUDIO_PORTS, CLAP_PORT_MONO, CLAP_PORT_STEREO,
    clap_audio_port_info, clap_host_audio_ports, clap_plugin_audio_ports,
};
use clap_sys::ext::audio_ports_config::{clap_audio_ports_config, clap_plugin_audio_ports_config};
use clap_sys::ext::configurable_audio_ports::clap_audio_port_configuration_request;
#[cfg(feature = "mpe-input")]
use clap_sys::ext::note_ports::CLAP_NOTE_DIALECT_MIDI_MPE;
use clap_sys::ext::note_ports::{
    CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI2, clap_note_port_info,
    clap_plugin_note_ports,
};
use clap_sys::ext::surround::CLAP_PORT_SURROUND;
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::clap_plugin;
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use moose_core::Float;
use moose_core::bus::{BusConfig, BusKind, BusLayout, ChannelConfig};
use moose_core::bus_routing::{BusActivation, BusRouting};
use moose_core::cast::len_u32;
use moose_core::export::PluginExport;
use moose_core::info::MidiDialect;
use moose_core::plugin::PluginRuntime;
use std::ffi::{CStr, c_char};
use std::ptr;
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// Extension: audio_ports
// ---------------------------------------------------------------------------

/// The `bus_layouts()` index the host selected through
/// `clap.audio-ports-config`, clamped in range. Falls back to 0 for a
/// single-layout plugin or a null handle.
pub(crate) unsafe fn selected_layout_index<P: PluginExport>(
    plugin: *const clap_plugin,
    layouts_len: usize,
) -> usize {
    if layouts_len <= 1 || plugin.is_null() {
        return 0;
    }
    let data = unsafe { data_from_plugin::<P>(plugin) };
    (data.selected_config.load(Ordering::Relaxed) as usize).min(layouts_len - 1)
}

/// The CLAP main-port-type string for a channel config. Unsupported custom
/// counts remain untyped.
pub(crate) fn clap_port_type_ptr(channels: ChannelConfig) -> *const c_char {
    match channels {
        ChannelConfig::Mono => CLAP_PORT_MONO.as_ptr(),
        ChannelConfig::Stereo => CLAP_PORT_STEREO.as_ptr(),
        ChannelConfig::Custom(4 | 6 | 8) => CLAP_PORT_SURROUND.as_ptr(),
        ChannelConfig::Custom(_) => ptr::null(),
    }
}

pub(crate) unsafe extern "C" fn audio_ports_count<P: PluginExport>(
    plugin: *const clap_plugin,
    is_input: bool,
) -> u32 {
    let layouts = P::bus_layouts();
    let idx = unsafe { selected_layout_index::<P>(plugin, layouts.len()) };
    let Some(layout) = layouts.get(idx) else {
        return 0;
    };
    if is_input {
        len_u32(layout.inputs.iter().filter(|bus| bus.enabled).count())
    } else {
        len_u32(layout.outputs.iter().filter(|bus| bus.enabled).count())
    }
}

/// CLAP allows one main port per direction, at index 0. Take the role
/// from the bus kind so a layout whose first enabled bus is a sidechain
/// (its main bus disabled) doesn't advertise the sidechain as main.
pub(crate) fn audio_port_main_flag(index: u32, kind: BusKind) -> u32 {
    if index == 0 && kind == BusKind::Main {
        CLAP_AUDIO_PORT_IS_MAIN
    } else {
        0
    }
}

pub(crate) unsafe extern "C" fn audio_ports_get<P: PluginExport>(
    plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    unsafe {
        let layouts = P::bus_layouts();
        let idx = selected_layout_index::<P>(plugin, layouts.len());
        let Some(layout) = layouts.get(idx) else {
            return false;
        };

        let buses = if is_input {
            &layout.inputs
        } else {
            &layout.outputs
        };

        let Some(bus) = buses.iter().filter(|bus| bus.enabled).nth(index as usize) else {
            return false;
        };

        let out = &mut *info;
        out.id = index;
        out.name = [0; CLAP_NAME_SIZE];
        copy_str_to_buf(&mut out.name, bus.name);
        out.channel_count = bus.channels.channel_count();
        out.flags = audio_port_main_flag(index, bus.kind);
        // f64 plugins take the host's 64-bit wire directly (zero
        // copy, no precision loss at the boundary); the process loop
        // reads whichever of data32/data64 the host picked per port.
        if <P as PluginRuntime>::Sample::IS_F64 {
            out.flags |= CLAP_AUDIO_PORT_SUPPORTS_64BITS | CLAP_AUDIO_PORT_PREFERS_64BITS;
        }
        out.port_type = clap_port_type_ptr(bus.channels);
        out.in_place_pair = CLAP_INVALID_ID;

        true
    }
}

// Match the canonical channel arrangements exposed by the VST3 adapter.
pub(crate) fn surround_channels(channels: ChannelConfig) -> &'static [u8] {
    match channels {
        ChannelConfig::Custom(4) => &[0, 1, 4, 5],
        ChannelConfig::Custom(6) => &[0, 1, 2, 3, 4, 5],
        ChannelConfig::Custom(8) => &[0, 1, 2, 3, 4, 5, 9, 10],
        _ => &[],
    }
}

pub(crate) unsafe extern "C" fn surround_mask<P: PluginExport>(
    _: *const clap_plugin,
    mask: u64,
) -> bool {
    P::bus_layouts()
        .iter()
        .flat_map(|layout| layout.inputs.iter().chain(&layout.outputs))
        .filter(|bus| bus.enabled)
        .any(|bus| {
            let channels = surround_channels(bus.channels);
            !channels.is_empty()
                && channels
                    .iter()
                    .fold(0u64, |bits, channel| bits | (1u64 << channel))
                    == mask
        })
}

pub(crate) unsafe extern "C" fn surround_map<P: PluginExport>(
    plugin: *const clap_plugin,
    is_input: bool,
    port: u32,
    destination: *mut u8,
    capacity: u32,
) -> u32 {
    let layouts = P::bus_layouts();
    // SAFETY: CLAP supplies a live plugin instance for this main-thread callback.
    let index = unsafe { selected_layout_index::<P>(plugin, layouts.len()) };
    let Some(layout) = layouts.get(index) else {
        return 0;
    };
    let buses = if is_input {
        &layout.inputs
    } else {
        &layout.outputs
    };
    let Some(bus) = buses.iter().filter(|bus| bus.enabled).nth(port as usize) else {
        return 0;
    };
    let channels = surround_channels(bus.channels);
    if channels.is_empty() || destination.is_null() || (capacity as usize) < channels.len() {
        return 0;
    }
    // SAFETY: CLAP supplies a writable buffer of capacity bytes, checked above.
    unsafe {
        ptr::copy_nonoverlapping(channels.as_ptr(), destination, channels.len());
    }
    len_u32(channels.len())
}

pub(crate) fn make_audio_ports_extension<P: PluginExport>() -> clap_plugin_audio_ports {
    clap_plugin_audio_ports {
        count: Some(audio_ports_count::<P>),
        get: Some(audio_ports_get::<P>),
    }
}

// ---------------------------------------------------------------------------
// Extension: audio_ports_config (switch between the plugin's bus layouts)
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn audio_ports_config_count<P: PluginExport>(
    _plugin: *const clap_plugin,
) -> u32 {
    len_u32(P::bus_layouts().len())
}

pub(crate) fn main_bus(buses: &[BusConfig]) -> Option<&BusConfig> {
    buses
        .iter()
        .find(|bus| bus.enabled)
        .filter(|bus| bus.kind == BusKind::Main)
}

pub(crate) unsafe extern "C" fn audio_ports_config_get<P: PluginExport>(
    _plugin: *const clap_plugin,
    index: u32,
    config: *mut clap_audio_ports_config,
) -> bool {
    unsafe {
        let layouts = P::bus_layouts();
        let Some(layout) = layouts.get(index as usize) else {
            return false;
        };
        let out = &mut *config;
        // Config id == index into `bus_layouts()`; `select` maps back.
        out.id = index;
        out.name = [0; CLAP_NAME_SIZE];
        let name = format!(
            "{} in / {} out",
            layout.total_input_channels(),
            layout.total_output_channels()
        );
        copy_str_to_buf(&mut out.name, layout.name.unwrap_or(&name));
        out.input_port_count = len_u32(layout.inputs.iter().filter(|bus| bus.enabled).count());
        out.output_port_count = len_u32(layout.outputs.iter().filter(|bus| bus.enabled).count());
        // The main port is the first enabled bus, and only if it is Main.
        let main_input = main_bus(&layout.inputs);
        out.has_main_input = main_input.is_some();
        out.main_input_channel_count = main_input.map_or(0, |b| b.channels.channel_count());
        out.main_input_port_type =
            main_input.map_or(ptr::null(), |b| clap_port_type_ptr(b.channels));
        let main_output = main_bus(&layout.outputs);
        out.has_main_output = main_output.is_some();
        out.main_output_channel_count = main_output.map_or(0, |b| b.channels.channel_count());
        out.main_output_port_type =
            main_output.map_or(ptr::null(), |b| clap_port_type_ptr(b.channels));
        true
    }
}

pub(crate) fn clear_ports_inactive<P: PluginExport>(data: &ClapPluginData<P>) {
    for mask in &data.ports_inactive {
        mask.store(0, Ordering::Relaxed);
    }
}

pub(crate) unsafe extern "C" fn audio_ports_config_select<P: PluginExport>(
    plugin: *const clap_plugin,
    config_id: clap_id,
) -> bool {
    unsafe {
        if (config_id as usize) >= P::bus_layouts().len() {
            return false;
        }
        // Called on the main thread while deactivated; the host rescans
        // `audio_ports` afterward, which now reports this layout.
        // Per `clap.audio-ports-activation`, selecting a config invalidates
        // the cached port activation state and resets ports to active.
        let data = data_from_plugin::<P>(plugin);
        data.selected_config.store(config_id, Ordering::Relaxed);
        clear_ports_inactive(data);
        true
    }
}

/// Ask the host to re-read the whole port list. Inactive, main thread.
///
/// # Safety
/// `data.host` is null or the live host of this instance.
pub(crate) unsafe fn host_rescan_audio_ports<P: PluginExport>(data: &ClapPluginData<P>) {
    // Per `clap.audio-ports-activation`, `CLAP_AUDIO_PORTS_RESCAN_LIST`
    // invalidates the activation state and resets ports to active.
    clear_ports_inactive(data);
    let host = data.host;
    // SAFETY: caller contract; `get_extension` is [thread-safe].
    unsafe {
        if host.is_null() {
            return;
        }
        let Some(get) = (*host).get_extension else {
            return;
        };
        let ext = get(host, CLAP_EXT_AUDIO_PORTS.as_ptr()).cast::<clap_host_audio_ports>();
        if !ext.is_null()
            && let Some(rescan) = (*ext).rescan
        {
            rescan(host, CLAP_AUDIO_PORTS_RESCAN_LIST);
        }
    }
}

// ---------------------------------------------------------------------------
// Extension: configurable-audio-ports (host proposes a port configuration)
// ---------------------------------------------------------------------------

/// The first declared layout (the selected one first) that satisfies every
/// request: each named port exists among the enabled buses, with that
/// channel count and, when the host gives one, that port type.
///
/// # Safety
/// `requests` points at `count` readable requests whose `port_type`s are
/// null or NUL-terminated.
pub(crate) unsafe fn layout_for_requests<P: PluginExport>(
    plugin: *const clap_plugin,
    requests: *const clap_audio_port_configuration_request,
    count: u32,
) -> Option<usize> {
    let requests: &[clap_audio_port_configuration_request] = if count == 0 {
        &[]
    } else if requests.is_null() {
        return None;
    } else {
        // SAFETY: the host passes `count` requests (caller contract).
        unsafe { std::slice::from_raw_parts(requests, count as usize) }
    };
    let layouts = P::bus_layouts();
    // SAFETY: a live plugin for this main-thread callback.
    let selected = unsafe { selected_layout_index::<P>(plugin, layouts.len()) };
    let fits = |layout: &BusLayout| {
        requests.iter().all(|request| {
            let buses = if request.is_input {
                &layout.inputs
            } else {
                &layout.outputs
            };
            let Some(bus) = buses
                .iter()
                .filter(|bus| bus.enabled)
                .nth(request.port_index as usize)
            else {
                return false;
            };
            if bus.channels.channel_count() != request.channel_count {
                return false;
            }
            let ours = clap_port_type_ptr(bus.channels);
            // SAFETY: NUL-terminated or null (caller contract).
            let wanted = (!request.port_type.is_null())
                .then(|| unsafe { CStr::from_ptr(request.port_type) })
                .filter(|t| !t.is_empty());
            // SAFETY: ours are static CLAP constants.
            let type_fits = wanted
                .is_none_or(|wanted| !ours.is_null() && wanted == unsafe { CStr::from_ptr(ours) });
            // Mono and stereo ignore details. Surround details must match
            // our canonical channel map; ambisonic types never match.
            let details_fit = request.port_details.is_null()
                || wanted == Some(CLAP_PORT_MONO)
                || wanted == Some(CLAP_PORT_STEREO)
                || (wanted == Some(CLAP_PORT_SURROUND) && {
                    let map = surround_channels(bus.channels);
                    // SAFETY: a surround request's details are
                    // `channel_count` channel ids.
                    !map.is_empty()
                        && map
                            == unsafe {
                                std::slice::from_raw_parts(
                                    request.port_details.cast::<u8>(),
                                    request.channel_count as usize,
                                )
                            }
                });
            type_fits && details_fit
        })
    };
    std::iter::once(selected)
        .chain(0..layouts.len())
        .find(|&index| layouts.get(index).is_some_and(fits))
}

pub(crate) unsafe extern "C" fn configurable_ports_can_apply<P: PluginExport>(
    plugin: *const clap_plugin,
    requests: *const clap_audio_port_configuration_request,
    count: u32,
) -> bool {
    // SAFETY: the host's request array, per the CLAP contract.
    unsafe { layout_for_requests::<P>(plugin, requests, count).is_some() }
}

pub(crate) unsafe extern "C" fn configurable_ports_apply<P: PluginExport>(
    plugin: *const clap_plugin,
    requests: *const clap_audio_port_configuration_request,
    count: u32,
) -> bool {
    // SAFETY: as in `configurable_ports_can_apply`; called on the main
    // thread while deactivated, like `audio_ports_config_select`.
    unsafe {
        let Some(index) = layout_for_requests::<P>(plugin, requests, count) else {
            return false;
        };
        let data = data_from_plugin::<P>(plugin);
        if data.active.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(index) = u32::try_from(index) else {
            return false;
        };
        data.selected_config.store(index, Ordering::Relaxed);
        clear_ports_inactive(data);
        true
    }
}

// ---------------------------------------------------------------------------
// Extension: audio-ports-activation (host switches ports on and off)
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn ports_activation_while_processing(_: *const clap_plugin) -> bool {
    false
}

pub(crate) unsafe extern "C" fn ports_activation_set_active<P: PluginExport>(
    plugin: *const clap_plugin,
    is_input: bool,
    port_index: u32,
    is_active: bool,
    _sample_size: u32,
) -> bool {
    // SAFETY: a live plugin; called while not processing.
    unsafe {
        let layouts = P::bus_layouts();
        let selected = selected_layout_index::<P>(plugin, layouts.len());
        let Some(layout) = layouts.get(selected) else {
            return false;
        };
        let buses = if is_input {
            &layout.inputs
        } else {
            &layout.outputs
        };
        if port_index >= 32 || port_index as usize >= buses.iter().filter(|b| b.enabled).count() {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        let mask = &data.ports_inactive[usize::from(!is_input)];
        if is_active {
            mask.fetch_and(!(1 << port_index), Ordering::Relaxed);
        } else {
            mask.fetch_or(1 << port_index, Ordering::Relaxed);
        }
        data.ports_activation_used.store(true, Ordering::Relaxed);
        // Active but not processing: route the change into this block's
        // routing now; otherwise `activate` applies it.
        if data.active.load(Ordering::Relaxed) {
            let mut audio = data.audio.enter();
            apply_port_activation(&mut audio.bus_routing, data, layout);
        }
        true
    }
}

/// Write the host's port activation onto the routing built for `layout`.
/// Routing indexes every declared bus; CLAP port indices count only the
/// enabled ones.
pub(crate) fn apply_port_activation<P: PluginExport>(
    routing: &mut BusRouting,
    data: &ClapPluginData<P>,
    layout: &BusLayout,
) {
    if !data.ports_activation_used.load(Ordering::Relaxed) {
        return;
    }
    for (dir, buses) in [&layout.inputs, &layout.outputs].into_iter().enumerate() {
        let inactive = data.ports_inactive[dir].load(Ordering::Relaxed);
        let enabled = buses.iter().enumerate().filter(|(_, bus)| bus.enabled);
        for (port, (declared, _)) in enabled.enumerate() {
            let activation = if port < 32 && inactive & (1 << port) != 0 {
                BusActivation::Inactive
            } else {
                BusActivation::Active
            };
            if dir == 0 {
                routing.set_input_activation(declared, activation);
            } else {
                routing.set_output_activation(declared, activation);
            }
        }
    }
}

pub(crate) fn make_audio_ports_config_extension<P: PluginExport>() -> clap_plugin_audio_ports_config
{
    clap_plugin_audio_ports_config {
        count: Some(audio_ports_config_count::<P>),
        get: Some(audio_ports_config_get::<P>),
        select: Some(audio_ports_config_select::<P>),
    }
}

// ---------------------------------------------------------------------------
// Extension: note_ports (only for instruments)
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn note_ports_count<P: PluginExport>(
    _plugin: *const clap_plugin,
    is_input: bool,
) -> u32 {
    // Declare the plugin's MIDI ports for this direction. Count comes
    // from `PluginInfo` (category default of 0-or-1, raised by
    // `midi_input_ports` / `midi_output_ports`). A plain audio effect
    // declares neither direction.
    let info = P::info();
    let ports = if is_input {
        info.midi_input_ports
    } else {
        info.midi_output_ports
    };
    u32::from(ports)
}

pub(crate) unsafe extern "C" fn note_ports_get<P: PluginExport>(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    unsafe {
        let ports = if is_input {
            u32::from(P::info().midi_input_ports)
        } else {
            u32::from(P::info().midi_output_ports)
        };
        if index >= ports {
            return false;
        }

        let out = &mut *info;
        // Port ids must be unique across the plugin. Fold the direction
        // bit and the index together: input k -> 2k, output k -> 2k+1.
        out.id = (index << 1) | u32::from(!is_input);
        // Notes go out as `CLAP_EVENT_NOTE`, but every other
        // channel-voice message and SysEx go out as `CLAP_EVENT_MIDI`
        // (raw MIDI dialect), so advertise both dialects or a
        // dialect-routing host drops the raw-MIDI events. Keep CLAP
        // preferred: our own note events are CLAP-dialect, and
        // preferring MIDI would also pull host input through the
        // raw-MIDI path.
        out.supported_dialects = CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI;
        #[cfg(feature = "mpe-input")]
        if is_input {
            out.supported_dialects |= CLAP_NOTE_DIALECT_MIDI_MPE;
        }
        out.preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
        // A port that opted into MIDI 2.0 also carries UMP: advertise
        // the MIDI2 dialect so the host routes `CLAP_EVENT_MIDI2` to us
        // (in) and reads it back (out). Still CLAP-preferred - MIDI2 is
        // the opt-in native path, not the default.
        let dialect = if is_input {
            P::info().midi_input_dialect
        } else {
            P::info().midi_output_dialect
        };
        if dialect == MidiDialect::Midi2 {
            out.supported_dialects |= CLAP_NOTE_DIALECT_MIDI2;
        }
        out.name = [0; CLAP_NAME_SIZE];
        let base = if is_input {
            "Note Input"
        } else {
            "Note Output"
        };
        // Number the ports only when there's more than one, so the
        // single-port case keeps its plain label. Not the audio thread -
        // allocation here is fine.
        let name = if ports > 1 {
            format!("{base} {}", index + 1)
        } else {
            base.to_string()
        };
        copy_str_to_buf(&mut out.name, &name);

        true
    }
}

pub(crate) fn make_note_ports_extension<P: PluginExport>() -> clap_plugin_note_ports {
    clap_plugin_note_ports {
        count: Some(note_ports_count::<P>),
        get: Some(note_ports_get::<P>),
    }
}
