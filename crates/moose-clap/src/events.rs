//! CLAP input events to `EventList`, transport conversion, and output events.

use crate::data::ClapAudio;

use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_DONT_RECORD, CLAP_EVENT_IS_LIVE, CLAP_EVENT_MIDI,
    CLAP_EVENT_MIDI_SYSEX, CLAP_EVENT_MIDI2, CLAP_EVENT_NOTE_CHOKE, CLAP_EVENT_NOTE_END,
    CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_TRANSPORT, CLAP_NOTE_EXPRESSION_BRIGHTNESS,
    CLAP_NOTE_EXPRESSION_EXPRESSION, CLAP_NOTE_EXPRESSION_PAN, CLAP_NOTE_EXPRESSION_PRESSURE,
    CLAP_NOTE_EXPRESSION_TUNING, CLAP_NOTE_EXPRESSION_VIBRATO, CLAP_NOTE_EXPRESSION_VOLUME,
    CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
    CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE,
    CLAP_TRANSPORT_IS_PLAYING, CLAP_TRANSPORT_IS_RECORDING, clap_event_header, clap_event_midi,
    clap_event_midi_sysex, clap_event_midi2, clap_event_note, clap_event_note_expression,
    clap_event_param_mod, clap_event_param_value, clap_event_transport, clap_input_events,
    clap_output_events,
};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::id::CLAP_INVALID_ID;
use moose_core::Float;
use moose_core::cast::{len_u32, size_of_u32};
use moose_core::events::{
    EVENT_LIST_PREALLOC, Event, EventBody, EventList, ExactEvent, ExactEventBody,
    ExactEventQualifiers, ExactEventRef, ExactNoteAddress, ExactNoteKind, LosslessEventRef,
    OutputEventStatus, PushError, RawMidi1, RawUmp, TransportInfo,
};
use moose_core::export::PluginExport;
use moose_core::info::{MidiDialect, PluginInfo};
#[cfg(test)]
use moose_core::midi::per_note_bend_semitones;
use moose_core::midi::{
    PER_NOTE_VOLUME_MAX_GAIN, decode_short_message, denorm_7bit, downconvert_to_midi1,
    per_note_bend_from_semitones,
};
use moose_core::ump::{decode_ump_channel_voice_2, encode_ump_channel_voice_2};

// ---------------------------------------------------------------------------
// Event conversion: CLAP input events -> EventList
// ---------------------------------------------------------------------------

/// 32-bit wire value -> CLAP `0..1` note-expression value.
#[cfg(test)]
pub(crate) fn unit_from_u32(v: u32) -> f64 {
    f64::from(v) / f64::from(u32::MAX)
}

/// CLAP `0..1` note-expression value -> 32-bit wire value.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn u32_from_unit(v: f64) -> u32 {
    (v.clamp(0.0, 1.0) * f64::from(u32::MAX)).round() as u32
}

/// Map a moose per-note 2.0 event to a CLAP note expression
/// `(expression_id, channel, note, value)`. `TUNING` is in semitones;
/// the rest are `0..1`.
#[cfg(test)]
pub(crate) fn clap_note_expression_of(body: &EventBody) -> Option<(i32, u8, u8, f64)> {
    match *body {
        // Registered per-note controllers only: the predefined CLAP
        // expression ids carry the registered indices' semantics
        // (7 = volume, 74 = brightness, ...); an assignable index is
        // manufacturer-defined and must not alias onto them.
        EventBody::PerNoteCC {
            channel,
            note,
            cc,
            value,
            registered: true,
            ..
        } => {
            let id = match cc {
                7 => CLAP_NOTE_EXPRESSION_VOLUME,
                10 => CLAP_NOTE_EXPRESSION_PAN,
                1 => CLAP_NOTE_EXPRESSION_VIBRATO,
                11 => CLAP_NOTE_EXPRESSION_EXPRESSION,
                74 => CLAP_NOTE_EXPRESSION_BRIGHTNESS,
                _ => return None,
            };
            // CLAP `VOLUME` is plain linear gain `0..=4` (the wire's
            // quarter point is unity); the other ids are `0..=1`.
            let value = if id == CLAP_NOTE_EXPRESSION_VOLUME {
                PER_NOTE_VOLUME_MAX_GAIN * unit_from_u32(value)
            } else {
                unit_from_u32(value)
            };
            Some((id, channel, note, value))
        }
        EventBody::PerNotePitchBend {
            channel,
            note,
            value,
            ..
        } => Some((
            CLAP_NOTE_EXPRESSION_TUNING,
            channel,
            note,
            per_note_bend_semitones(value),
        )),
        EventBody::PolyPressure2 {
            channel,
            note,
            pressure,
            ..
        } => Some((
            CLAP_NOTE_EXPRESSION_PRESSURE,
            channel,
            note,
            unit_from_u32(pressure),
        )),
        _ => None,
    }
}

/// Resolve a CLAP note event's `(channel, note)` address. CLAP carries
/// both as `i16` where `-1` is a "match all" wildcard; moose's
/// `EventBody` speaks concrete MIDI addresses (`channel 0..=15`,
/// `note 0..=127`) and plugins index tables by them, so anything
/// outside the domain - wildcards included - is dropped rather than
/// delivered mislabeled. (Wildcard `NOTE_OFF` / `NOTE_CHOKE` don't take
/// this path; see [`SoundingNotes`].)
#[cfg(test)]
pub(crate) fn clap_note_address(ne: &clap_event_note) -> Option<(u8, u8)> {
    let channel = u8::try_from(ne.channel).ok().filter(|c| *c <= 15)?;
    let note = u8::try_from(ne.key).ok().filter(|k| *k <= 127)?;
    Some((channel, note))
}

/// One wildcardable axis of a CLAP note address: `-1` matches all,
/// a concrete in-domain value matches itself, out-of-domain junk
/// matches nothing (the event drops, same as the concrete path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoteAxis {
    All,
    One(u8),
    Invalid,
}

