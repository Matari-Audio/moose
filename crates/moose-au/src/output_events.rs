//! Output events from the plugin to the host: MIDI 1, MIDI 2 and parameter changes.

use crate::callbacks::ump_protocol_accepts;
use crate::callbacks::ump_word_count_for_type;
use crate::instance::AuAudio;
use crate::instance::AuInstance;
use crate::instance::queue_sysex_input;
use crate::instance::try_queue_sysex_input;

use crate::ffi::{
    AU_NATIVE_CARRIER_BYTES, AU_NATIVE_CARRIER_UMP, AU_NATIVE_EVENT_MIDI1, AU_NATIVE_EVENT_SYSEX,
    AU_NATIVE_EVENT_UMP, AU_OUTPUT_EMITTED, AU_OUTPUT_END, AU_OUTPUT_INVALID, AU_OUTPUT_QUEUE_FULL,
    AU_OUTPUT_UNSUPPORTED, AuMidiEvent, AuNativeEvent, AuUmpEvent,
};
use moose_core::Float;
use moose_core::cast::len_u32;
use moose_core::events::{
    Event, EventBody, EventList, ExactEventBody, ExactEventMetadata, ExactEventQualifiers,
    LosslessEventCursor, LosslessEventRef, OutputEventStatus, RawMidi1,
};
use moose_core::export::PluginExport;
use moose_core::info::MidiDialect;
use moose_core::midi::{
    downconvert_to_midi1, event_to_midi1, pitch_bend_to_bytes, route_midi_port, upconvert_to_midi2,
};
use moose_core::ump::{
    decode_ump_channel_voice_2, encode_sysex7_packet, encode_ump_channel_voice_1,
    encode_ump_channel_voice_2, sysex7_packet_count,
};
use moose_core::wrapper::run_extern_callback_with;
use moose_params::Params;
use std::slice;

// ---------------------------------------------------------------------------
// Output event callbacks (plugin → host MIDI)
// ---------------------------------------------------------------------------

// UMP MIDI 2.0 CV decoder lives in `moose-core::ump` so the same
// codec backs CLAP's `CLAP_EVENT_MIDI2` path and AU's MIDIEventList
// path.

/// Map a moose `Event` body to a 3-byte AU MIDI packet. Returns
/// `None` for event types that don't fit (MIDI 2.0, `ParamChange`,
/// Transport, etc.).
pub(crate) fn try_encode_au_midi(event: &Event) -> Option<AuMidiEvent> {
    // The MIDI 1.0 byte output path (AU v2, and AU v3 in 1.0-protocol
    // mode). AU v3 in 2.0 mode emits UMP via `try_encode_au_ump`, so a
    // 2.0 variant only reaches here on a 1.0 transport - down-convert it.
    let body = downconvert_to_midi1(&event.body).unwrap_or(event.body);
    let (status, data1, data2) = match &body {
        EventBody::NoteOn {
            channel,
            note,
            velocity,
            ..
        } => (0x90 | (channel & 0x0F), *note, *velocity),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
            ..
        } => (0x80 | (channel & 0x0F), *note, *velocity),
        EventBody::ControlChange {
            channel, cc, value, ..
        } => (0xB0 | (channel & 0x0F), *cc, *value),
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
            ..
        } => (0xA0 | (channel & 0x0F), *note, *pressure),
        EventBody::ChannelPressure {
            channel, pressure, ..
        } => (0xD0 | (channel & 0x0F), *pressure, 0),
        EventBody::PitchBend { channel, value, .. } => {
            let (lsb, msb) = pitch_bend_to_bytes(*value);
            (0xE0 | (channel & 0x0F), lsb, msb)
        }
        EventBody::ProgramChange {
            channel, program, ..
        } => (0xC0 | (channel & 0x0F), *program, 0),
        _ => return None,
    };
    Some(AuMidiEvent {
        sample_offset: event.sample_offset,
        status,
        data1,
        data2,
        port: event.port,
    })
}

/// Plugin latency in samples, for the host's delay compensation.
/// Reads the atomic cache the audio thread refreshes each block (and
/// `cb_reset` / `cb_create` seed) rather than touching the plugin,
/// so a host's main-thread property query never contends with render.
pub(crate) unsafe extern "C" fn cb_latency_samples<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u32 {
    unsafe {
        if ctx.is_null() {
            return 0;
        }
        (*ctx.cast::<AuInstance<P>>()).delay.latency()
    }
}

