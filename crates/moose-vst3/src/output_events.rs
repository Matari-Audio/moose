//! Output events from the plugin to the host: notes, MIDI, expressions and output parameters.

use crate::input_events::{
    VST3_EVENT_DATA, VST3_EVENT_EMITTED, VST3_EVENT_END, VST3_EVENT_INVALID, VST3_EVENT_IS_LIVE,
    VST3_EVENT_LEGACY_MIDI_CC_OUT, VST3_EVENT_NOTE_EXPRESSION, VST3_EVENT_NOTE_OFF,
    VST3_EVENT_NOTE_ON, VST3_EVENT_POLY_PRESSURE, VST3_EVENT_QUEUE_FULL, VST3_EVENT_UNSUPPORTED,
    valid_unit_f32, valid_unit_f64,
};
use crate::instance::{OutputNoteIds, OutputSourceIdentity, PendingOutputMutation, Vst3Instance};
#[cfg(test)]
use moose_core::midi::{downconvert_to_midi1, per_note_bend_from_semitones};

use crate::ffi::Vst3NativeEvent;
#[cfg(test)]
use crate::instance::Vst3MidiEvent;
use moose_core::cast::len_u32;
use moose_core::events::{
    Event, EventBody, EventList, ExactAddress, ExactEventBody, ExactEventMetadata,
    ExactNoteAddress, ExactNoteKind, LosslessEventCursor, LosslessEventRef, OutputEventStatus,
};
use moose_core::export::PluginExport;
use moose_core::midi::{
    denorm_7bit, denorm_pitch_bend, per_note_bend_semitones, pitch_bend_to_bytes,
};
use moose_params::{MidiSource, ParamInfo, ParamRange};

// ---------------------------------------------------------------------------
// Output event callbacks
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) fn try_encode_vst3_midi(event: &Event) -> Option<Vst3MidiEvent> {
    let body = match event.body {
        body if note_expression_of(&body).is_some() => return None,
        other => downconvert_to_midi1(&other).unwrap_or(other),
    };
    let (status, data1, data2) = match body {
        EventBody::NoteOn {
            channel,
            note,
            velocity,
            ..
        } => (0x90 | (channel & 0x0F), note, velocity),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
            ..
        } => (0x80 | (channel & 0x0F), note, velocity),
        EventBody::ControlChange {
            channel, cc, value, ..
        } => (0xB0 | (channel & 0x0F), cc, value),
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
            ..
        } => (0xA0 | (channel & 0x0F), note, pressure),
        EventBody::ChannelPressure {
            channel, pressure, ..
        } => (0xD0 | (channel & 0x0F), pressure, 0),
        EventBody::PitchBend { channel, value, .. } => {
            let (lsb, msb) = pitch_bend_to_bytes(value);
            (0xE0 | (channel & 0x0F), lsb, msb)
        }
        EventBody::ProgramChange {
            channel, program, ..
        } => (0xC0 | (channel & 0x0F), program, 0),
        _ => return None,
    };
    Some(Vst3MidiEvent {
        sample_offset: event.sample_offset,
        status,
        data1,
        data2,
        port: event.port,
        note_id: -1,
        ne_value: 0.0,
    })
}

/// VST3 has no native CC / pitch-bend / channel-pressure / program
/// input event. Hosts route those MIDI messages to a parameter the
/// plugin advertises through `IMidiMapping` (see
/// `cb_midi_mapping_get_param_id`) and deliver them as parameter
/// changes. When `info` carries such a binding, turn the parameter
/// change back into the MIDI event the plugin expects, so event-based
/// plugins behave the same here as on AU / CLAP / LV2 (which hand the
/// plugin raw MIDI). Returns `None` for unmapped parameters - the
/// caller still emits the plain `ParamChange`.
///
/// `plain` is the denormalized value the shim already produced; we
/// re-normalize through the parameter's range so the MIDI-domain
/// mapping is independent of how the binding parameter declares its
/// range.
//
// `norm as f32` is a lossless-enough narrowing of a clamped `0..=1`
// value; the MIDI encoders take `f32`.
#[allow(clippy::cast_possible_truncation)]
/// A parameter's precomputed MIDI-controller binding. Built once per
/// instance for every param that declares a `midi_map`, so the audio
/// thread can bridge a mapped controller change to its `EventBody`
/// through a binary search instead of a linear `ParamInfo` scan.
#[derive(Clone, Copy)]
pub(crate) struct MidiMap {
    pub(crate) source: MidiSource,
    pub(crate) channel: u8,
    pub(crate) range: ParamRange,
}