impl NoteAxis {
    pub(crate) fn parse(raw: i16, domain: u8) -> Self {
        if raw == -1 {
            return Self::All;
        }
        match u8::try_from(raw) {
            Ok(v) if v < domain => Self::One(v),
            _ => Self::Invalid,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pck {
    pub(crate) port: u8,
    pub(crate) channel: u8,
    pub(crate) key: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrackedVoice {
    pub(crate) pck: Pck,
    pub(crate) note_id: i32,
    pub(crate) lifecycle: VoiceLifecycle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VoiceLifecycle {
    Active,
    Released,
    Choked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoteIdAxis {
    All,
    One(i32),
    Invalid,
}

impl NoteIdAxis {
    pub(crate) fn parse(raw: i32) -> Self {
        match raw {
            -1 => Self::All,
            0.. => Self::One(raw),
            _ => Self::Invalid,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VoicePattern {
    pub(crate) port: NoteAxis,
    pub(crate) channel: NoteAxis,
    pub(crate) key: NoteAxis,
    pub(crate) note_id: NoteIdAxis,
}

impl VoicePattern {
    pub(crate) fn parse(port: i16, channel: i16, key: i16, note_id: i32, port_count: u8) -> Self {
        Self {
            port: NoteAxis::parse(port, port_count),
            channel: NoteAxis::parse(channel, 16),
            key: NoteAxis::parse(key, 128),
            note_id: NoteIdAxis::parse(note_id),
        }
    }

    pub(crate) fn is_valid(self) -> bool {
        self.port != NoteAxis::Invalid
            && self.channel != NoteAxis::Invalid
            && self.key != NoteAxis::Invalid
            && self.note_id != NoteIdAxis::Invalid
    }

    pub(crate) fn concrete_pck(self) -> Option<Pck> {
        match (self.port, self.channel, self.key) {
            (NoteAxis::One(port), NoteAxis::One(channel), NoteAxis::One(key)) => {
                Some(Pck { port, channel, key })
            }
            _ => None,
        }
    }

    pub(crate) fn matches(self, voice: TrackedVoice) -> bool {
        fn axis_matches(axis: NoteAxis, value: u8) -> bool {
            matches!(axis, NoteAxis::All) || matches!(axis, NoteAxis::One(one) if one == value)
        }

        axis_matches(self.port, voice.pck.port)
            && axis_matches(self.channel, voice.pck.channel)
            && axis_matches(self.key, voice.pck.key)
            && (matches!(self.note_id, NoteIdAxis::All)
                || matches!(self.note_id, NoteIdAxis::One(one) if one == voice.note_id))
    }
}

/// Bounded, preallocated tracker for CLAP note identities. Every
/// `(port, channel, key, note_id)` voice is retained separately, including
/// stacked anonymous (`note_id == -1`) voices. Wildcard fanout is emitted only
/// while the tracker is complete; an overflow makes completeness sticky until
/// the next host reset so partial state can never invent semantic events.
pub(crate) struct SoundingNotes {
    pub(crate) voices: Vec<TrackedVoice>,
    pub(crate) complete: bool,
}

impl SoundingNotes {
    pub(crate) fn new(_midi_input_ports: u8) -> Self {
        Self {
            voices: Vec::with_capacity(EVENT_LIST_PREALLOC),
            complete: true,
        }
    }

    pub(crate) fn track(&mut self, voice: TrackedVoice) -> bool {
        if self.voices.len() >= self.voices.capacity() {
            self.complete = false;
            return false;
        }
        self.voices.push(voice);
        true
    }

    #[cfg(test)]
    pub(crate) fn set(&mut self, port: u8, channel: u8, key: u8) {
        let _ = self.track(TrackedVoice {
            pck: Pck { port, channel, key },
            note_id: -1,
            lifecycle: VoiceLifecycle::Active,
        });
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self, port: u8, channel: u8, key: u8) {
        self.voices
            .retain(|voice| voice.pck != Pck { port, channel, key });
    }

    pub(crate) fn clear_all(&mut self) {
        self.voices.clear();
        self.complete = true;
    }

    #[cfg(test)]
    pub(crate) fn drain_matching(
        &mut self,
        port: u8,
        channel: Option<u8>,
        key: Option<u8>,
        f: impl FnMut(u8, u8),
    ) {
        self.visit_matching(port, channel, key, true, f);
    }

    #[cfg(test)]
    pub(crate) fn for_each_matching(
        &mut self,
        port: u8,
        channel: Option<u8>,
        key: Option<u8>,
        f: impl FnMut(u8, u8),
    ) {
        self.visit_matching(port, channel, key, false, f);
    }

    #[cfg(test)]
    pub(crate) fn visit_matching(
        &mut self,
        port: u8,
        channel: Option<u8>,
        key: Option<u8>,
        clear: bool,
        mut f: impl FnMut(u8, u8),
    ) {
        let pattern = VoicePattern {
            port: NoteAxis::One(port),
            channel: channel.map_or(NoteAxis::All, NoteAxis::One),
            key: key.map_or(NoteAxis::All, NoteAxis::One),
            note_id: NoteIdAxis::All,
        };
        if clear {
            self.release_pattern(pattern, |pck| f(pck.channel, pck.key));
        } else {
            self.for_each_pattern(pattern, |pck| f(pck.channel, pck.key));
        }
    }

    pub(crate) fn release_pattern(&mut self, pattern: VoicePattern, mut f: impl FnMut(Pck)) {
        if !pattern.is_valid() {
            return;
        }
        let mut matched = [Pck::default(); EVENT_LIST_PREALLOC];
        let mut matched_len = 0;
        for voice in &mut self.voices {
            if voice.lifecycle == VoiceLifecycle::Active && pattern.matches(*voice) {
                if self.complete {
                    matched[matched_len] = voice.pck;
                    matched_len += 1;
                }
                voice.lifecycle = VoiceLifecycle::Released;
            }
        }
        if !self.complete {
            return;
        }
        emit_unique_pcks(&mut matched[..matched_len], &mut f);
    }

    pub(crate) fn choke_pattern(&mut self, pattern: VoicePattern) {
        if !pattern.is_valid() {
            return;
        }
        for voice in &mut self.voices {
            if voice.lifecycle != VoiceLifecycle::Choked && pattern.matches(*voice) {
                voice.lifecycle = VoiceLifecycle::Choked;
            }
        }
    }

    pub(crate) fn remove_pattern(&mut self, pattern: VoicePattern) {
        if pattern.is_valid() {
            self.voices.retain(|voice| !pattern.matches(*voice));
        }
    }

    pub(crate) fn for_each_pattern(&self, pattern: VoicePattern, mut f: impl FnMut(Pck)) {
        if !pattern.is_valid() || !self.complete {
            return;
        }
        let mut matched = [Pck::default(); EVENT_LIST_PREALLOC];
        let mut matched_len = 0;
        for voice in &self.voices {
            if voice.lifecycle != VoiceLifecycle::Choked && pattern.matches(*voice) {
                matched[matched_len] = voice.pck;
                matched_len += 1;
            }
        }
        emit_unique_pcks(&mut matched[..matched_len], &mut f);
    }
}

pub(crate) fn emit_unique_pcks(matched: &mut [Pck], mut f: impl FnMut(Pck)) {
    matched.sort_unstable();
    let mut previous = None;
    for pck in matched {
        if previous != Some(*pck) {
            f(*pck);
            previous = Some(*pck);
        }
    }
}

/// Decode a CLAP note expression into its moose per-note 2.0 event,
/// addressed to one concrete `(channel, note)` voice. The event's own
/// (possibly wildcard) axes are resolved by the caller - see
/// [`push_note_expressions`].
pub(crate) fn note_expression_body(
    ne: &clap_event_note_expression,
    channel: u8,
    note: u8,
) -> Option<EventBody> {
    let cc = match ne.expression_id {
        CLAP_NOTE_EXPRESSION_TUNING => {
            return Some(EventBody::PerNotePitchBend {
                group: 0,
                channel,
                note,
                value: per_note_bend_from_semitones(ne.value),
            });
        }
        CLAP_NOTE_EXPRESSION_VOLUME => 7,
        CLAP_NOTE_EXPRESSION_PAN => 10,
        CLAP_NOTE_EXPRESSION_VIBRATO => 1,
        CLAP_NOTE_EXPRESSION_EXPRESSION => 11,
        CLAP_NOTE_EXPRESSION_BRIGHTNESS => 74,
        CLAP_NOTE_EXPRESSION_PRESSURE => {
            return Some(EventBody::PolyPressure2 {
                group: 0,
                channel,
                note,
                pressure: u32_from_unit(ne.value),
            });
        }
        _ => return None,
    };
    // CLAP `VOLUME` arrives as plain linear gain `0..=4`; normalize
    // into the wire domain so unity gain lands on the quarter point.
    let value = if ne.expression_id == CLAP_NOTE_EXPRESSION_VOLUME {
        u32_from_unit(ne.value / PER_NOTE_VOLUME_MAX_GAIN)
    } else {
        u32_from_unit(ne.value)
    };
    Some(EventBody::PerNoteCC {
        group: 0,
        channel,
        note,
        cc,
        value,
        registered: true,
    })
}

/// Build a `TransportInfo` from a CLAP transport event/struct.
///
/// Same flag-driven decoding is needed in two places - the
/// `CLAP_EVENT_TRANSPORT` arm of `convert_input_events` (which sees a
/// `clap_event_transport` arriving as an input event mid-block) and
/// the per-process `clap_process::transport` field. Hosts deliver
/// transport state through whichever channel they prefer; the bit
/// layout is identical, so the decode is too.
// CLAP transport positions arrive as `i64` fixed-point counts that
// must be divided into `f64` seconds/beats; the `i64 as f64`
// narrowing is bounded in practice by song-length (well below 2^52).
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub(crate) fn build_transport_info(t: &clap_event_transport, sample_rate: f64) -> TransportInfo {
    let flags = t.flags;
    let beats_timeline = flags & CLAP_TRANSPORT_HAS_BEATS_TIMELINE != 0;
    let has_time_sig = flags & CLAP_TRANSPORT_HAS_TIME_SIGNATURE != 0;
    let position_seconds = if flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE != 0 {
        t.song_pos_seconds as f64 / CLAP_SECTIME_FACTOR as f64
    } else {
        0.0
    };
    TransportInfo {
        playing: flags & CLAP_TRANSPORT_IS_PLAYING != 0,
        recording: flags & CLAP_TRANSPORT_IS_RECORDING != 0,
        tempo: if flags & CLAP_TRANSPORT_HAS_TEMPO != 0 {
            t.tempo
        } else {
            120.0
        },
        // CLAP delivers `tsig_num` / `tsig_denom` as `i16`; the
        // narrowing is bounded by the MIDI domain (≤ 255).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        time_sig_num: if has_time_sig { t.tsig_num as u8 } else { 4 },
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        time_sig_den: if has_time_sig { t.tsig_denom as u8 } else { 4 },
        // CLAP transport carries no sample position; derive it from the
        // seconds timeline (rounded to the nearest sample), the same way
        // the host computes it. 0 when no seconds timeline is provided.
        position_samples: (position_seconds * sample_rate).round() as i64,
        position_seconds,
        position_beats: if beats_timeline {
            t.song_pos_beats as f64 / CLAP_BEATTIME_FACTOR as f64
        } else {
            0.0
        },
        bar_start_beats: if beats_timeline {
            t.bar_start as f64 / CLAP_BEATTIME_FACTOR as f64
        } else {
            0.0
        },
        loop_active: flags & CLAP_TRANSPORT_IS_LOOP_ACTIVE != 0,
        loop_start_beats: if beats_timeline {
            t.loop_start_beats as f64 / CLAP_BEATTIME_FACTOR as f64
        } else {
            0.0
        },
        loop_end_beats: if beats_timeline {
            t.loop_end_beats as f64 / CLAP_BEATTIME_FACTOR as f64
        } else {
            0.0
        },
    }
}

/// UMP packet width from the message-type nibble (MIDI 2.0 UMP table 1).
/// CLAP stores every packet in four words, so retaining this width is what
/// distinguishes meaningful words from padding without decoding the payload.
pub(crate) fn ump_word_count(word0: u32) -> u8 {
    const WORD_COUNTS: [u8; 16] = [1, 1, 1, 2, 2, 4, 1, 1, 2, 2, 2, 3, 3, 4, 4, 4];
    let message_type = usize::try_from((word0 >> 28) & 0x0F).unwrap_or(0);
    WORD_COUNTS[message_type]
}

pub(crate) fn exact_event_qualifiers(flags: u32) -> ExactEventQualifiers {
    ExactEventQualifiers {
        is_live: flags & CLAP_EVENT_IS_LIVE != 0,
        dont_record: flags & CLAP_EVENT_DONT_RECORD != 0,
    }
}

pub(crate) fn clap_exact_flags(qualifiers: ExactEventQualifiers) -> u32 {
    let mut flags = 0;
    if qualifiers.is_live {
        flags |= CLAP_EVENT_IS_LIVE;
    }
    if qualifiers.dont_record {
        flags |= CLAP_EVENT_DONT_RECORD;
    }
    flags
}

pub(crate) fn exact_note_event(
    sample_offset: u32,
    kind: ExactNoteKind,
    note: &clap_event_note,
) -> ExactEvent {
    ExactEvent::new(
        sample_offset,
        ExactEventBody::Note {
            kind,
            address: ExactNoteAddress::from_raw_signed(
                note.port_index,
                note.channel,
                note.key,
                note.note_id,
            ),
            velocity: note.velocity,
        },
    )
    .with_qualifiers(exact_event_qualifiers(note.header.flags))
}

pub(crate) fn exact_note_expression(
    sample_offset: u32,
    expression: &clap_event_note_expression,
) -> ExactEvent {
    ExactEvent::new(
        sample_offset,
        ExactEventBody::NoteExpression {
            expression_id: expression.expression_id,
            address: ExactNoteAddress::from_raw_signed(
                expression.port_index,
                expression.channel,
                expression.key,
                expression.note_id,
            ),
            value: expression.value,
        },
    )
    .with_qualifiers(exact_event_qualifiers(expression.header.flags))
}

/// Keep the exact lane authoritative without sacrificing the semantic view
/// when only one bounded lane is full. Returns whether the typed view became
/// visible, which lets state trackers avoid seeding semantic fanout from an
/// exact-only event. A partial-capacity result remains observable through
/// `EventList::overflow()`.
pub(crate) fn push_exact_with_fallback(
    event_list: &mut EventList,
    exact: ExactEvent,
    fallback: Event,
) -> bool {
    match event_list.try_push_with_exact(fallback, exact) {
        Ok(()) => true,
        Err(PushError::PoolFull | PushError::UnknownExactEvent | PushError::VoiceTrackerFull) => {
            false
        }
        Err(PushError::EventFull) => {
            let _ = event_list.try_push_exact(exact);
            false
        }
        Err(PushError::ExactEventFull) => event_list.try_push(fallback).is_ok(),
    }
}

pub(crate) type ClapTryPush =
    unsafe extern "C" fn(list: *const clap_output_events, event: *const clap_event_header) -> bool;

pub(crate) unsafe extern "C" fn accept_output_push(
    _list: *const clap_output_events,
    _event: *const clap_event_header,
) -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExactEmit {
    Emitted,
    Unsupported,
    Invalid,
    QueueFull,
}

pub(crate) fn exact_push_result(pushed: bool) -> ExactEmit {
    if pushed {
        ExactEmit::Emitted
    } else {
        ExactEmit::QueueFull
    }
}

pub(crate) fn note_expression_value_is_valid(expression_id: i32, value: f64) -> bool {
    if !value.is_finite() {
        return false;
    }
    match expression_id {
        CLAP_NOTE_EXPRESSION_VOLUME => value > 0.0 && value <= PER_NOTE_VOLUME_MAX_GAIN,
        CLAP_NOTE_EXPRESSION_PAN
        | CLAP_NOTE_EXPRESSION_VIBRATO
        | CLAP_NOTE_EXPRESSION_EXPRESSION
        | CLAP_NOTE_EXPRESSION_BRIGHTNESS
        | CLAP_NOTE_EXPRESSION_PRESSURE => (0.0..=1.0).contains(&value),
        CLAP_NOTE_EXPRESSION_TUNING => (-120.0..=120.0).contains(&value),
        _ => false,
    }
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

pub(crate) fn typed_output_body_is_valid(body: &EventBody) -> bool {
    match *body {
        EventBody::NoteOn {
            group,
            channel,
            note,
            velocity,
        }
        | EventBody::NoteOff {
            group,
            channel,
            note,
            velocity,
        } => group == 0 && channel < 16 && note < 128 && velocity < 128,
        EventBody::Aftertouch {
            group,
            channel,
            note,
            pressure,
        } => group == 0 && channel < 16 && note < 128 && pressure < 128,
        EventBody::ChannelPressure {
            group,
            channel,
            pressure,
        } => group == 0 && channel < 16 && pressure < 128,
        EventBody::ControlChange {
            group,
            channel,
            cc,
            value,
        } => group == 0 && channel < 16 && cc < 128 && value < 128,
        EventBody::PitchBend {
            group,
            channel,
            value,
        } => group == 0 && channel < 16 && value < 16_384,
        EventBody::ProgramChange {
            group,
            channel,
            program,
        } => group == 0 && channel < 16 && program < 128,
        EventBody::NoteOn2 {
            group,
            channel,
            note,
            attribute_type,
            ..
        }
        | EventBody::NoteOff2 {
            group,
            channel,
            note,
            attribute_type,
            ..
        } => group < 16 && channel < 16 && note < 128 && attribute_type < 4,
        EventBody::PolyPressure2 {
            group,
            channel,
            note,
            ..
        }
        | EventBody::PerNotePitchBend {
            group,
            channel,
            note,
            ..
        }
        | EventBody::PerNoteCC {
            group,
            channel,
            note,
            ..
        } => group < 16 && channel < 16 && note < 128,
        EventBody::PerNoteManagement {
            group,
            channel,
            note,
            flags,
        } => group < 16 && channel < 16 && note < 128 && flags & !0b11 == 0,
        EventBody::ControlChange2 {
            group, channel, cc, ..
        } => group < 16 && channel < 16 && cc < 128,
        EventBody::ChannelPressure2 { group, channel, .. }
        | EventBody::PitchBend2 { group, channel, .. } => group < 16 && channel < 16,
        EventBody::ProgramChange2 {
            group,
            channel,
            program,
            bank,
        } => {
            group < 16
                && channel < 16
                && program < 128
                && bank.is_none_or(|(msb, lsb)| msb < 128 && lsb < 128)
        }
        EventBody::RegisteredController {
            group,
            channel,
            bank,
            index,
            ..
        }
        | EventBody::AssignableController {
            group,
            channel,
            bank,
            index,
            ..
        } => group < 16 && channel < 16 && bank < 128 && index < 128,
        EventBody::ParamChange { id, value } => id != CLAP_INVALID_ID && value.is_finite(),
        EventBody::ParamMod { id, note_id, value } => {
            id != CLAP_INVALID_ID && note_id >= -1 && value.is_finite()
        }
        EventBody::Transport(_) | EventBody::SysEx { .. } => true,
        // CLAP param events are points; a ramp has no output encoding.
        EventBody::ParamRamp { .. } => false,
    }
}

pub(crate) fn is_portless_output_body(body: &EventBody) -> bool {
    matches!(
        body,
        EventBody::ParamChange { .. } | EventBody::ParamMod { .. } | EventBody::Transport(_)
    )
}

pub(crate) fn exact_note_fields(
    address: ExactNoteAddress,
    output_ports: u8,
    require_concrete_note_address: bool,
) -> Option<(i32, i16, i16, i16)> {
    if output_ports == 0
        || address.port.invalid_raw().is_some()
        || address.channel.invalid_raw().is_some()
        || address.key.invalid_raw().is_some()
        || address.note_id.invalid_raw().is_some()
    {
        return None;
    }

    let port = address.port.raw_i32();
    let channel = address.channel.raw_i32();
    let key = address.key.raw_i32();
    let note_id = address.note_id.raw_i32();
    if note_id < -1
        || (port != -1 && !(0..i32::from(output_ports)).contains(&port))
        || (channel != -1 && !(0..=15).contains(&channel))
        || (key != -1 && !(0..=127).contains(&key))
        || (require_concrete_note_address && (port == -1 || channel == -1 || key == -1))
    {
        return None;
    }

    Some((
        note_id,
        i16::try_from(port).ok()?,
        i16::try_from(channel).ok()?,
        i16::try_from(key).ok()?,
    ))
}

/// Emit an exact event when the configured CLAP port can legally carry it.
/// Unsupported and invalid representations fail closed, while queue exhaustion
/// stops the globally ordered replay.
pub(crate) unsafe fn emit_exact_clap(
    out_events: *const clap_output_events,
    try_push: ClapTryPush,
    event: ExactEventRef<'_>,
    info: &PluginInfo,
    frames_count: u32,
) -> ExactEmit {
    if !event.metadata().is_none() {
        return ExactEmit::Unsupported;
    }
    let sample_offset = event.sample_offset();
    if sample_offset >= frames_count {
        return ExactEmit::Invalid;
    }
    let flags = clap_exact_flags(event.qualifiers());
    match event.body() {
        ExactEventBody::Midi1 { port, message } => {
            if *port >= u16::from(info.midi_output_ports) || !raw_midi1_valid(*message) {
                return ExactEmit::Invalid;
            }
            let ev = clap_event_midi {
                header: clap_event_header {
                    size: size_of_u32::<clap_event_midi>(),
                    time: sample_offset,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_MIDI,
                    flags,
                },
                port_index: *port,
                data: *message.storage(),
            };
            // SAFETY: `out_events` is the host's output list for this block (or null in preflight,
            // where `try_push` never dereferences it); `ev` is a local that outlives the push,
            // which copies it.
            exact_push_result(unsafe { try_push(out_events, &raw const ev.header) })
        }
        ExactEventBody::Ump { port, packet } => {
            if *port >= u16::from(info.midi_output_ports)
                || packet.word_count() != usize::from(ump_word_count(packet.storage()[0]))
            {
                return ExactEmit::Invalid;
            }
            if info.midi_output_dialect != MidiDialect::Midi2 {
                return ExactEmit::Unsupported;
            }
            let ev = clap_event_midi2 {
                header: clap_event_header {
                    size: size_of_u32::<clap_event_midi2>(),
                    time: sample_offset,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_MIDI2,
                    flags,
                },
                port_index: *port,
                data: *packet.storage(),
            };
            // SAFETY: Same as above: `out_events` is the block's output list (or null in
            // preflight), and `ev` is a local copied by the push.
            exact_push_result(unsafe { try_push(out_events, &raw const ev.header) })
        }
        ExactEventBody::SysEx { port } => {
            if *port >= u16::from(info.midi_output_ports) {
                return ExactEmit::Invalid;
            }
            let Some(bytes) = event.sysex_bytes_checked() else {
                return ExactEmit::Invalid;
            };
            let ev = clap_event_midi_sysex {
                header: clap_event_header {
                    size: size_of_u32::<clap_event_midi_sysex>(),
                    time: sample_offset,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_MIDI_SYSEX,
                    flags,
                },
                port_index: *port,
                buffer: bytes.as_ptr(),
                size: len_u32(bytes.len()),
            };
            // SAFETY: `out_events` is the block's output list (or null in preflight). `ev.buffer`
            // borrows `bytes`, which outlives the push, and the host copies `SysEx` payloads during
            // `try_push`.
            exact_push_result(unsafe { try_push(out_events, &raw const ev.header) })
        }
        ExactEventBody::Note {
            kind,
            address,
            velocity,
        } => {
            // Choke is host -> plugin; replaying it toward the host would
            // reverse a lifecycle command with no legal output meaning.
            if matches!(kind, ExactNoteKind::Choke) {
                return ExactEmit::Unsupported;
            }
            let type_ = match kind {
                ExactNoteKind::On => CLAP_EVENT_NOTE_ON,
                ExactNoteKind::Off => CLAP_EVENT_NOTE_OFF,
                ExactNoteKind::End => CLAP_EVENT_NOTE_END,
                _ => return ExactEmit::Unsupported,
            };
            if !velocity.is_finite() || !(0.0..=1.0).contains(velocity) {
                return ExactEmit::Invalid;
            }
            let port_count = if matches!(kind, ExactNoteKind::End) {
                info.midi_input_ports
            } else {
                info.midi_output_ports
            };
            let Some((note_id, port_index, channel, key)) = exact_note_fields(
                *address,
                port_count,
                matches!(kind, ExactNoteKind::On | ExactNoteKind::End),
            ) else {
                return ExactEmit::Invalid;
            };
            let ev = clap_event_note {
                header: clap_event_header {
                    size: size_of_u32::<clap_event_note>(),
                    time: sample_offset,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_,
                    flags,
                },
                note_id,
                port_index,
                channel,
                key,
                velocity: *velocity,
            };
            // SAFETY: `out_events` is the block's output list (or null in preflight), and `ev` is a
            // local copied by the push.
            exact_push_result(unsafe { try_push(out_events, &raw const ev.header) })
        }
        ExactEventBody::NoteExpression {
            expression_id,
            address,
            value,
        } => {
            if !note_expression_value_is_valid(*expression_id, *value) {
                return ExactEmit::Invalid;
            }
            let Some((note_id, port_index, channel, key)) =
                exact_note_fields(*address, info.midi_output_ports, false)
            else {
                return ExactEmit::Invalid;
            };
            let ev = clap_event_note_expression {
                header: clap_event_header {
                    size: size_of_u32::<clap_event_note_expression>(),
                    time: sample_offset,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_NOTE_EXPRESSION,
                    flags,
                },
                expression_id: *expression_id,
                note_id,
                port_index,
                channel,
                key,
                value: *value,
            };
            // SAFETY: `out_events` is the block's output list (or null in preflight), and `ev` is a
            // local copied by the push.
            exact_push_result(unsafe { try_push(out_events, &raw const ev.header) })
        }
        _ => ExactEmit::Unsupported,
    }
}

pub(crate) fn preflight_clap_output(
    events: &EventList,
    info: &PluginInfo,
    param_infos: &moose_params::ParamCatalog,
    frames_count: u32,
) -> OutputEventStatus {
    for replay in events.lossless_iter() {
        let event = match replay {
            LosslessEventRef::Exact(exact) => {
                // SAFETY: Preflight passes a null list together with `accept_output_push`, which
                // never dereferences it, so no host memory is touched.
                let status = unsafe {
                    emit_exact_clap(
                        std::ptr::null(),
                        accept_output_push,
                        exact,
                        info,
                        frames_count,
                    )
                };
                match status {
                    ExactEmit::Emitted => continue,
                    ExactEmit::Unsupported => return OutputEventStatus::Unsupported,
                    ExactEmit::Invalid | ExactEmit::QueueFull => {
                        return OutputEventStatus::Invalid;
                    }
                }
            }
            LosslessEventRef::Typed(event) => event,
        };
        if event.sample_offset >= frames_count
            || !typed_output_body_is_valid(&event.body)
            || (!is_portless_output_body(&event.body)
                && (info.midi_output_ports == 0 || event.port >= info.midi_output_ports))
        {
            return OutputEventStatus::Invalid;
        }
        match &event.body {
            EventBody::ParamChange { id, .. } if param_infos.by_id(*id).is_none() => {
                return OutputEventStatus::Invalid;
            }
            EventBody::SysEx { .. } => {
                if events.sysex_bytes_checked(&event.body).is_none() {
                    return OutputEventStatus::Invalid;
                }
            }
            body @ (EventBody::NoteOn2 { .. }
            | EventBody::NoteOff2 { .. }
            | EventBody::PolyPressure2 { .. }
            | EventBody::PerNoteCC { .. }
            | EventBody::PerNotePitchBend { .. }
            | EventBody::PerNoteManagement { .. }
            | EventBody::ControlChange2 { .. }
            | EventBody::ChannelPressure2 { .. }
            | EventBody::PitchBend2 { .. }
            | EventBody::ProgramChange2 { .. }
            | EventBody::RegisteredController { .. }
            | EventBody::AssignableController { .. }) => {
                if info.midi_output_dialect != MidiDialect::Midi2 {
                    return OutputEventStatus::Unsupported;
                }
                if encode_ump_channel_voice_2(body).is_none() {
                    return OutputEventStatus::Invalid;
                }
            }
            EventBody::ParamMod { .. } | EventBody::Transport(_) => {
                return OutputEventStatus::Unsupported;
            }
            _ => {}
        }
    }
    OutputEventStatus::Success
}

pub(crate) fn outbound_note_end_pattern(
    event: ExactEventRef<'_>,
    info: &PluginInfo,
) -> Option<VoicePattern> {
    let ExactEventBody::Note {
        kind: ExactNoteKind::End,
        address,
        ..
    } = event.body()
    else {
        return None;
    };
    let pattern = VoicePattern::parse(
        i16::try_from(address.port.value()?).ok()?,
        i16::from(address.channel.value()?),
        i16::from(address.key.value()?),
        address.note_id.raw_i32(),
        info.midi_input_ports,
    );
    (pattern.is_valid() && pattern.concrete_pck().is_some()).then_some(pattern)
}

/// Preserve one exact release and expose each newly released semantic PCK once.
/// Released voices remain tracked for note expressions until the plugin emits
/// `NOTE_END`; a repeated host release therefore has no semantic companion.
pub(crate) fn push_note_offs<P: PluginExport>(
    scr: &mut ClapAudio<P>,
    info: &PluginInfo,
    note_event: &clap_event_note,
    sample_offset: u32,
    exact: ExactEvent,
    velocity: u8,
) {
    let pattern = VoicePattern::parse(
        note_event.port_index,
        note_event.channel,
        note_event.key,
        note_event.note_id,
        info.midi_input_ports,
    );
    if !pattern.is_valid() {
        return;
    }

    let ClapAudio {
        sounding_notes,
        event_list,
        ..
    } = scr;
    let token = event_list.try_push_exact_token(exact);
    sounding_notes.release_pattern(pattern, |pck| {
        let event = Event::on_port(
            sample_offset,
            pck.port,
            EventBody::NoteOff {
                group: 0,
                channel: pck.channel,
                note: pck.key,
                velocity,
            },
        );
        if let Ok(token) = token {
            let _ = event_list.try_push_exact_companion(token, event);
        } else {
            let _ = event_list.try_push(event);
        }
    });
}

/// Preserve a host choke exactly and stop later semantic expression fanout for
/// the affected voices. Choke is not a MIDI note-off and has no typed fallback.
pub(crate) fn push_note_choke<P: PluginExport>(
    scr: &mut ClapAudio<P>,
    info: &PluginInfo,
    note_event: &clap_event_note,
    exact: ExactEvent,
) {
    let pattern = VoicePattern::parse(
        note_event.port_index,
        note_event.channel,
        note_event.key,
        note_event.note_id,
        info.midi_input_ports,
    );
    if !pattern.is_valid() {
        return;
    }
    let _ = scr.event_list.try_push_exact(exact);
    scr.sounding_notes.choke_pattern(pattern);
}

/// Deliver one exact note expression plus the linked semantic PCK views that
/// a typed plugin can consume.
pub(crate) fn push_note_expressions<P: PluginExport>(
    scr: &mut ClapAudio<P>,
    info: &PluginInfo,
    ne: &clap_event_note_expression,
    sample_offset: u32,
    exact: ExactEvent,
) {
    let pattern = VoicePattern::parse(
        ne.port_index,
        ne.channel,
        ne.key,
        ne.note_id,
        info.midi_input_ports,
    );
    if !pattern.is_valid() {
        return;
    }

    let semantic = |pck: Pck| {
        let decoded = note_expression_body(ne, pck.channel, pck.key)?;
        let body = if info.midi_input_dialect == MidiDialect::Midi2 {
            Some(decoded)
        } else {
            downconvert_to_midi1(&decoded)
        }?;
        Some(Event::on_port(sample_offset, pck.port, body))
    };

    let ClapAudio {
        sounding_notes,
        event_list,
        ..
    } = scr;
    let token = event_list.try_push_exact_token(exact);
    sounding_notes.for_each_pattern(pattern, |pck| {
        let Some(event) = semantic(pck) else {
            return;
        };
        if let Ok(token) = token {
            let _ = event_list.try_push_exact_companion(token, event);
        } else {
            let _ = event_list.try_push(event);
        }
    });
}

/// `sort` controls whether the resulting `event_list` gets a stable
/// sort by sample offset. `process` needs sorted events (the plugin
/// iterates them in time order); `params_flush` discards the events
/// after extracting param/GUI updates and doesn't care about order, so
/// it passes `false` to skip the sort. Process calls supply `frames_count`
/// so out-of-block host timestamps fail closed before state changes; flush has
/// no audio block and passes `None`.
pub(crate) fn clap_event_has_size<T>(header: &clap_event_header) -> bool {
    header.size >= size_of_u32::<T>()
}

pub(crate) fn input_note_pattern(
    note: &clap_event_note,
    info: &PluginInfo,
    require_concrete_pck: bool,
) -> Option<VoicePattern> {
    if info.midi_input_ports == 0
        || !note.velocity.is_finite()
        || !(0.0..=1.0).contains(&note.velocity)
    {
        return None;
    }
    let pattern = VoicePattern::parse(
        note.port_index,
        note.channel,
        note.key,
        note.note_id,
        info.midi_input_ports,
    );
    (pattern.is_valid() && (!require_concrete_pck || pattern.concrete_pck().is_some()))
        .then_some(pattern)
}

pub(crate) fn input_note_expression_pattern(
    expression: &clap_event_note_expression,
    info: &PluginInfo,
) -> Option<VoicePattern> {
    if info.midi_input_ports == 0
        || !note_expression_value_is_valid(expression.expression_id, expression.value)
    {
        return None;
    }
    let pattern = VoicePattern::parse(
        expression.port_index,
        expression.channel,
        expression.key,
        expression.note_id,
        info.midi_input_ports,
    );
    pattern.is_valid().then_some(pattern)
}

pub(crate) fn declared_input_port(port: u16, count: u8) -> Option<u8> {
    (port < u16::from(count)).then(|| u8::try_from(port).ok())?
}

/// Record a `params_flush` param change for replay in the next process
/// block, latest value per id. Bounded by the vec's capacity (the param
/// count), so an id the plugin doesn't declare can't grow it.
pub(crate) fn defer_param_change(deferred: &mut Vec<(u32, f64)>, id: u32, value: f64) {
    if let Some(slot) = deferred.iter_mut().find(|(d, _)| *d == id) {
        slot.1 = value;
    } else if deferred.len() < deferred.capacity() {
        deferred.push((id, value));
    }
}

/// Push the deferred flush changes at offset 0 ahead of the block's own
/// events, then forget them. A state load discards them: they predate
/// the recalled state, like the live param events it drops.
pub(crate) fn replay_deferred_params(
    list: &mut EventList,
    deferred: &mut Vec<(u32, f64)>,
    state_loaded: bool,
) {
    if !state_loaded {
        for &(id, value) in deferred.iter() {
            list.push(Event::new(0, EventBody::ParamChange { id, value }));
        }
    }
    deferred.clear();
}

#[allow(clippy::too_many_lines)]
pub(crate) unsafe fn convert_input_events<P: PluginExport>(
    scr: &mut ClapAudio<P>,
    info: &PluginInfo,
    in_events: *const clap_input_events,
    sort: bool,
    state_loaded: bool,
    frames_count: Option<u32>,
) {
    // SAFETY: The caller passes `in_events` as null or the host's input list for this block (the
    // `convert_input_events` contract); every event header it returns is read within that call.
    unsafe {
        scr.event_list.clear();
        // Process path only (`params_flush` passes no frame count):
        // stopped-transport param events precede this block's events.
        if frames_count.is_some() {
            replay_deferred_params(&mut scr.event_list, &mut scr.deferred_params, state_loaded);
        }

        if in_events.is_null() {
            return;
        }

        let Some(size_fn) = (*in_events).size else {
            return;
        };
        let Some(get_fn) = (*in_events).get else {
            return;
        };

        let count = size_fn(in_events);

        for i in 0..count {
            let header_ptr = get_fn(in_events, i);
            if header_ptr.is_null() {
                continue;
            }

            let header = &*header_ptr;
            if !clap_event_has_size::<clap_event_header>(header)
                || header.space_id != CLAP_CORE_EVENT_SPACE_ID
                || frames_count.is_some_and(|frames| header.time >= frames)
            {
                continue;
            }

            let sample_offset = header.time;
            // `NOTE_END` is plugin -> host only. An inbound event is a
            // protocol violation and must not alter plugin voice state.
            if header.type_ == CLAP_EVENT_NOTE_END {
                continue;
            }
            match header.type_ {
                CLAP_EVENT_NOTE_ON => {
                    if !clap_event_has_size::<clap_event_note>(header) {
                        continue;
                    }
                    let note_event = &*header_ptr.cast::<clap_event_note>();
                    let Some(pattern) = input_note_pattern(note_event, info, true) else {
                        continue;
                    };
                    let Some(pck) = pattern.concrete_pck() else {
                        continue;
                    };
                    let exact = exact_note_event(sample_offset, ExactNoteKind::On, note_event);
                    // CLAP's f64 velocity is a normalized [0, 1]; moose
                    // exposes it as a wire-native 7-bit value in the semantic
                    // view to match every other format. Track the voice only
                    // when that semantic onset is actually visible.
                    let typed_visible = push_exact_with_fallback(
                        &mut scr.event_list,
                        exact,
                        Event::on_port(
                            sample_offset,
                            pck.port,
                            EventBody::NoteOn {
                                group: 0,
                                channel: pck.channel,
                                note: pck.key,
                                velocity: denorm_7bit(f32::from_f64(note_event.velocity)),
                            },
                        ),
                    );
                    if typed_visible
                        && !scr.sounding_notes.track(TrackedVoice {
                            pck,
                            note_id: note_event.note_id,
                            lifecycle: VoiceLifecycle::Active,
                        })
                    {
                        scr.event_list.record_overflow(PushError::VoiceTrackerFull);
                    }
                }
                CLAP_EVENT_NOTE_OFF => {
                    if !clap_event_has_size::<clap_event_note>(header) {
                        continue;
                    }
                    let note_event = &*header_ptr.cast::<clap_event_note>();
                    if input_note_pattern(note_event, info, false).is_none() {
                        continue;
                    }
                    let velocity = denorm_7bit(f32::from_f64(note_event.velocity));
                    let exact = exact_note_event(sample_offset, ExactNoteKind::Off, note_event);
                    push_note_offs(scr, info, note_event, sample_offset, exact, velocity);
                }
                CLAP_EVENT_NOTE_CHOKE => {
                    if !clap_event_has_size::<clap_event_note>(header) {
                        continue;
                    }
                    let note_event = &*header_ptr.cast::<clap_event_note>();
                    if input_note_pattern(note_event, info, false).is_none() {
                        continue;
                    }
                    let exact = exact_note_event(sample_offset, ExactNoteKind::Choke, note_event);
                    push_note_choke(scr, info, note_event, exact);
                }
                CLAP_EVENT_PARAM_VALUE => {
                    // When a state load was applied at the head of
                    // this block, param-change events queued by the
                    // host predate that intent (typical case: a clip-
                    // edge re-trigger sends preset-B automation in
                    // the same block as the preset-A state recall).
                    // Drop them so the just-restored preset isn't
                    // partly overwritten by stale automation.
                    if state_loaded || !clap_event_has_size::<clap_event_param_value>(header) {
                        continue;
                    }
                    let param_event = &*header_ptr.cast::<clap_event_param_value>();
                    if param_event.param_id == CLAP_INVALID_ID || !param_event.value.is_finite() {
                        continue;
                    }
                    // `set_plain` is deferred to the per-sub-block
                    // `apply_pending_events` pass in
                    // `chunked_process::process_chunked` - that way
                    // the smoother sees `set_target` at the event's
                    // sample, not at the head of the audio block.
                    scr.event_list.push(Event::new(
                        sample_offset,
                        EventBody::ParamChange {
                            id: param_event.param_id,
                            value: param_event.value,
                        },
                    ));
                }
                CLAP_EVENT_PARAM_MOD => {
                    // Same rationale as PARAM_VALUE above: drop
                    // pre-state-load mod packets.
                    if state_loaded || !clap_event_has_size::<clap_event_param_mod>(header) {
                        continue;
                    }
                    let mod_event = &*header_ptr.cast::<clap_event_param_mod>();
                    if mod_event.param_id == CLAP_INVALID_ID
                        || mod_event.note_id < -1
                        || mod_event.port_index < -1
                        || mod_event.channel < -1
                        || mod_event.key < -1
                        || !mod_event.amount.is_finite()
                    {
                        continue;
                    }
                    // Moose advertises only global (`CLAP_PARAM_IS_MODULATABLE`)
                    // and per-note-id (`CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID`)
                    // modulation, never per-port / per-channel / per-key. A
                    // wildcard `note_id == -1` with a specific port/channel/key
                    // is scoped rather than global mono and must not overwrite
                    // the global mono modulation slot.
                    if mod_event.note_id == -1
                        && (mod_event.port_index != -1
                            || mod_event.channel != -1
                            || mod_event.key != -1)
                    {
                        continue;
                    }
                    scr.event_list.push(Event::new(
                        sample_offset,
                        EventBody::ParamMod {
                            id: mod_event.param_id,
                            note_id: mod_event.note_id,
                            value: mod_event.amount,
                        },
                    ));
                }
                CLAP_EVENT_TRANSPORT => {
                    if !clap_event_has_size::<clap_event_transport>(header) {
                        continue;
                    }
                    let transport = &*header_ptr.cast::<clap_event_transport>();
                    scr.event_list.push(Event::new(
                        sample_offset,
                        EventBody::Transport(build_transport_info(transport, scr.sample_rate)),
                    ));
                }
                CLAP_EVENT_MIDI => {
                    // CLAP carries MIDI 1.0 channel-voice messages in fixed
                    // 3-byte slots (with trailing padding for 1-/2-byte
                    // status bytes); derive the true length from the status
                    // byte and demux back into typed `EventBody` variants.
                    if !clap_event_has_size::<clap_event_midi>(header) {
                        continue;
                    }
                    let midi = &*header_ptr.cast::<clap_event_midi>();
                    let len =
                        u8::try_from(midi1_message_len(midi.data[0]).unwrap_or(3)).unwrap_or(3);
                    if let Some(message) = RawMidi1::new(midi.data, len) {
                        let exact = ExactEvent::new(
                            sample_offset,
                            ExactEventBody::Midi1 {
                                port: midi.port_index,
                                message,
                            },
                        )
                        .with_qualifiers(exact_event_qualifiers(midi.header.flags));
                        let status = midi.data[0] & 0xF0;
                        let semantic_is_valid = (0x80..=0xE0).contains(&status)
                            && midi.data[1] < 128
                            && (matches!(status, 0xC0 | 0xD0) || midi.data[2] < 128);
                        if let Some((port, body)) = declared_input_port(
                            midi.port_index,
                            info.midi_input_ports,
                        )
                        .zip(
                            semantic_is_valid
                                .then(|| {
                                    decode_short_message(midi.data[0], midi.data[1], midi.data[2])
                                })
                                .flatten(),
                        ) {
                            let _ = push_exact_with_fallback(
                                &mut scr.event_list,
                                exact,
                                Event::on_port(sample_offset, port, body),
                            );
                        } else {
                            let _ = scr.event_list.try_push_exact(exact);
                        }
                    }
                }
                CLAP_EVENT_MIDI_SYSEX => {
                    // CLAP delivers `SysEx` payloads as a pointer +
                    // length owned by the host for the duration of
                    // this `process()` call. Copy into our pool
                    // immediately - the bytes can't be assumed valid
                    // after we return. `push_sysex` is fail-closed
                    // when the pool is exhausted; we drop the
                    // message and keep going (a corrupt-by-split
                    // alternative is never the right answer for
                    // `SysEx`).
                    if !clap_event_has_size::<clap_event_midi_sysex>(header) {
                        continue;
                    }
                    let sysex = &*header_ptr.cast::<clap_event_midi_sysex>();
                    let Some(port) = declared_input_port(sysex.port_index, info.midi_input_ports)
                    else {
                        continue;
                    };
                    let bytes = if sysex.size == 0 {
                        &[][..]
                    } else if sysex.buffer.is_null() {
                        continue;
                    } else {
                        let Ok(size) = usize::try_from(sysex.size) else {
                            continue;
                        };
                        std::slice::from_raw_parts(sysex.buffer, size)
                    };
                    let exact = ExactEvent::new(
                        sample_offset,
                        ExactEventBody::SysEx {
                            port: sysex.port_index,
                        },
                    )
                    .with_qualifiers(exact_event_qualifiers(sysex.header.flags));
                    let _ = scr.event_list.try_push_sysex_with_exact_on_port(
                        sample_offset,
                        port,
                        bytes,
                        exact,
                    );
                }
                // Preserve every well-formed UMP exactly. Only a declared
                // MIDI 2.0 input port receives a typed semantic companion;
                // an ignored dialect never triggers a lossy down-conversion.
                CLAP_EVENT_MIDI2 => {
                    if !clap_event_has_size::<clap_event_midi2>(header) {
                        continue;
                    }
                    let midi2 = &*header_ptr.cast::<clap_event_midi2>();
                    if let Some(packet) = RawUmp::new(midi2.data, ump_word_count(midi2.data[0])) {
                        let exact = ExactEvent::new(
                            sample_offset,
                            ExactEventBody::Ump {
                                port: midi2.port_index,
                                packet,
                            },
                        )
                        .with_qualifiers(exact_event_qualifiers(midi2.header.flags));
                        if let Some((port, body)) =
                            declared_input_port(midi2.port_index, info.midi_input_ports)
                                .filter(|_| info.midi_input_dialect == MidiDialect::Midi2)
                                .zip(decode_ump_channel_voice_2(midi2.data))
                        {
                            let _ = push_exact_with_fallback(
                                &mut scr.event_list,
                                exact,
                                Event::on_port(sample_offset, port, body),
                            );
                        } else {
                            let _ = scr.event_list.try_push_exact(exact);
                        }
                    }
                }
                CLAP_EVENT_NOTE_EXPRESSION => {
                    // Per-note expression (hosts send these for MPE-style
                    // input). Decode to the 2.0 per-note event - fanned
                    // out across wildcard axes - with the down-converted
                    // channel form for plugins that didn't opt into 2.0.
                    if !clap_event_has_size::<clap_event_note_expression>(header) {
                        continue;
                    }
                    let ne = &*header_ptr.cast::<clap_event_note_expression>();
                    if input_note_expression_pattern(ne, info).is_none() {
                        continue;
                    }
                    let exact = exact_note_expression(sample_offset, ne);
                    push_note_expressions(scr, info, ne, sample_offset, exact);
                }
                _ => {
                    // Unsupported event type (system real-time, utility)
                    // - skip silently.
                }
            }
        }

        if sort {
            scr.event_list.ensure_sorted_by_offset();
        }
    }
}