/// Plugin release-tail length in samples. Same cache/threading story
/// as [`cb_latency_samples`].
pub(crate) unsafe extern "C" fn cb_tail_samples<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u32 {
    unsafe {
        if ctx.is_null() {
            return 0;
        }
        (*ctx.cast::<AuInstance<P>>()).delay.tail()
    }
}

pub(crate) unsafe extern "C" fn cb_output_event_count<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(audio) = inst.audio.try_enter() else {
            return 0;
        };
        let n = audio
            .output_events
            .iter()
            .filter(|e| try_encode_au_midi(e).is_some())
            .count();
        len_u32(n)
    }
}

pub(crate) unsafe extern "C" fn cb_output_event_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut AuMidiEvent,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut audio) = inst.audio.try_enter() else {
            return;
        };
        let scr = &mut *audio;
        if let Some(mut packet) = nth_matching(
            &scr.output_events,
            index as usize,
            &mut scr.midi_drain_cursor,
            try_encode_au_midi,
        ) {
            // Route to the plugin's chosen MIDI output cable; an
            // out-of-range port lands on 0. The appex passes `port`
            // straight to `midiOutputEventBlock`.
            packet.port = route_midi_port(packet.port, P::info().midi_output_ports);
            *out = packet;
        }
    }
}

/// The UMP protocol a `MIDIEventList` stream is declared in. Crosses
/// the appex FFI as a `u32` (`1` / `2`); anything else defaults to
/// 2.0, the protocol `CoreMIDI` hosts translate natively.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UmpProtocol {
    Midi1,
    Midi2,
}

impl UmpProtocol {
    pub(crate) fn from_wire(protocol: u32) -> Self {
        if protocol == 1 {
            Self::Midi1
        } else {
            Self::Midi2
        }
    }
}

/// Encode a channel-voice event into UMP for AU v3's `MIDIEventList`
/// output, in the stream's declared protocol. The UMP spec forbids
/// mixing MT 0x2 (1.0 CV) and MT 0x4 (2.0 CV) packets in one protocol
/// stream, so events convert across dialects instead: a 1.0 body
/// widens for a 2.0 stream, a 2.0 body narrows (lossy) for a 1.0
/// stream, and a 2.0-only body with no 1.0 form drops there. Returns
/// `None` for bodies that aren't channel voice (`SysEx` flattens to
/// its own packet chain in [`au_ump_packet_at`]; transport /
/// automation don't ride UMP).
pub(crate) fn try_encode_au_ump(event: &Event, protocol: UmpProtocol) -> Option<AuUmpEvent> {
    let (word_count, words) = match protocol {
        UmpProtocol::Midi2 => (
            2u8,
            encode_ump_channel_voice_2(&event.body).or_else(|| {
                upconvert_to_midi2(&event.body)
                    .as_ref()
                    .and_then(encode_ump_channel_voice_2)
            })?,
        ),
        UmpProtocol::Midi1 => (
            1u8,
            encode_ump_channel_voice_1(&event.body).or_else(|| {
                downconvert_to_midi1(&event.body)
                    .as_ref()
                    .and_then(encode_ump_channel_voice_1)
            })?,
        ),
    };
    Some(AuUmpEvent {
        sample_offset: event.sample_offset,
        cable: event.port,
        word_count,
        reserved: [0; 2],
        words,
    })
}

/// UMP packets `event` contributes to the output stream: channel voice
/// is one packet (or zero when it has no form in the stream's
/// protocol), `SysEx` is its whole `SysEx`-7 chain (one 64-bit packet
/// per 6 payload bytes, valid in either protocol).
pub(crate) fn au_ump_packet_count(list: &EventList, event: &Event, protocol: UmpProtocol) -> usize {
    if matches!(event.body, EventBody::SysEx { .. }) {
        sysex7_packet_count(list.sysex_bytes(&event.body).len())
    } else {
        usize::from(try_encode_au_ump(event, protocol).is_some())
    }
}

/// Resume point for the sequential UMP output drain. `output_ump_at`
/// addresses the flattened packet stream by index; without a cursor
/// every call rescans from the head, turning a SysEx-heavy drain
/// quadratic (one large `SysEx` is thousands of packets in a single
/// render).
pub(crate) struct UmpDrainCursor {
    /// Packets contributed by events before [`Self::event_pos`].
    pub(crate) packets_before: usize,
    /// Event position the next lookup resumes from.
    pub(crate) event_pos: usize,
    /// Protocol the positions were computed for.
    pub(crate) protocol: UmpProtocol,
}

