//! Input events and parameter changes from the host, converted to moose events.

use crate::instance::{Vst3Instance, Vst3Scratch};
use crate::midi_proxy::{allocated_midi_proxy_decode, midi_proxy_event};
use crate::output_events::midi_event_from_map;

use crate::ffi::Vst3NativeEvent;
use moose_core::events::{
    Event, EventBody, EventList, ExactEvent, ExactEventBody, ExactEventMetadata,
    ExactEventQualifiers, ExactNoteAddress, ExactNoteKind, PushError, Vst3EventMetadata,
};
use moose_core::export::PluginExport;
use moose_params::ParamRange;

pub(crate) const VST3_EVENT_NOTE_ON: u32 = 0;
pub(crate) const VST3_EVENT_NOTE_OFF: u32 = 1;
pub(crate) const VST3_EVENT_DATA: u32 = 2;
pub(crate) const VST3_EVENT_POLY_PRESSURE: u32 = 3;
pub(crate) const VST3_EVENT_NOTE_EXPRESSION: u32 = 4;
pub(crate) const VST3_EVENT_LEGACY_MIDI_CC_OUT: u32 = 65_535;

pub(crate) const VST3_EVENT_END: u32 = 0;
pub(crate) const VST3_EVENT_EMITTED: u32 = 1;
pub(crate) const VST3_EVENT_UNSUPPORTED: u32 = 2;
pub(crate) const VST3_EVENT_INVALID: u32 = 3;
pub(crate) const VST3_EVENT_QUEUE_FULL: u32 = 4;
/// The typed/exact pair was refused, but later exact-only events may still fit.
pub(crate) const VST3_EVENT_RESERVED: u32 = 5;
pub(crate) const VST3_EVENT_IS_LIVE: u32 = 1;

pub(crate) unsafe extern "C" fn cb_begin_input_events<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    num_frames: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut audio = inst.audio.enter();
        audio.event_list.clear();
        audio.input_num_frames = num_frames;
    }
}

pub(crate) unsafe extern "C" fn cb_push_input_event<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    event: *const Vst3NativeEvent,
) -> u32 {
    unsafe {
        if event.is_null() {
            return VST3_EVENT_INVALID;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut audio = inst.audio.enter();
        push_vst3_input_event::<P>(&mut audio, &*event)
    }
}

/// One host automation point, plain (denormalized) value. Returns 1 when
/// queued, 0 when the event list is full (the COM layer then commits the
/// queue's final value directly, so no change is lost).
pub(crate) unsafe extern "C" fn cb_push_param_change<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    sample_offset: i32,
    plain: f64,
    from_offset: i32,
    from_plain: f64,
) -> i32 {
    // SAFETY: ctx is a live instance; called on the audio thread inside
    // process(), like `cb_push_input_event`.
    let inst = unsafe { &*ctx.cast::<Vst3Instance<P>>() };
    let mut audio = inst.audio.enter();
    let Ok(sample_offset) = u32::try_from(sample_offset) else {
        return 0;
    };
    // A segment that starts after its end (a host sending points out of
    // order) degrades to a step at the point. So does a non-linear range:
    // the host's segment is linear in normalized space, and a plain-value
    // ramp would leave its curve.
    // ponytail: steps for non-linear ranges; carry normalized ramps if a
    // PerSample plugin with log params needs the curve.
    let linear = matches!(
        inst.param_ranges
            .binary_search_by_key(&id, |(i, _)| *i)
            .map(|at| inst.param_ranges[at].1),
        Ok(ParamRange::Linear { .. })
    );
    let ramp_from = u32::try_from(from_offset)
        .ok()
        .filter(|from| linear && *from <= sample_offset)
        .map(|from| (from, from_plain));
    i32::from(push_vst3_param_change::<P>(
        inst,
        &mut audio,
        id,
        sample_offset,
        plain,
        ramp_from,
    ))
}