impl MidiMap {
    /// The binding `info` declares, or `None` when it has no `midi_map`.
    pub(crate) fn from_param(info: &ParamInfo) -> Option<Self> {
        Some(Self {
            source: info.midi_map?,
            channel: info.midi_channel.unwrap_or(0),
            range: info.range,
        })
    }
}

/// Bridge a MIDI-mapped parameter change back into the `EventBody` the
/// plugin expects. VST3 has no native input event for channel MIDI, so
/// the host delivers it as a parameter change on the mapped id.
// `normalize` yields a `0.0..=1.0` value; the MIDI encoders take `f32`.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn midi_event_from_map(map: &MidiMap, plain: f64) -> EventBody {
    let channel = map.channel;
    let norm = map.range.normalize(plain) as f32; // 0.0..=1.0
    match map.source {
        // Host-normalized `0..1` is the pitch-wheel position (0 = full
        // down, 0.5 = center, 1 = full up); shift to `[-1, 1]` for the
        // 14-bit encoder.
        MidiSource::PitchBend => EventBody::PitchBend {
            group: 0,
            channel,
            value: denorm_pitch_bend(norm * 2.0 - 1.0),
        },
        MidiSource::Cc(cc) => EventBody::ControlChange {
            group: 0,
            channel,
            cc,
            value: denorm_7bit(norm),
        },
        MidiSource::ChannelPressure => EventBody::ChannelPressure {
            group: 0,
            channel,
            pressure: denorm_7bit(norm),
        },
        MidiSource::ProgramChange => EventBody::ProgramChange {
            group: 0,
            channel,
            program: denorm_7bit(norm),
        },
    }
}

pub(crate) enum Vst3EncodeResult {
    Emitted(Vst3NativeEvent, PendingOutputMutation),
    Unsupported,
    Invalid,
}

pub(crate) unsafe extern "C" fn cb_begin_output_events<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut audio = inst.audio.enter();
        audio.output_cursor = LosslessEventCursor::default();
        audio.output_preflight_status = if audio.output_events.overflow().is_some() {
            VST3_EVENT_QUEUE_FULL
        } else {
            let mut note_ids = audio.output_note_ids;
            audio
                .output_events
                .lossless_iter()
                .find_map(|event| {
                    if let LosslessEventRef::Typed(Event {
                        sample_offset,
                        body: EventBody::ParamChange { id, value },
                        ..
                    }) = event
                    {
                        let valid = *sample_offset < audio.input_num_frames
                            && value.is_finite()
                            && inst.param_infos.by_id(*id).is_some();
                        return (!valid).then_some(VST3_EVENT_INVALID);
                    }
                    match encode_vst3_output_event::<P>(
                        event,
                        &audio.output_events,
                        &note_ids,
                        audio.input_num_frames,
                    ) {
                        Vst3EncodeResult::Emitted(_, mutation) => {
                            note_ids.commit(mutation);
                            None
                        }
                        Vst3EncodeResult::Unsupported => Some(VST3_EVENT_UNSUPPORTED),
                        Vst3EncodeResult::Invalid => Some(VST3_EVENT_INVALID),
                    }
                })
                .unwrap_or(VST3_EVENT_END)
        };
        audio.pending_output_mutation = PendingOutputMutation::None;
    }
}