impl UmpDrainCursor {
    /// Head of the stream; also the per-block reset value.
    pub(crate) const HEAD: Self = Self {
        packets_before: 0,
        event_pos: 0,
        protocol: UmpProtocol::Midi2,
    };
}

/// Resume point for an ascending-index drain over the output events that
/// `pick` accepts (`output_event_at`, `output_sysex_at`). The appex asks
/// for indices 0, 1, 2, ... in order, so resuming makes the whole drain
/// O(n) instead of O(n²). Chosen over encoding a packet list per block:
/// no second buffer to size, and the same shape as [`UmpDrainCursor`].
#[derive(Clone, Copy)]
pub(crate) struct NthCursor {
    /// Matches before [`Self::event_pos`].
    pub(crate) matched_before: usize,
    /// Event position the next lookup resumes from.
    pub(crate) event_pos: usize,
}

impl NthCursor {
    /// Head of the list; also the per-block reset value.
    pub(crate) const HEAD: Self = Self {
        matched_before: 0,
        event_pos: 0,
    };
}

/// The `index`-th event `pick` maps to `Some`, resuming from `cursor`
/// (a backwards query restarts at the head).
pub(crate) fn nth_matching<'a, T>(
    list: &'a EventList,
    index: usize,
    cursor: &mut NthCursor,
    mut pick: impl FnMut(&'a Event) -> Option<T>,
) -> Option<T> {
    if index < cursor.matched_before {
        *cursor = NthCursor::HEAD;
    }
    let mut seen = cursor.matched_before;
    for pos in cursor.event_pos..list.len() {
        let Some(found) = list.get(pos).and_then(&mut pick) else {
            continue;
        };
        if seen == index {
            // Resume here: a repeat of `index` finds this event again.
            *cursor = NthCursor {
                matched_before: seen,
                event_pos: pos,
            };
            return Some(found);
        }
        seen += 1;
    }
    None
}

/// The `index`-th packet of the flattened UMP output stream. Resumes
/// from `cursor` when the query continues forward in the same
/// protocol (the appex drains indices in ascending order); a
/// backwards or cross-protocol query restarts at the head.
pub(crate) fn au_ump_packet_at(
    list: &EventList,
    index: usize,
    protocol: UmpProtocol,
    cursor: &mut UmpDrainCursor,
) -> Option<AuUmpEvent> {
    if protocol != cursor.protocol || index < cursor.packets_before {
        *cursor = UmpDrainCursor {
            protocol,
            ..UmpDrainCursor::HEAD
        };
    }
    let mut remaining = index - cursor.packets_before;
    // Indexed from the cursor (`get` is O(1)); `iter().skip()` would walk
    // from the head on every call.
    for (pos, event) in (cursor.event_pos..list.len()).filter_map(|p| Some((p, list.get(p)?))) {
        let n = au_ump_packet_count(list, event, protocol);
        if remaining < n {
            cursor.event_pos = pos;
            cursor.packets_before = index - remaining;
            if matches!(event.body, EventBody::SysEx { .. }) {
                // SysEx carries no group; packets go out on group 0. The
                // cable still routes by the event's port.
                let words = encode_sysex7_packet(0, list.sysex_bytes(&event.body), remaining)?;
                return Some(AuUmpEvent {
                    sample_offset: event.sample_offset,
                    cable: event.port,
                    word_count: 2,
                    reserved: [0; 2],
                    words: [words[0], words[1], 0, 0],
                });
            }
            return try_encode_au_ump(event, protocol);
        }
        remaining -= n;
    }
    None
}

pub(crate) unsafe extern "C" fn cb_output_ump_count<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    protocol: u32,
) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(audio) = inst.audio.try_enter() else {
            return 0;
        };
        let list = &audio.output_events;
        let protocol = UmpProtocol::from_wire(protocol);
        len_u32(
            list.iter()
                .map(|e| au_ump_packet_count(list, e, protocol))
                .sum(),
        )
    }
}