/// Queue one sample-accurate parameter change. `set_plain` is deferred to
/// the chunker's per-sub-block apply pass so smoothers see `set_target` at
/// the event's sample rather than at the head of the audio block.
pub(crate) fn push_vst3_param_change<P: PluginExport>(
    inst: &Vst3Instance<P>,
    audio: &mut Vst3Scratch<P>,
    id: u32,
    sample_offset: u32,
    value: f64,
    ramp_from: Option<(u32, f64)>,
) -> bool {
    let list = &mut audio.event_list;
    // Unbound MIDI controllers arrive on the hidden proxy ids: decode
    // straight to the event. No `ParamChange` and no `Params` write - a
    // proxy is not a plugin parameter. Denormalize is identity for proxy
    // ids, so `value` is the host's raw `0..=1`. The id carries the event
    // bus it was mapped for, so multi-port plugins keep controllers per
    // port.
    if let Some((port, channel, controller)) = allocated_midi_proxy_decode(&inst.midi_proxy_ids, id)
    {
        // Proxies carry MIDI only, so they share the host input limit.
        // Letting them use automation headroom can starve real parameters.
        if input_event_lane_full(list) {
            return false;
        }
        #[allow(clippy::cast_possible_truncation)]
        let normalized = value.clamp(0.0, 1.0) as f32;
        return list
            .try_push(Event::on_port(
                sample_offset,
                port,
                midi_proxy_event(channel, controller, normalized),
            ))
            .is_ok();
    }
    // MIDI-mapped controllers (pitch bend, CC, pressure, program) arrive
    // as parameter changes because VST3 has no native input event for
    // them. Bridge them back into the MIDI event the plugin expects, in
    // addition to the plain `ParamChange` so the bound parameter still
    // tracks the controller. The bridged event is port 0: an explicit
    // `midi_map` binds one plugin parameter across every bus, so the host
    // delivers a bus-less change with no originating port to recover.
    let mapped = inst.midi_maps.binary_search_by_key(&id, |(id, _)| *id).ok();
    if list.len() + 1 + usize::from(mapped.is_some()) > list.capacity() {
        // Not silent: the plugin sees the overflow, and the COM layer
        // commits the queue's final value directly.
        list.record_overflow(PushError::EventFull);
        return false;
    }
    let point = param_point_event(inst.automation, id, sample_offset, value, ramp_from);
    // A `ParamRamp` starts at the previous point, before the bridged MIDI
    // event: push it first so the pair stays in offset order.
    let point_first = point.sample_offset < sample_offset;
    if point_first {
        list.push(point);
    }
    if let Some(idx) = mapped {
        list.push(Event {
            sample_offset,
            port: 0,
            body: midi_event_from_map(&inst.midi_maps[idx].1, value),
        });
    }
    if !point_first {
        list.push(point);
    }
    true
}

/// Host input events (notes, pressure, `SysEx`, MIDI proxies) may fill the typed event
/// lane only up to `capacity - capacity / VST3_AUTOMATION_RESERVE_DIVISOR`.
/// The rest is reserved for parameter automation, which the COM layer
/// interleaves with notes by sample offset. This reserves capacity, not
/// delivery: a mapped parameter point can need two typed slots.
pub(crate) const VST3_AUTOMATION_RESERVE_DIVISOR: usize = 4;

/// Typed-lane slots host input events may use in a list of `capacity`.
pub(crate) const fn input_event_typed_limit(capacity: usize) -> usize {
    capacity - capacity / VST3_AUTOMATION_RESERVE_DIVISOR
}

/// Whether one more typed host input event would eat into the automation
/// reserve. Records the overflow, so the refusal is never silent.
fn input_event_lane_full(list: &mut EventList) -> bool {
    if list.len() < input_event_typed_limit(list.capacity()) {
        return false;
    }
    list.record_overflow(PushError::EventFull);
    true
}

/// Classify a typed refusal by whether later exact-only events can fit.
/// Only exact-lane exhaustion stops host traversal after a typed refusal.
fn input_refusal_status(list: &EventList) -> u32 {
    if list.exact_len() < list.exact_capacity() {
        VST3_EVENT_RESERVED
    } else {
        VST3_EVENT_QUEUE_FULL
    }
}

