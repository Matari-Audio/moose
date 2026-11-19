//! MIDI input proxy parameters (`IMidiMapping`).

use moose_core::events::EventBody;
use moose_core::export::PluginExport;
use moose_core::midi::{denorm_7bit, denorm_pitch_bend};
use moose_params::ParamInfo;
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// MIDI input proxy parameters
//
// VST3 has no input events for channel-level MIDI; hosts deliver CC /
// pitch bend / channel pressure only to a parameter advertised through
// `IMidiMapping`. Explicit `midi_map` bindings cover parameters the
// plugin *wants* as parameters; these hidden proxies cover everything
// else, so event-consuming plugins hear the same MIDI on VST3 as on
// AU / CLAP / LV2 / VST2. Proxies are not real parameters: never
// serialized into state, never visible to `Params`, only an
// `IMidiMapping` target that turns back into the matching `EventBody`.
// ---------------------------------------------------------------------------

/// Controllers per channel: CC 0..=127, 128 = channel pressure,
/// 129 = pitch bend. Program change (VST3 controller 130) is
/// deliberately not proxied - `kIsProgramChange` parameters interact
/// with unit/program-list metadata, so it stays explicit-binding-only.
pub(crate) const MIDI_PROXY_PER_CHANNEL: u32 = 130;
/// One event-input bus's worth of proxies (16 channels). A plugin
/// gets one bank per declared MIDI input port, so controllers keep
/// their bus attribution - the host queries `IMidiMapping` per bus
/// and a shared id would merge every bus's values into one parameter
/// queue before moose ever saw them.
pub(crate) const MIDI_PROXY_BANK: u32 = 16 * MIDI_PROXY_PER_CHANNEL;
pub(crate) const MIDI_PROXY_PRESSURE: u32 = 128;
pub(crate) const MIDI_PROXY_PITCH_BEND: u32 = 129;

/// Allocate every hidden proxy from the top of the VST3-safe 31-bit
/// domain, skipping real parameter IDs without changing them. The
/// descending order is stable for a given real-ID set and doubles as
/// the logical proxy index for allocation-free binary-search decoding.
pub(crate) fn allocate_midi_proxy_ids(real_params: &[ParamInfo], count: usize) -> Vec<u32> {
    let real_ids: HashSet<u32> = real_params.iter().map(|info| info.id).collect();
    let mut ids = Vec::with_capacity(count);
    let mut candidate = Some(moose_params::PARAM_ID_MAX);
    while ids.len() < count {
        let id = candidate.expect("VST3 parameter ID domain exhausted by MIDI proxies");
        candidate = id.checked_sub(1);
        if !real_ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

pub(crate) fn allocated_midi_proxy_id(
    ids: &[u32],
    port: u8,
    channel: u8,
    controller: u32,
) -> Option<u32> {
    let index = u32::from(port) * MIDI_PROXY_BANK
        + u32::from(channel.min(15)) * MIDI_PROXY_PER_CHANNEL
        + controller;
    ids.get(index as usize).copied()
}

pub(crate) fn allocated_midi_proxy_index(ids: &[u32], id: u32) -> Option<usize> {
    ids.binary_search_by(|candidate| candidate.cmp(&id).reverse())
        .ok()
}

/// `(port, channel, controller)` for an allocated proxy id, `None`
/// for every real parameter ID and other host input.
pub(crate) fn allocated_midi_proxy_decode(ids: &[u32], id: u32) -> Option<(u8, u8, u32)> {
    let index = u32::try_from(allocated_midi_proxy_index(ids, id)?).ok()?;
    #[allow(clippy::cast_possible_truncation)]
    Some((
        (index / MIDI_PROXY_BANK) as u8,
        (index % MIDI_PROXY_BANK / MIDI_PROXY_PER_CHANNEL) as u8,
        index % MIDI_PROXY_PER_CHANNEL,
    ))
}

// Keep the historical arithmetic available to the existing unit
// checks; production proxy IDs use the collision-safe allocator above.
#[cfg(test)]
pub(crate) const MIDI_PROXY_ID_BASE: u32 = moose_params::METER_ID_BASE + 0x1_0000;

#[cfg(test)]
pub(crate) fn midi_proxy_id(port: u8, channel: u8, controller: u32) -> u32 {
    MIDI_PROXY_ID_BASE
        + u32::from(port) * MIDI_PROXY_BANK
        + u32::from(channel.min(15)) * MIDI_PROXY_PER_CHANNEL
        + controller
}

#[cfg(test)]
pub(crate) fn midi_proxy_decode(id: u32) -> Option<(u8, u8, u32)> {
    let rel = id.checked_sub(MIDI_PROXY_ID_BASE)?;
    if rel >= 256 * MIDI_PROXY_BANK {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some((
        (rel / MIDI_PROXY_BANK) as u8,
        (rel % MIDI_PROXY_BANK / MIDI_PROXY_PER_CHANNEL) as u8,
        rel % MIDI_PROXY_PER_CHANNEL,
    ))
}

/// Wheel-centred default for pitch bend, zero for everything else.
pub(crate) fn midi_proxy_default(controller: u32) -> f64 {
    if controller == MIDI_PROXY_PITCH_BEND {
        0.5
    } else {
        0.0
    }
}

/// The MIDI event a proxy change decodes to. `normalized` is the
/// host's `0..=1` wheel/controller position (the shim's denormalize is
/// identity for proxy ids, so the plain value passes through).
pub(crate) fn midi_proxy_event(channel: u8, controller: u32, normalized: f32) -> EventBody {
    match controller {
        MIDI_PROXY_PITCH_BEND => EventBody::PitchBend {
            group: 0,
            channel,
            value: denorm_pitch_bend(normalized * 2.0 - 1.0),
        },
        MIDI_PROXY_PRESSURE => EventBody::ChannelPressure {
            group: 0,
            channel,
            pressure: denorm_7bit(normalized),
        },
        cc => EventBody::ControlChange {
            group: 0,
            channel,
            // Bounded to 0..=127 by `midi_proxy_decode`.
            cc: u8::try_from(cc).unwrap_or(0) & 0x7F,
            value: denorm_7bit(normalized),
        },
    }
}

/// Proxy count for this plugin: the full bank when it accepts MIDI
/// input, zero otherwise (no surface change for non-MIDI plugins).
pub(crate) fn midi_proxy_len<P: PluginExport>() -> usize {
    if P::info().accepts_midi_in {
        MIDI_PROXY_BANK as usize * usize::from(P::info().midi_input_ports)
    } else {
        0
    }
}