pub(crate) unsafe extern "C" fn cb_output_ump_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    protocol: u32,
    index: u32,
    out: *mut AuUmpEvent,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut audio) = inst.audio.try_enter() else {
            return;
        };
        let scr = &mut *audio;
        if let Some(mut packet) = au_ump_packet_at(
            &scr.output_events,
            index as usize,
            UmpProtocol::from_wire(protocol),
            &mut scr.ump_drain_cursor,
        ) {
            // Route the cable like the byte path (out-of-range lands
            // on 0); the appex passes it to `midiOutputEventListBlock`.
            packet.cable = route_midi_port(packet.cable, P::info().midi_output_ports);
            *out = packet;
        }
    }
}

/// `SysEx` input for AU v2. The shim's `au_v2_sysex` strips the
/// `0xF0`/`0xF7` framing and calls this once per complete message. AU v2
/// does **not** serialize `MusicDeviceSysEx` against `AudioUnitRender`,
/// so the host may call this on any thread while `cb_process` is running
/// on the audio thread. It must therefore never touch the audio-thread-
/// owned scratch: it copies the bytes into a recycled buffer and queues
/// it lock-free ([`SysExQueue`]); `cb_process` drains the queue into the
/// event pool at the block top. AU v3 takes the in-line `events2`
/// `SysEx`-7/8 path instead and never calls this.
pub(crate) unsafe extern "C" fn cb_au_push_sysex_input<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_offset: u32,
    bytes: *const u8,
    len: u32,
) {
    // Firewalled like the other extern boundaries: a panic here (unlike
    // before) must not unwind across the C ABI and abort the host.
    run_extern_callback_with::<P, ()>("au", "sysex_input", (), || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        if bytes.is_null() || len == 0 {
            return;
        }
        let slice = std::slice::from_raw_parts(bytes, len as usize);
        queue_sysex_input(&inst.sysex_free, &inst.sysex_ready, sample_offset, slice);
    });
}

pub(crate) unsafe extern "C" fn cb_au_push_sysex_input_native<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_offset: u32,
    bytes: *const u8,
    len: u32,
) -> u32 {
    run_extern_callback_with::<P, u32>("au", "sysex_input_native", 0, || unsafe {
        if ctx.is_null() || (bytes.is_null() && len != 0) {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let payload = if len == 0 {
            &[][..]
        } else {
            slice::from_raw_parts(bytes, len as usize)
        };
        u32::from(try_queue_sysex_input(
            &inst.sysex_free,
            &inst.sysex_ready,
            sample_offset,
            payload,
        ))
    })
}

pub(crate) unsafe extern "C" fn cb_output_sysex_count<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(audio) = inst.audio.try_enter() else {
            return 0;
        };
        len_u32(
            audio
                .output_events
                .iter()
                .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
                .count(),
        )
    }
}

pub(crate) unsafe extern "C" fn cb_output_sysex_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out_delta_frames: *mut u32,
    out_bytes: *mut *const u8,
    out_len: *mut u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut audio) = inst.audio.try_enter() else {
            return;
        };
        let scr = &mut *audio;
        if let Some(event) = nth_matching(
            &scr.output_events,
            index as usize,
            &mut scr.sysex_drain_cursor,
            |e| matches!(e.body, EventBody::SysEx { .. }).then_some(e),
        ) {
            let bytes = scr.output_events.sysex_bytes(&event.body);
            *out_delta_frames = event.sample_offset;
            *out_bytes = bytes.as_ptr();
            *out_len = len_u32(bytes.len());
        }
    }
}

pub(crate) enum NativeEncodeResult {
    Emitted(AuNativeEvent),
    Unsupported,
    Invalid,
}

pub(crate) fn typed_midi1_bytes(body: &EventBody) -> Option<([u8; 3], u32)> {
    let (bytes, len) = match *body {
        EventBody::NoteOn {
            group: 0,
            channel,
            note,
            velocity,
        } if channel < 16 && note < 128 && velocity < 128 => ([0x90 | channel, note, velocity], 3),
        EventBody::NoteOff {
            group: 0,
            channel,
            note,
            velocity,
        } if channel < 16 && note < 128 && velocity < 128 => ([0x80 | channel, note, velocity], 3),
        EventBody::ControlChange {
            group: 0,
            channel,
            cc,
            value,
        } if channel < 16 && cc < 128 && value < 128 => ([0xB0 | channel, cc, value], 3),
        EventBody::Aftertouch {
            group: 0,
            channel,
            note,
            pressure,
        } if channel < 16 && note < 128 && pressure < 128 => ([0xA0 | channel, note, pressure], 3),
        EventBody::ChannelPressure {
            group: 0,
            channel,
            pressure,
        } if channel < 16 && pressure < 128 => ([0xD0 | channel, pressure, 0], 2),
        EventBody::ProgramChange {
            group: 0,
            channel,
            program,
        } if channel < 16 && program < 128 => ([0xC0 | channel, program, 0], 2),
        EventBody::PitchBend {
            group: 0,
            channel,
            value,
        } if channel < 16 && value <= 0x3FFF => {
            let (lsb, msb) = pitch_bend_to_bytes(value);
            ([0xE0 | channel, lsb, msb], 3)
        }
        _ => return None,
    };
    Some((bytes, len))
}