pub(crate) unsafe extern "C" fn cb_next_output_event<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out: *mut Vst3NativeEvent,
) -> u32 {
    unsafe {
        if out.is_null() {
            return VST3_EVENT_INVALID;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut audio = inst.audio.enter();
        audio.pending_output_mutation = PendingOutputMutation::None;
        let scr = &mut *audio;
        if scr.output_preflight_status != VST3_EVENT_END {
            return std::mem::replace(&mut scr.output_preflight_status, VST3_EVENT_END);
        }
        let event = loop {
            let Some(event) = scr.output_events.lossless_next(&mut scr.output_cursor) else {
                return VST3_EVENT_END;
            };
            if matches!(
                event,
                LosslessEventRef::Typed(Event {
                    body: EventBody::ParamChange { .. },
                    ..
                })
            ) {
                continue;
            }
            break event;
        };
        match encode_vst3_output_event::<P>(
            event,
            &scr.output_events,
            &scr.output_note_ids,
            scr.input_num_frames,
        ) {
            Vst3EncodeResult::Emitted(native, mutation) => {
                scr.pending_output_mutation = mutation;
                out.write(native);
                VST3_EVENT_EMITTED
            }
            Vst3EncodeResult::Unsupported => VST3_EVENT_UNSUPPORTED,
            Vst3EncodeResult::Invalid => VST3_EVENT_INVALID,
        }
    }
}

pub(crate) unsafe extern "C" fn cb_finish_output_events<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    status: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut audio = inst.audio.enter();
        let status = audio.output_events.overflow().map_or_else(
            || match status {
                VST3_EVENT_END | VST3_EVENT_EMITTED => OutputEventStatus::Success,
                VST3_EVENT_UNSUPPORTED => OutputEventStatus::Unsupported,
                VST3_EVENT_QUEUE_FULL => OutputEventStatus::HostQueueFull,
                _ => OutputEventStatus::Invalid,
            },
            OutputEventStatus::BufferFull,
        );
        audio.output_events.set_output_status(status);
    }
}

pub(crate) unsafe extern "C" fn cb_commit_output_event<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut audio = inst.audio.enter();
        let mutation = std::mem::replace(
            &mut audio.pending_output_mutation,
            PendingOutputMutation::None,
        );
        audio.output_note_ids.commit(mutation);
    }
}

pub(crate) fn encode_vst3_output_event<P: PluginExport>(
    event: LosslessEventRef<'_>,
    events: &EventList,
    note_ids: &OutputNoteIds,
    num_frames: u32,
) -> Vst3EncodeResult {
    let sample_offset = match event {
        LosslessEventRef::Typed(event) => event.sample_offset,
        LosslessEventRef::Exact(event) => event.sample_offset(),
    };
    if sample_offset >= num_frames {
        return Vst3EncodeResult::Invalid;
    }

    match event {
        LosslessEventRef::Typed(event) => encode_typed_vst3_output::<P>(event, events, note_ids),
        LosslessEventRef::Exact(event) => encode_exact_vst3_output::<P>(event, note_ids),
    }
}