/// Push an exact host event with its optional typed fallback, keeping the
/// typed lane's automation reserve. Refuse both halves atomically; never
/// deliver an exact note whose typed fallback was refused.
fn push_input_pair(
    list: &mut EventList,
    typed: Option<Event>,
    exact: ExactEvent,
) -> Result<(), PushError> {
    match typed {
        Some(_) if input_event_lane_full(list) => Err(PushError::EventFull),
        Some(typed) => list.try_push_with_exact(typed, exact),
        None => list.try_push_exact(exact),
    }
}

/// The event for one host queue point. `PerSample` plugins ramp
/// themselves, so they get the segment from the previous point as a linear
/// `ParamRamp`; `BlockSplit` plugins get the point, and the chunker splits
/// there.
pub(crate) fn param_point_event(
    automation: moose_core::Automation,
    id: u32,
    sample_offset: u32,
    value: f64,
    ramp_from: Option<(u32, f64)>,
) -> Event {
    match (automation, ramp_from) {
        (moose_core::Automation::PerSample, Some((from, start))) => Event {
            sample_offset: from,
            port: 0,
            body: EventBody::ParamRamp {
                id,
                start,
                end: value,
                len: sample_offset - from,
            },
        },
        _ => Event {
            sample_offset,
            port: 0,
            body: EventBody::ParamChange { id, value },
        },
    }
}

pub(crate) fn push_vst3_input_event<P: PluginExport>(
    audio: &mut Vst3Scratch<P>,
    event: &Vst3NativeEvent,
) -> u32 {
    let Ok(sample_offset) = u32::try_from(event.sample_offset) else {
        return VST3_EVENT_INVALID;
    };
    let Ok(port) = u8::try_from(event.bus_index) else {
        return VST3_EVENT_INVALID;
    };
    if sample_offset >= audio.input_num_frames || port >= P::info().midi_input_ports {
        return VST3_EVENT_INVALID;
    }
    let Some((qualifiers, metadata)) = vst3_exact_provenance(event) else {
        return VST3_EVENT_INVALID;
    };

    let result = match event.kind {
        VST3_EVENT_NOTE_ON | VST3_EVENT_NOTE_OFF => {
            let Some((channel, pitch)) = valid_note_axes(event.channel, event.pitch) else {
                return VST3_EVENT_INVALID;
            };
            if !valid_unit_f32(event.velocity) || !event.tuning.is_finite() {
                return VST3_EVENT_INVALID;
            }
            let (kind, length) = if event.kind == VST3_EVENT_NOTE_ON {
                (ExactNoteKind::On, Some(event.length))
            } else {
                (ExactNoteKind::Off, None)
            };
            let address = ExactNoteAddress::from_vst3_signed(
                event.bus_index,
                event.channel,
                event.pitch,
                event.note_id,
            );
            let exact = ExactEvent::new(
                sample_offset,
                ExactEventBody::DetailedNote {
                    kind,
                    address,
                    velocity: event.velocity,
                    tuning: event.tuning,
                    length,
                },
            )
            .with_qualifiers(qualifiers)
            .with_metadata(ExactEventMetadata::Vst3(metadata));
            let typed = faithful_typed_note(event, channel, pitch, kind)
                .map(|body| Event::on_port(sample_offset, port, body));
            push_input_pair(&mut audio.event_list, typed, exact)
        }
        VST3_EVENT_POLY_PRESSURE => {
            let Some((channel, pitch)) = valid_note_axes(event.channel, event.pitch) else {
                return VST3_EVENT_INVALID;
            };
            if !valid_unit_f32(event.velocity) {
                return VST3_EVENT_INVALID;
            }
            let address = ExactNoteAddress::from_vst3_signed(
                event.bus_index,
                event.channel,
                event.pitch,
                event.note_id,
            );
            let exact = ExactEvent::new(
                sample_offset,
                ExactEventBody::DetailedPolyPressure {
                    address,
                    pressure: event.velocity,
                },
            )
            .with_qualifiers(qualifiers)
            .with_metadata(ExactEventMetadata::Vst3(metadata));
            let typed = (event.note_id == -1)
                .then(|| normalized_u7_exact(event.velocity))
                .flatten()
                .map(|pressure| {
                    Event::on_port(
                        sample_offset,
                        port,
                        EventBody::Aftertouch {
                            group: 0,
                            channel,
                            note: pitch,
                            pressure,
                        },
                    )
                });
            push_input_pair(&mut audio.event_list, typed, exact)
        }
        VST3_EVENT_NOTE_EXPRESSION => {
            if !valid_unit_f64(event.value) || event.note_id == -1 {
                return VST3_EVENT_INVALID;
            }
            let exact = ExactEvent::new(
                sample_offset,
                ExactEventBody::NormalizedNoteExpression {
                    expression_id: event.type_id,
                    address: ExactNoteAddress::from_vst3_signed(
                        event.bus_index,
                        -1,
                        -1,
                        event.note_id,
                    ),
                    value: event.value,
                },
            )
            .with_qualifiers(qualifiers)
            .with_metadata(ExactEventMetadata::Vst3(metadata));
            audio.event_list.try_push_exact(exact)
        }
        VST3_EVENT_DATA => {
            if event.type_id != 0 || (event.len > 0 && event.bytes.is_null()) {
                return VST3_EVENT_INVALID;
            }
            if input_event_lane_full(&mut audio.event_list) {
                return input_refusal_status(&audio.event_list);
            }
            let bytes = if event.len == 0 {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(event.bytes, event.len as usize) }
            };
            let exact = ExactEvent::new(
                sample_offset,
                ExactEventBody::SysEx {
                    port: u16::from(port),
                },
            )
            .with_qualifiers(qualifiers)
            .with_metadata(ExactEventMetadata::Vst3(metadata));
            audio
                .event_list
                .try_push_sysex_with_exact_on_port(sample_offset, port, bytes, exact)
        }
        _ => return VST3_EVENT_UNSUPPORTED,
    };

    match result {
        Ok(()) => VST3_EVENT_EMITTED,
        Err(PushError::EventFull) => input_refusal_status(&audio.event_list),
        Err(PushError::ExactEventFull | PushError::VoiceTrackerFull | PushError::PoolFull) => {
            VST3_EVENT_QUEUE_FULL
        }
        Err(PushError::UnknownExactEvent) => VST3_EVENT_INVALID,
    }
}