pub(crate) fn midi1_message_len(status: u8) -> Option<usize> {
    if (0x80..=0xEF).contains(&status) {
        return Some(if matches!(status & 0xF0, 0xC0 | 0xD0) {
            2
        } else {
            3
        });
    }
    match status {
        0xF1 | 0xF3 => Some(2),
        0xF2 => Some(3),
        0xF6 | 0xF8 | 0xFA | 0xFB | 0xFC | 0xFE | 0xFF => Some(1),
        _ => None,
    }
}

pub(crate) fn raw_midi1_valid(message: RawMidi1) -> bool {
    let bytes = message.bytes();
    midi1_message_len(bytes[0]) == Some(bytes.len())
        && bytes.iter().skip(1).all(|byte| byte & 0x80 == 0)
}

pub(crate) fn output_port<P: PluginExport>(port: u16) -> Option<u16> {
    (port < u16::from(P::info().midi_output_ports)).then_some(port)
}

pub(crate) fn default_protocol<P: PluginExport>() -> u8 {
    if P::info().midi_output_dialect == MidiDialect::Midi2 {
        2
    } else {
        1
    }
}

pub(crate) fn raw_ump_protocol<P: PluginExport>(
    exact: moose_core::ExactEventRef<'_>,
) -> Result<u8, NativeEncodeResult> {
    let ExactEventBody::Ump { packet, .. } = exact.body() else {
        return Err(NativeEncodeResult::Invalid);
    };
    let message_type = ((packet.words()[0] >> 28) & 0x0f) as u8;
    if packet.word_count() != usize::from(ump_word_count_for_type(packet.words()[0])) {
        return Err(NativeEncodeResult::Invalid);
    }
    let protocol = match exact.metadata() {
        ExactEventMetadata::Au(metadata) => metadata.protocol(),
        ExactEventMetadata::None => match message_type {
            0x2 => 1,
            0x4 => 2,
            _ => default_protocol::<P>(),
        },
        _ => return Err(NativeEncodeResult::Unsupported),
    };
    if !ump_protocol_accepts(protocol, message_type) {
        return Err(NativeEncodeResult::Invalid);
    }
    Ok(protocol)
}