#[allow(clippy::float_cmp)]
pub(crate) fn encode_exact_vst3_output<P: PluginExport>(
    exact: moose_core::ExactEventRef<'_>,
    note_ids: &OutputNoteIds,
) -> Vst3EncodeResult {
    let (flags, ppq_position) = match exact.metadata() {
        ExactEventMetadata::None => (
            u32::from(exact.qualifiers().is_live) * VST3_EVENT_IS_LIVE,
            0.0,
        ),
        ExactEventMetadata::Vst3(metadata) if metadata.ppq_position().is_finite() => {
            (u32::from(metadata.raw_flags()), metadata.ppq_position())
        }
        ExactEventMetadata::Vst3(_) => return Vst3EncodeResult::Invalid,
        _ => return Vst3EncodeResult::Unsupported,
    };
    let encoded = match *exact.body() {
        ExactEventBody::DetailedNote {
            kind,
            address,
            velocity,
            tuning,
            length,
        } => encode_detailed_note::<P>(
            exact.sample_offset(),
            flags,
            kind,
            address,
            velocity,
            tuning,
            length,
            note_ids,
        ),
        ExactEventBody::Note {
            kind,
            address,
            velocity,
        } => {
            #[allow(clippy::cast_possible_truncation)]
            let native_velocity = velocity as f32;
            if f64::from(native_velocity) != velocity {
                return Vst3EncodeResult::Invalid;
            }
            encode_detailed_note::<P>(
                exact.sample_offset(),
                flags,
                kind,
                address,
                native_velocity,
                0.0,
                (kind == ExactNoteKind::On).then_some(0),
                note_ids,
            )
        }
        ExactEventBody::DetailedPolyPressure { address, pressure } => {
            let Some((bus, channel, pitch, source)) = concrete_output_address::<P>(address) else {
                return Vst3EncodeResult::Invalid;
            };
            if !valid_unit_f32(pressure) {
                return Vst3EncodeResult::Invalid;
            }
            let note_id = match source {
                OutputSourceIdentity::Exact { .. } => {
                    let Some(note_id) =
                        note_ids.note_id_for_expression(source, bus, channel, pitch)
                    else {
                        return Vst3EncodeResult::Unsupported;
                    };
                    note_id
                }
                OutputSourceIdentity::Anonymous => -1,
            };
            Vst3EncodeResult::Emitted(
                Vst3NativeEvent {
                    kind: VST3_EVENT_POLY_PRESSURE,
                    sample_offset: i32::try_from(exact.sample_offset()).unwrap_or(i32::MAX),
                    bus_index: i32::from(bus),
                    flags,
                    channel: i16::from(channel),
                    pitch: i16::from(pitch),
                    note_id,
                    velocity: pressure,
                    ..Vst3NativeEvent::default()
                },
                PendingOutputMutation::None,
            )
        }
        ExactEventBody::NormalizedNoteExpression {
            expression_id,
            address,
            value,
        } => {
            if !valid_unit_f64(value) {
                return Vst3EncodeResult::Invalid;
            }
            let Some((bus, source)) = output_expression_address::<P>(address) else {
                return Vst3EncodeResult::Invalid;
            };
            let Some(note_id) = note_ids.note_id_for_expression(source, bus, 0, 0) else {
                return Vst3EncodeResult::Unsupported;
            };
            Vst3EncodeResult::Emitted(
                Vst3NativeEvent {
                    kind: VST3_EVENT_NOTE_EXPRESSION,
                    sample_offset: i32::try_from(exact.sample_offset()).unwrap_or(i32::MAX),
                    bus_index: i32::from(bus),
                    flags,
                    note_id,
                    type_id: expression_id,
                    value,
                    ..Vst3NativeEvent::default()
                },
                PendingOutputMutation::None,
            )
        }
        ExactEventBody::SysEx { port } => {
            let Ok(bus) = u8::try_from(port) else {
                return Vst3EncodeResult::Invalid;
            };
            if bus >= P::info().midi_output_ports {
                return Vst3EncodeResult::Invalid;
            }
            let Some(bytes) = exact.sysex_bytes_checked() else {
                return Vst3EncodeResult::Invalid;
            };
            Vst3EncodeResult::Emitted(
                sysex_native_event(exact.sample_offset(), bus, flags, bytes),
                PendingOutputMutation::None,
            )
        }
        // Raw MIDI 1.0 and UMP, legacy note expression, and every exact
        // variant without a native VST3 representation stay unsupported.
        _ => Vst3EncodeResult::Unsupported,
    };
    match encoded {
        Vst3EncodeResult::Emitted(mut native, mutation) => {
            native.flags = flags;
            native.ppq_position = ppq_position;
            Vst3EncodeResult::Emitted(native, mutation)
        }
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_detailed_note<P: PluginExport>(
    sample_offset: u32,
    flags: u32,
    kind: ExactNoteKind,
    address: ExactNoteAddress,
    velocity: f32,
    tuning: f32,
    length: Option<i32>,
    note_ids: &OutputNoteIds,
) -> Vst3EncodeResult {
    let Some((bus, channel, pitch, source)) = concrete_output_address::<P>(address) else {
        return Vst3EncodeResult::Invalid;
    };
    if !valid_unit_f32(velocity) || !tuning.is_finite() {
        return Vst3EncodeResult::Invalid;
    }
    let (kind, length, note_id, mutation) = match kind {
        ExactNoteKind::On => {
            let Some(length) = length else {
                return Vst3EncodeResult::Invalid;
            };
            let Some((note_id, mutation)) = note_ids.propose_note_on(source, bus, channel, pitch)
            else {
                return Vst3EncodeResult::Unsupported;
            };
            (VST3_EVENT_NOTE_ON, length, note_id, mutation)
        }
        ExactNoteKind::Off => {
            if length.is_some() {
                return Vst3EncodeResult::Invalid;
            }
            let Some((note_id, mutation)) = note_ids.propose_note_off(source, bus, channel, pitch)
            else {
                return Vst3EncodeResult::Unsupported;
            };
            (VST3_EVENT_NOTE_OFF, 0, note_id, mutation)
        }
        // Choke, End, and future note kinds have no native VST3 event.
        _ => return Vst3EncodeResult::Unsupported,
    };
    Vst3EncodeResult::Emitted(
        Vst3NativeEvent {
            kind,
            sample_offset: i32::try_from(sample_offset).unwrap_or(i32::MAX),
            bus_index: i32::from(bus),
            flags,
            channel: i16::from(channel),
            pitch: i16::from(pitch),
            note_id,
            length,
            velocity,
            tuning,
            ..Vst3NativeEvent::default()
        },
        mutation,
    )
}

pub(crate) fn concrete_output_address<P: PluginExport>(
    address: ExactNoteAddress,
) -> Option<(u8, u8, u8, OutputSourceIdentity)> {
    let bus = u8::try_from(address.port.value()?).ok()?;
    if bus >= P::info().midi_output_ports {
        return None;
    }
    let channel = address.channel.value()?;
    let pitch = address.key.value()?;
    if channel >= 16 || pitch >= 128 {
        return None;
    }
    let source = match address.note_id {
        ExactAddress::Wildcard => OutputSourceIdentity::Anonymous,
        ExactAddress::Value(note_id) => OutputSourceIdentity::Exact {
            bus: u16::from(bus),
            note_id,
        },
        ExactAddress::InvalidRaw(_) => return None,
    };
    Some((bus, channel, pitch, source))
}

pub(crate) fn output_expression_address<P: PluginExport>(
    address: ExactNoteAddress,
) -> Option<(u8, OutputSourceIdentity)> {
    let bus = u8::try_from(address.port.value()?).ok()?;
    if bus >= P::info().midi_output_ports {
        return None;
    }
    let note_id = address.note_id.value()?;
    Some((
        bus,
        OutputSourceIdentity::Exact {
            bus: u16::from(bus),
            note_id,
        },
    ))
}

pub(crate) fn encode_typed_vst3_output<P: PluginExport>(
    event: &Event,
    events: &EventList,
    note_ids: &OutputNoteIds,
) -> Vst3EncodeResult {
    if event.port >= P::info().midi_output_ports {
        return Vst3EncodeResult::Invalid;
    }
    let bus = event.port;
    let mut native = Vst3NativeEvent {
        sample_offset: i32::try_from(event.sample_offset).unwrap_or(i32::MAX),
        bus_index: i32::from(bus),
        ..Vst3NativeEvent::default()
    };
    let mutation = match event.body {
        EventBody::NoteOn {
            group: 0,
            channel,
            note,
            velocity,
        } if channel < 16 && note < 128 && velocity < 128 => {
            let Some((note_id, mutation)) =
                note_ids.propose_note_on(OutputSourceIdentity::Anonymous, bus, channel, note)
            else {
                return Vst3EncodeResult::Unsupported;
            };
            native.kind = VST3_EVENT_NOTE_ON;
            native.channel = i16::from(channel);
            native.pitch = i16::from(note);
            native.note_id = note_id;
            native.velocity = f32::from(velocity) / 127.0;
            mutation
        }
        EventBody::NoteOff {
            group: 0,
            channel,
            note,
            velocity,
        } if channel < 16 && note < 128 && velocity < 128 => {
            let Some((note_id, mutation)) =
                note_ids.propose_note_off(OutputSourceIdentity::Anonymous, bus, channel, note)
            else {
                return Vst3EncodeResult::Unsupported;
            };
            native.kind = VST3_EVENT_NOTE_OFF;
            native.channel = i16::from(channel);
            native.pitch = i16::from(note);
            native.note_id = note_id;
            native.velocity = f32::from(velocity) / 127.0;
            mutation
        }
        EventBody::Aftertouch {
            group: 0,
            channel,
            note,
            pressure,
        } if channel < 16 && note < 128 && pressure < 128 => {
            native.kind = VST3_EVENT_POLY_PRESSURE;
            native.channel = i16::from(channel);
            native.pitch = i16::from(note);
            native.note_id = note_ids
                .note_id_for_expression(OutputSourceIdentity::Anonymous, bus, channel, note)
                .unwrap_or(-1);
            native.velocity = f32::from(pressure) / 127.0;
            PendingOutputMutation::None
        }
        EventBody::PerNotePitchBend {
            group: 0,
            channel,
            note,
            value,
        } if channel < 16 && note < 128 => {
            let Some(note_id) = note_ids.note_id_for_expression(
                OutputSourceIdentity::Anonymous,
                bus,
                channel,
                note,
            ) else {
                return Vst3EncodeResult::Unsupported;
            };
            native.kind = VST3_EVENT_NOTE_EXPRESSION;
            native.note_id = note_id;
            native.type_id = 2;
            native.value = wire_to_vst3_tuning(value);
            PendingOutputMutation::None
        }
        EventBody::ControlChange {
            group: 0,
            channel,
            cc,
            value,
        } if channel < 16 && cc < 128 && value < 128 => {
            native.kind = VST3_EVENT_LEGACY_MIDI_CC_OUT;
            native.channel = i16::from(channel);
            native.type_id = u32::from(cc);
            native.data1 = value;
            PendingOutputMutation::None
        }
        EventBody::ChannelPressure {
            group: 0,
            channel,
            pressure,
        } if channel < 16 && pressure < 128 => {
            native.kind = VST3_EVENT_LEGACY_MIDI_CC_OUT;
            native.channel = i16::from(channel);
            native.type_id = 128;
            native.data1 = pressure;
            PendingOutputMutation::None
        }
        EventBody::PitchBend {
            group: 0,
            channel,
            value,
        } if channel < 16 && value < 16_384 => {
            let (lsb, msb) = pitch_bend_to_bytes(value);
            native.kind = VST3_EVENT_LEGACY_MIDI_CC_OUT;
            native.channel = i16::from(channel);
            native.type_id = 129;
            native.data1 = lsb;
            native.data2 = msb;
            PendingOutputMutation::None
        }
        EventBody::ProgramChange {
            group: 0,
            channel,
            program,
        } if channel < 16 && program < 128 => {
            native.kind = VST3_EVENT_LEGACY_MIDI_CC_OUT;
            native.channel = i16::from(channel);
            native.type_id = 130;
            native.data1 = program;
            PendingOutputMutation::None
        }
        EventBody::SysEx { .. } => {
            let Some(bytes) = events.sysex_bytes_checked(&event.body) else {
                return Vst3EncodeResult::Invalid;
            };
            return Vst3EncodeResult::Emitted(
                sysex_native_event(event.sample_offset, bus, 0, bytes),
                PendingOutputMutation::None,
            );
        }
        _ => return Vst3EncodeResult::Unsupported,
    };
    Vst3EncodeResult::Emitted(native, mutation)
}

pub(crate) fn sysex_native_event(
    sample_offset: u32,
    bus: u8,
    flags: u32,
    bytes: &[u8],
) -> Vst3NativeEvent {
    Vst3NativeEvent {
        kind: VST3_EVENT_DATA,
        sample_offset: i32::try_from(sample_offset).unwrap_or(i32::MAX),
        bus_index: i32::from(bus),
        flags,
        bytes: bytes.as_ptr(),
        len: len_u32(bytes.len()),
        ..Vst3NativeEvent::default()
    }
}

/// Map a moose per-note MIDI 2.0 event to a VST3 note-expression tuple
/// `(type_id, note_id, value)`. VST3 has no UMP; per-note richness rides
/// `INoteExpressionController` value events keyed by `note_id`. We key
/// `note_id` deterministically as `(channel << 7) | note` - the shim
/// stamps the plugin's `NoteOn` with the same id, so notes and their
/// expression correlate without any per-instance tracking state. Returns
/// `None` for per-note controllers VST3 has no predefined type for; the
/// value is normalized `0..=1` (VST3's `NoteExpressionValue` domain).
#[cfg(test)]
pub(crate) fn note_expression_of(body: &EventBody) -> Option<(u32, i32, f64)> {
    // Predefined VST3 NoteExpressionTypeIDs (reverse of the input map):
    // Volume=0, Pan=1, Tuning=2, Vibrato=3, Expression=4, Brightness=5.
    let (type_id, channel, note, value) = match *body {
        // Registered per-note controllers only: the predefined VST3
        // expression types carry the registered indices' semantics;
        // an assignable index is manufacturer-defined and must not
        // alias onto them.
        EventBody::PerNoteCC {
            channel,
            note,
            cc,
            value,
            registered: true,
            ..
        } => {
            let type_id = match cc {
                7 => 0,
                10 => 1,
                1 => 3,
                11 => 4,
                74 => 5,
                _ => return None,
            };
            (type_id, channel, note, u32_to_unit(value))
        }
        EventBody::PerNotePitchBend {
            channel,
            note,
            value,
            ..
        } => (2, channel, note, wire_to_vst3_tuning(value)),
        _ => return None,
    };
    Some((type_id, vst3_note_id(channel, note), value))
}

/// Deterministic VST3 `noteId` for a moose note: `(channel << 7) | note`.
/// The C++ shim stamps every emitted note-on/off with the same formula,
/// so a plug-in's note-expression events address the live note without
/// any shared correlation state.
#[cfg(test)]
pub(crate) fn vst3_note_id(channel: u8, note: u8) -> i32 {
    (i32::from(channel & 0x0F) << 7) | i32::from(note & 0x7F)
}

/// Normalize a wire-native 32-bit per-note value into VST3's `0..=1`
/// `NoteExpressionValue` domain.
#[cfg(test)]
pub(crate) fn u32_to_unit(v: u32) -> f64 {
    f64::from(v) / f64::from(u32::MAX)
}

/// VST3's tuning note-expression span: normalized `0..=1` covers
/// `-120..=+120` semitones (`plain = 240 * (norm - 0.5)` per the SDK).
pub(crate) const VST3_TUNING_SPAN_SEMITONES: f64 = 240.0;

/// VST3 tuning norm (`0..=1`, ±120 st) -> wire per-note bend. The wire
/// full-scale is ±48 st, so a wider host bend saturates.
#[cfg(test)]
pub(crate) fn vst3_tuning_to_wire(norm: f64) -> u32 {
    per_note_bend_from_semitones((norm - 0.5) * VST3_TUNING_SPAN_SEMITONES)
}

/// Wire per-note bend (±48 st full-scale) -> VST3 tuning norm
/// (`0..=1`, ±120 st), so the same event bends identically on every
/// semitone-denominated host domain.
pub(crate) fn wire_to_vst3_tuning(v: u32) -> f64 {
    0.5 + per_note_bend_semitones(v) / VST3_TUNING_SPAN_SEMITONES
}

/// Inverse of [`u32_to_unit`]: widen a VST3 `NoteExpressionValue` into
/// the wire-native 32-bit per-note domain. Hosts are supposed to stay
/// in `0..=1`, but the value crosses an FFI boundary - clamp first.
#[cfg(test)]
pub(crate) fn unit_to_u32(v: f64) -> u32 {
    // Clamped to `0..=u32::MAX` before the cast, so no truncation or
    // sign loss is possible.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled = (v.clamp(0.0, 1.0) * f64::from(u32::MAX)).round() as u32;
    scaled
}

pub(crate) unsafe extern "C" fn cb_get_output_param_count<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let audio = inst.audio.enter();
        len_u32(
            audio
                .output_events
                .iter()
                .filter(|e| matches!(e.body, EventBody::ParamChange { .. }))
                .count(),
        )
    }
}

pub(crate) unsafe extern "C" fn cb_get_output_param<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out_id: *mut u32,
    out_sample_offset: *mut i32,
    out_value: *mut f64,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let audio = inst.audio.enter();
        if let Some(event) = audio
            .output_events
            .iter()
            .filter(|e| matches!(e.body, EventBody::ParamChange { .. }))
            .nth(index as usize)
            && let EventBody::ParamChange { id, value } = event.body
        {
            // VST3 output param queues carry normalized values; the
            // plugin emits plain. Fall back to the plain value if the
            // id has no descriptor (shouldn't happen for real params).
            let normalized = inst
                .param_infos
                .iter()
                .find(|i| i.id == id)
                .map_or(value, |i| i.range.normalize(value));
            *out_id = id;
            *out_sample_offset = i32::try_from(event.sample_offset).unwrap_or(0);
            *out_value = normalized;
        }
    }
}