pub(crate) fn vst3_exact_provenance(
    event: &Vst3NativeEvent,
) -> Option<(ExactEventQualifiers, Vst3EventMetadata)> {
    let raw_flags = u16::try_from(event.flags).ok()?;
    let metadata = Vst3EventMetadata::new(event.ppq_position, raw_flags)?;
    let qualifiers = ExactEventQualifiers {
        is_live: event.flags & VST3_EVENT_IS_LIVE != 0,
        dont_record: false,
    };
    Some((qualifiers, metadata))
}

pub(crate) fn valid_note_axes(channel: i16, pitch: i16) -> Option<(u8, u8)> {
    let channel = u8::try_from(channel).ok().filter(|value| *value < 16)?;
    let pitch = u8::try_from(pitch).ok().filter(|value| *value < 128)?;
    Some((channel, pitch))
}

pub(crate) fn valid_unit_f32(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

pub(crate) fn valid_unit_f64(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

#[allow(clippy::float_cmp)]
pub(crate) fn normalized_u7_exact(value: f32) -> Option<u8> {
    let scaled = value * 127.0;
    let rounded = scaled.round();
    if rounded != scaled {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let wire = rounded as u8;
    (f32::from(wire) / 127.0 == value).then_some(wire)
}

pub(crate) fn faithful_typed_note(
    event: &Vst3NativeEvent,
    channel: u8,
    pitch: u8,
    kind: ExactNoteKind,
) -> Option<EventBody> {
    if event.note_id != -1 || event.tuning != 0.0 || event.length != 0 {
        return None;
    }
    let velocity = normalized_u7_exact(event.velocity)?;
    match kind {
        ExactNoteKind::On if velocity > 0 => Some(EventBody::NoteOn {
            group: 0,
            channel,
            note: pitch,
            velocity,
        }),
        ExactNoteKind::Off => Some(EventBody::NoteOff {
            group: 0,
            channel,
            note: pitch,
            velocity,
        }),
        _ => None,
    }
}