pub(crate) fn require_ump_carrier(
    carrier_mask: u32,
    host_protocol: u8,
) -> Result<(), NativeEncodeResult> {
    if carrier_mask & AU_NATIVE_CARRIER_UMP == 0 {
        Err(NativeEncodeResult::Unsupported)
    } else if matches!(host_protocol, 1 | 2) {
        Ok(())
    } else {
        Err(NativeEncodeResult::Invalid)
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn encode_native_output<P: PluginExport>(
    event: LosslessEventRef<'_>,
    list: &EventList,
    carrier_mask: u32,
    num_frames: u32,
    host_protocol: u8,
) -> NativeEncodeResult {
    let sample_offset = match event {
        LosslessEventRef::Typed(event) => event.sample_offset,
        LosslessEventRef::Exact(exact) => exact.sample_offset(),
    };
    if sample_offset >= num_frames {
        return NativeEncodeResult::Invalid;
    }

    match event {
        LosslessEventRef::Typed(event) => {
            let Some(port) = output_port::<P>(u16::from(event.port)) else {
                return NativeEncodeResult::Invalid;
            };
            if let Some((midi, data_len)) = typed_midi1_bytes(&event.body) {
                if carrier_mask & AU_NATIVE_CARRIER_BYTES != 0 {
                    return NativeEncodeResult::Emitted(AuNativeEvent {
                        sample_offset,
                        port,
                        kind: AU_NATIVE_EVENT_MIDI1,
                        data_len,
                        midi,
                        ..AuNativeEvent::default()
                    });
                }
                if let Err(status) = require_ump_carrier(carrier_mask, host_protocol) {
                    return status;
                }
                let Some(words) = encode_ump_channel_voice_1(&event.body) else {
                    return NativeEncodeResult::Invalid;
                };
                return NativeEncodeResult::Emitted(AuNativeEvent {
                    sample_offset,
                    port,
                    kind: AU_NATIVE_EVENT_UMP,
                    protocol: 1,
                    data_len: 1,
                    words,
                    ..AuNativeEvent::default()
                });
            }
            if event_to_midi1(&event.body).is_some() {
                return NativeEncodeResult::Invalid;
            }
            if let EventBody::SysEx { .. } = event.body {
                let Some(bytes) = list.sysex_bytes_checked(&event.body) else {
                    return NativeEncodeResult::Invalid;
                };
                if bytes.iter().any(|byte| byte & 0x80 != 0) {
                    return NativeEncodeResult::Invalid;
                }
                if carrier_mask & AU_NATIVE_CARRIER_BYTES == 0 {
                    return NativeEncodeResult::Unsupported;
                }
                return NativeEncodeResult::Emitted(AuNativeEvent {
                    sample_offset,
                    port,
                    kind: AU_NATIVE_EVENT_SYSEX,
                    data_len: len_u32(bytes.len()),
                    sysex: bytes.as_ptr(),
                    ..AuNativeEvent::default()
                });
            }
            if let Some(words) = encode_ump_channel_voice_2(&event.body) {
                if decode_ump_channel_voice_2(words).as_ref() != Some(&event.body) {
                    return NativeEncodeResult::Invalid;
                }
                if let Err(status) = require_ump_carrier(carrier_mask, host_protocol) {
                    return status;
                }
                return NativeEncodeResult::Emitted(AuNativeEvent {
                    sample_offset,
                    port,
                    kind: AU_NATIVE_EVENT_UMP,
                    protocol: 2,
                    data_len: 2,
                    words,
                    ..AuNativeEvent::default()
                });
            }
            NativeEncodeResult::Unsupported
        }
        LosslessEventRef::Exact(exact) => {
            if exact.qualifiers() != ExactEventQualifiers::default() {
                return NativeEncodeResult::Unsupported;
            }
            match *exact.body() {
                ExactEventBody::Midi1 { port, message } => {
                    if !exact.metadata().is_none() {
                        return NativeEncodeResult::Unsupported;
                    }
                    let Some(port) = output_port::<P>(port) else {
                        return NativeEncodeResult::Invalid;
                    };
                    if !raw_midi1_valid(message) {
                        return NativeEncodeResult::Invalid;
                    }
                    if carrier_mask & AU_NATIVE_CARRIER_BYTES == 0 {
                        return NativeEncodeResult::Unsupported;
                    }
                    NativeEncodeResult::Emitted(AuNativeEvent {
                        sample_offset,
                        port,
                        kind: AU_NATIVE_EVENT_MIDI1,
                        data_len: u32::try_from(message.len()).unwrap_or(0),
                        midi: *message.storage(),
                        ..AuNativeEvent::default()
                    })
                }
                ExactEventBody::SysEx { port } => {
                    if !exact.metadata().is_none() {
                        return NativeEncodeResult::Unsupported;
                    }
                    let Some(port) = output_port::<P>(port) else {
                        return NativeEncodeResult::Invalid;
                    };
                    let Some(bytes) = exact.sysex_bytes_checked() else {
                        return NativeEncodeResult::Invalid;
                    };
                    if bytes.iter().any(|byte| byte & 0x80 != 0) {
                        return NativeEncodeResult::Invalid;
                    }
                    if carrier_mask & AU_NATIVE_CARRIER_BYTES == 0 {
                        return NativeEncodeResult::Unsupported;
                    }
                    NativeEncodeResult::Emitted(AuNativeEvent {
                        sample_offset,
                        port,
                        kind: AU_NATIVE_EVENT_SYSEX,
                        data_len: len_u32(bytes.len()),
                        sysex: bytes.as_ptr(),
                        ..AuNativeEvent::default()
                    })
                }
                ExactEventBody::Ump { port, packet } => {
                    let Some(port) = output_port::<P>(port) else {
                        return NativeEncodeResult::Invalid;
                    };
                    if let Err(status) = require_ump_carrier(carrier_mask, host_protocol) {
                        return status;
                    }
                    let protocol = match raw_ump_protocol::<P>(exact) {
                        Ok(protocol) => protocol,
                        Err(status) => return status,
                    };
                    NativeEncodeResult::Emitted(AuNativeEvent {
                        sample_offset,
                        port,
                        kind: AU_NATIVE_EVENT_UMP,
                        protocol,
                        data_len: u32::try_from(packet.word_count()).unwrap_or(0),
                        words: *packet.storage(),
                        ..AuNativeEvent::default()
                    })
                }
                _ => NativeEncodeResult::Unsupported,
            }
        }
    }
}

pub(crate) fn encode_native_output_checked<P: PluginExport>(
    event: LosslessEventRef<'_>,
    list: &EventList,
    carrier_mask: u32,
    num_frames: u32,
    host_protocol: u8,
    max_absolute_offset: u32,
    ump_time_valid: bool,
) -> NativeEncodeResult {
    let sample_offset = match event {
        LosslessEventRef::Typed(event) => event.sample_offset,
        LosslessEventRef::Exact(exact) => exact.sample_offset(),
    };
    if sample_offset > max_absolute_offset {
        return NativeEncodeResult::Invalid;
    }
    match encode_native_output::<P>(event, list, carrier_mask, num_frames, host_protocol) {
        NativeEncodeResult::Emitted(event)
            if event.kind == AU_NATIVE_EVENT_UMP && !ump_time_valid =>
        {
            NativeEncodeResult::Invalid
        }
        status => status,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn begin_output_events<P: PluginExport>(
    inst: &AuInstance<P>,
    audio: &mut AuAudio<P>,
    carrier_mask: u32,
    num_frames: u32,
    ump_protocol: u32,
    max_absolute_offset: u32,
    ump_time_valid: u32,
) {
    audio.native_output_cursor = LosslessEventCursor::default();
    audio.native_output_carriers = carrier_mask & (AU_NATIVE_CARRIER_BYTES | AU_NATIVE_CARRIER_UMP);
    audio.native_output_num_frames = num_frames;
    audio.native_output_ump_protocol = u8::try_from(ump_protocol).unwrap_or(0);
    audio.native_output_max_absolute_offset = max_absolute_offset;
    audio.native_output_ump_time_valid = ump_time_valid != 0;
    if audio.output_events.overflow().is_some() {
        audio.native_output_status = AU_OUTPUT_QUEUE_FULL;
        return;
    }

    let mut param_count = 0usize;
    let status = audio.output_events.lossless_iter().find_map(|event| {
        if let LosslessEventRef::Typed(Event {
            sample_offset,
            body: EventBody::ParamChange { id, value },
            ..
        }) = event
        {
            if *sample_offset >= num_frames
                || !value.is_finite()
                || audio.catalog.by_id(*id).is_none()
            {
                return Some(AU_OUTPUT_INVALID);
            }
            if !f32::from_f64(*value).is_finite() {
                return Some(AU_OUTPUT_INVALID);
            }
            if !inst.host_sink.has_set_param() {
                return Some(AU_OUTPUT_UNSUPPORTED);
            }
            param_count += 1;
            return None;
        }
        match encode_native_output_checked::<P>(
            event,
            &audio.output_events,
            audio.native_output_carriers,
            num_frames,
            audio.native_output_ump_protocol,
            max_absolute_offset,
            audio.native_output_ump_time_valid,
        ) {
            NativeEncodeResult::Emitted(_) => None,
            NativeEncodeResult::Unsupported => Some(AU_OUTPUT_UNSUPPORTED),
            NativeEncodeResult::Invalid => Some(AU_OUTPUT_INVALID),
        }
    });
    audio.native_output_status = status.unwrap_or(AU_OUTPUT_END);

    if audio.native_output_status == AU_OUTPUT_END
        && param_count > 0
        && inst
            .param_out_queue
            .capacity()
            .saturating_sub(inst.param_out_queue.len())
            < param_count
    {
        audio.native_output_status = AU_OUTPUT_QUEUE_FULL;
    }
}

pub(crate) unsafe extern "C" fn cb_begin_output_events<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    carrier_mask: u32,
    num_frames: u32,
    ump_protocol: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut audio) = inst.audio.try_enter() else {
            return;
        };
        begin_output_events(
            inst,
            &mut audio,
            carrier_mask,
            num_frames,
            ump_protocol,
            u32::MAX,
            1,
        );
    }
}

pub(crate) unsafe extern "C" fn cb_begin_output_events_v9<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    carrier_mask: u32,
    num_frames: u32,
    ump_protocol: u32,
    max_absolute_offset: u32,
    ump_time_valid: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut audio) = inst.audio.try_enter() else {
            return;
        };
        begin_output_events(
            inst,
            &mut audio,
            carrier_mask,
            num_frames,
            ump_protocol,
            max_absolute_offset,
            ump_time_valid,
        );
    }
}

pub(crate) unsafe extern "C" fn cb_begin_output_events_v14<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    carrier_mask: u32,
    num_frames: u32,
    ump_protocol: u32,
    max_absolute_offset: u32,
    ump_time_valid: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut audio) = inst.audio.try_enter() else {
            return;
        };
        begin_output_events(
            inst,
            &mut audio,
            carrier_mask,
            num_frames,
            ump_protocol,
            max_absolute_offset,
            ump_time_valid,
        );
    }
}

pub(crate) unsafe extern "C" fn cb_next_output_event<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out: *mut AuNativeEvent,
) -> u32 {
    unsafe {
        if out.is_null() {
            return AU_OUTPUT_INVALID;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut audio) = inst.audio.try_enter() else {
            return AU_OUTPUT_END;
        };
        let scr = &mut *audio;
        if scr.native_output_status != AU_OUTPUT_END {
            return std::mem::replace(&mut scr.native_output_status, AU_OUTPUT_END);
        }
        let event = loop {
            let Some(event) = scr
                .output_events
                .lossless_next(&mut scr.native_output_cursor)
            else {
                return AU_OUTPUT_END;
            };
            if !matches!(
                event,
                LosslessEventRef::Typed(Event {
                    body: EventBody::ParamChange { .. },
                    ..
                })
            ) {
                break event;
            }
        };
        match encode_native_output_checked::<P>(
            event,
            &scr.output_events,
            scr.native_output_carriers,
            scr.native_output_num_frames,
            scr.native_output_ump_protocol,
            scr.native_output_max_absolute_offset,
            scr.native_output_ump_time_valid,
        ) {
            NativeEncodeResult::Emitted(event) => {
                out.write(event);
                AU_OUTPUT_EMITTED
            }
            NativeEncodeResult::Unsupported => AU_OUTPUT_UNSUPPORTED,
            NativeEncodeResult::Invalid => AU_OUTPUT_INVALID,
        }
    }
}

pub(crate) unsafe extern "C" fn cb_commit_output_params<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(audio) = inst.audio.try_enter() else {
            return AU_OUTPUT_END;
        };
        if audio.output_events.overflow().is_some() {
            return AU_OUTPUT_QUEUE_FULL;
        }

        let epoch = audio.feedback_epoch;
        let mut emitted = false;
        for event in audio.output_events.lossless_iter() {
            let LosslessEventRef::Typed(Event {
                body: EventBody::ParamChange { id, value },
                ..
            }) = event
            else {
                continue;
            };
            if !inst.host_sink.has_set_param() {
                return AU_OUTPUT_UNSUPPORTED;
            }
            // Check the epoch and update the store under one reservation.
            // Render never waits for an edit. Publish before notifying the
            // host so synchronous parameter getters see the committed value.
            if !inst.publish_feedback(*id, epoch, || inst.params_arc.set_plain(*id, *value)) {
                continue;
            }
            if inst
                .param_out_queue
                .push((*id, f32::from_f64(*value), epoch))
                .is_err()
            {
                return AU_OUTPUT_QUEUE_FULL;
            }
            emitted = true;
        }
        if emitted {
            if let Some(dispatch) = &inst.host_dispatch {
                dispatch.trigger_now();
            }
            AU_OUTPUT_EMITTED
        } else {
            AU_OUTPUT_END
        }
    }
}

pub(crate) unsafe extern "C" fn cb_finish_output_events<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    status: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(mut audio) = inst.audio.try_enter() else {
            return;
        };
        let status = audio.output_events.overflow().map_or_else(
            || match status {
                AU_OUTPUT_END | AU_OUTPUT_EMITTED => OutputEventStatus::Success,
                AU_OUTPUT_UNSUPPORTED => OutputEventStatus::Unsupported,
                AU_OUTPUT_QUEUE_FULL => OutputEventStatus::HostQueueFull,
                _ => OutputEventStatus::Invalid,
            },
            OutputEventStatus::BufferFull,
        );
        audio.output_events.set_output_status(status);
    }
}
