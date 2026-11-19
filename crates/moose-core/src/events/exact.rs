//! Exact host-native event payloads and the borrowed views over them.

use super::{
    Event, EventBody, ExactDeliveryCarrier, ExactDeliveryError, ExactPendingDelivery, RawMidi1,
    RawUmp, StoredTypedEvent,
};

/// One component of a host-native event address.
///
/// Hosts commonly use `-1` as a wildcard while carrying concrete values in
/// signed fields. [`Self::InvalidRaw`] keeps every other out-of-domain value
/// observable instead of accidentally turning hostile input into a wildcard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExactAddress<T> {
    #[default]
    Wildcard,
    Value(T),
    InvalidRaw(i32),
}

impl<T> ExactAddress<T>
where
    T: Copy + Into<i32>,
{
    #[must_use]
    pub fn is_wildcard(self) -> bool {
        matches!(self, Self::Wildcard)
    }

    #[must_use]
    pub fn value(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Wildcard | Self::InvalidRaw(_) => None,
        }
    }

    #[must_use]
    pub fn invalid_raw(self) -> Option<i32> {
        match self {
            Self::InvalidRaw(raw) => Some(raw),
            Self::Wildcard | Self::Value(_) => None,
        }
    }

    /// Reconstruct the signed host value without erasing invalid input.
    #[must_use]
    pub fn raw_i32(self) -> i32 {
        match self {
            Self::Wildcard => -1,
            Self::Value(value) => value.into(),
            Self::InvalidRaw(raw) => raw,
        }
    }
}

/// Format-neutral note addressing with lossless signed-host classification.
/// Concrete channels are `0..=15`, keys are `0..=127`, and `-1` is the
/// wildcard on every axis. Other raw signed values remain distinguishable as
/// [`ExactAddress::InvalidRaw`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExactNoteAddress {
    pub port: ExactAddress<u16>,
    pub channel: ExactAddress<u8>,
    pub key: ExactAddress<u8>,
    pub note_id: ExactAddress<i32>,
}

impl ExactNoteAddress {
    /// Classify raw signed host fields without narrowing invalid values.
    #[must_use]
    pub fn from_raw_signed(port: i16, channel: i16, key: i16, note_id: i32) -> Self {
        Self {
            port: classify_port(i32::from(port)),
            channel: classify_u8_axis(channel, 15),
            key: classify_u8_axis(key, 127),
            note_id: match note_id {
                -1 => ExactAddress::Wildcard,
                0.. => ExactAddress::Value(note_id),
                invalid => ExactAddress::InvalidRaw(invalid),
            },
        }
    }

    /// Classify VST3's signed note fields without discarding plug-in-owned
    /// negative note identifiers. VST3 reserves only `-1` for "unavailable";
    /// every other `i32` is a concrete identifier.
    #[must_use]
    pub fn from_vst3_signed(port: i32, channel: i16, key: i16, note_id: i32) -> Self {
        Self {
            port: classify_port(port),
            channel: classify_u8_axis(channel, 15),
            key: classify_u8_axis(key, 127),
            note_id: if note_id == -1 {
                ExactAddress::Wildcard
            } else {
                ExactAddress::Value(note_id)
            },
        }
    }
}

fn classify_port(raw: i32) -> ExactAddress<u16> {
    if raw == -1 {
        ExactAddress::Wildcard
    } else if let Ok(value) = u16::try_from(raw) {
        ExactAddress::Value(value)
    } else {
        ExactAddress::InvalidRaw(raw)
    }
}

fn classify_u8_axis(raw: i16, max: u8) -> ExactAddress<u8> {
    if raw == -1 {
        ExactAddress::Wildcard
    } else if let Ok(value) = u8::try_from(raw)
        && value <= max
    {
        ExactAddress::Value(value)
    } else {
        ExactAddress::InvalidRaw(i32::from(raw))
    }
}

/// A note lifecycle operation which may not have a MIDI byte equivalent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExactNoteKind {
    On,
    Off,
    Choke,
    End,
}

/// Lossless, format-neutral event payloads kept alongside the convenient
/// [`EventBody`] representation.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum ExactEventBody {
    Midi1 {
        port: u16,
        message: RawMidi1,
    },
    Ump {
        port: u16,
        packet: RawUmp,
    },
    /// System Exclusive payload. Bytes remain in the owning [`super::EventList`]'s
    /// bounded pool and are exposed through [`ExactEventRef::sysex_bytes`].
    SysEx {
        port: u16,
    },
    Note {
        kind: ExactNoteKind,
        address: ExactNoteAddress,
        velocity: f64,
    },
    NoteExpression {
        expression_id: i32,
        address: ExactNoteAddress,
        value: f64,
    },
    /// A host-native note whose continuous fields must survive without MIDI
    /// velocity quantization. `length` is present only when the source format
    /// carries one on note-on.
    DetailedNote {
        kind: ExactNoteKind,
        address: ExactNoteAddress,
        velocity: f32,
        tuning: f32,
        length: Option<i32>,
    },
    /// Host-native polyphonic pressure with its original voice identifier.
    DetailedPolyPressure {
        address: ExactNoteAddress,
        pressure: f32,
    },
    /// Absolute normalized per-note expression with a full-width type ID.
    NormalizedNoteExpression {
        expression_id: u32,
        address: ExactNoteAddress,
        value: f64,
    },
}

/// Format-neutral provenance hints attached to an exact event.
///
/// These are semantic qualifiers shared by host formats, not opaque wrapper
/// flag bits. Unknown format-specific bits stay at their boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExactEventQualifiers {
    /// The event came directly from a live performer or controller.
    pub is_live: bool,
    /// The host should not write the event into recorded automation/MIDI.
    pub dont_record: bool,
}

/// VST3-only fields which have no format-neutral interpretation.
///
/// Construction rejects non-finite musical positions so an exact event can
/// never carry a value which is invalid at the VST3 boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst3EventMetadata {
    ppq_position: f64,
    raw_flags: u16,
}

/// Audio Unit UMP stream provenance needed to replay a native packet without
/// changing the self-described `MIDIEventList` protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuEventMetadata {
    protocol: u8,
}

impl AuEventMetadata {
    /// Construct metadata for `CoreMIDI` protocol 1 (MIDI 1.0 UMP) or 2
    /// (MIDI 2.0 UMP). Other values are not valid `MIDIProtocolID`s here.
    #[must_use]
    pub fn new(protocol: u8) -> Option<Self> {
        matches!(protocol, 1 | 2).then_some(Self { protocol })
    }

    #[must_use]
    pub fn protocol(self) -> u8 {
        self.protocol
    }
}

impl Vst3EventMetadata {
    #[must_use]
    pub fn new(ppq_position: f64, raw_flags: u16) -> Option<Self> {
        ppq_position.is_finite().then_some(Self {
            ppq_position,
            raw_flags,
        })
    }

    #[must_use]
    pub fn ppq_position(self) -> f64 {
        self.ppq_position
    }

    #[must_use]
    pub fn raw_flags(self) -> u16 {
        self.raw_flags
    }
}

/// Opaque source-format metadata attached to an exact event.
///
/// Wrappers must consume only their own variant. A different source format is
/// unsupported rather than silently translating or discarding opaque fields.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[non_exhaustive]
pub enum ExactEventMetadata {
    #[default]
    None,
    Vst3(Vst3EventMetadata),
    Au(AuEventMetadata),
}

impl ExactEventMetadata {
    #[must_use]
    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

/// A timestamped lossless event. Exact-only events have no [`EventBody`]
/// fallback; linked events expose one through [`ExactEventRef::fallback`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExactEvent {
    pub(super) sample_offset: u32,
    qualifiers: ExactEventQualifiers,
    metadata: ExactEventMetadata,
    pub body: ExactEventBody,
}

impl ExactEvent {
    #[must_use]
    pub fn new(sample_offset: u32, body: ExactEventBody) -> Self {
        Self {
            sample_offset,
            qualifiers: ExactEventQualifiers::default(),
            metadata: ExactEventMetadata::None,
            body,
        }
    }

    #[must_use]
    pub fn with_qualifiers(mut self, qualifiers: ExactEventQualifiers) -> Self {
        self.qualifiers = qualifiers;
        self
    }

    #[must_use]
    pub fn with_metadata(mut self, metadata: ExactEventMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    #[must_use]
    pub fn sample_offset(&self) -> u32 {
        self.sample_offset
    }

    #[must_use]
    pub fn qualifiers(&self) -> ExactEventQualifiers {
        self.qualifiers
    }

    #[must_use]
    pub fn metadata(&self) -> ExactEventMetadata {
        self.metadata
    }
}

/// Borrowed view of an exact event and its optional typed fallback.
#[derive(Clone, Copy, Debug)]
pub struct ExactEventPayload<'a> {
    pub(super) exact: &'a ExactEvent,
    pub(super) fallback: Option<&'a Event>,
    pub(super) sysex_range: Option<(u32, u32)>,
    pub(super) typed: &'a [StoredTypedEvent],
    pub(super) first_companion: Option<u32>,
    pub(super) sysex_pool: &'a [u8],
    pub(super) emittable: bool,
}

/// Borrowed exact carrier. Payload encoding is deliberately separated from
/// delivery authority: `payload()` contains no receipt bank or permit.
#[derive(Clone, Copy, Debug)]
pub struct ExactEventRef<'a> {
    pub(super) payload: ExactEventPayload<'a>,
    pub(super) delivery: &'a ExactDeliveryCarrier,
}

/// Allocation-free merged view of exact events and typed events which do not
/// have an exact payload. Linked typed fallbacks appear only as [`Self::Exact`]
/// so an adapter can replay the lossless payload without emitting a duplicate.
#[derive(Clone, Copy, Debug)]
pub enum LosslessEventRef<'a> {
    Typed(&'a Event),
    Exact(ExactEventRef<'a>),
}

/// Allocation-free position in an [`super::EventList::lossless_iter`] traversal.
/// Wrappers keep one in their preallocated audio scratch when their native
/// ABI pulls events one at a time.
#[derive(Clone, Copy, Debug, Default)]
pub struct LosslessEventCursor {
    pub(super) event_order_index: usize,
    pub(super) exact_order_index: usize,
}

impl<'a> ExactEventPayload<'a> {
    /// Static eligibility only. This never reads delivery state.
    #[must_use]
    pub fn is_emittable(self) -> bool {
        self.emittable
    }

    #[must_use]
    pub fn body(self) -> &'a ExactEventBody {
        &self.exact.body
    }

    #[must_use]
    pub fn fallback(self) -> Option<&'a Event> {
        self.fallback
    }

    #[must_use]
    pub fn qualifiers(self) -> ExactEventQualifiers {
        self.exact.qualifiers
    }

    #[must_use]
    pub fn metadata(self) -> ExactEventMetadata {
        self.exact.metadata
    }

    /// Semantic-only typed companions suppressed by [`super::EventList::lossless_iter`].
    /// These remain visible through [`super::EventList::iter`] for typed plugins.
    pub fn companions(self) -> impl Iterator<Item = &'a Event> {
        let mut next = self.first_companion;
        core::iter::from_fn(move || {
            let index = next? as usize;
            let stored = self.typed.get(index)?;
            next = stored.companion_next;
            Some(&stored.event)
        })
    }

    /// Resolve an exact [`ExactEventBody::SysEx`] against the owning pool.
    #[must_use]
    pub fn sysex_bytes(self) -> &'a [u8] {
        self.sysex_bytes_checked().unwrap_or(&[])
    }

    /// Checked form of [`Self::sysex_bytes`]. `None` means the exact `SysEx`
    /// event has no valid payload in the owning list's bounded pool.
    #[must_use]
    pub fn sysex_bytes_checked(self) -> Option<&'a [u8]> {
        if !matches!(self.exact.body, ExactEventBody::SysEx { .. }) {
            return None;
        }
        let range = self.sysex_range.or_else(|| {
            let EventBody::SysEx { pool_offset, len } = self.fallback?.body else {
                return None;
            };
            Some((pool_offset, len))
        })?;
        let start = range.0 as usize;
        let end = start.checked_add(range.1 as usize)?;
        self.sysex_pool.get(start..end)
    }

    /// Effective timestamp. A linked exact payload follows the fallback's
    /// timestamp.
    #[must_use]
    pub fn sample_offset(self) -> u32 {
        self.fallback
            .map_or(self.exact.sample_offset, |event| event.sample_offset)
    }
}

impl<'a> ExactEventRef<'a> {
    #[must_use]
    pub fn payload(self) -> ExactEventPayload<'a> {
        self.payload
    }
    #[must_use]
    pub fn body(self) -> &'a ExactEventBody {
        self.payload.body()
    }
    #[must_use]
    pub fn fallback(self) -> Option<&'a Event> {
        self.payload.fallback()
    }
    #[must_use]
    pub fn qualifiers(self) -> ExactEventQualifiers {
        self.payload.qualifiers()
    }
    #[must_use]
    pub fn metadata(self) -> ExactEventMetadata {
        self.payload.metadata()
    }
    pub fn companions(self) -> impl Iterator<Item = &'a Event> {
        self.payload.companions()
    }
    #[must_use]
    pub fn sysex_bytes(self) -> &'a [u8] {
        self.payload.sysex_bytes()
    }
    #[must_use]
    pub fn sysex_bytes_checked(self) -> Option<&'a [u8]> {
        self.payload.sysex_bytes_checked()
    }
    #[must_use]
    pub fn sample_offset(self) -> u32 {
        self.payload.sample_offset()
    }
    /// Actual native drain ONLY. Pure payload preflight must not call this.
    /// The returned exclusive emission phase prevents negative finalization
    /// while the actual native sink is executing.
    ///
    /// # Errors
    /// Returns a delivery error if the permit is stale or cannot enter native emission.
    pub fn pending_delivery(self) -> Result<Option<ExactPendingDelivery<'a>>, ExactDeliveryError> {
        match self.delivery {
            ExactDeliveryCarrier::Untracked => Ok(None),
            ExactDeliveryCarrier::NonEmittable => Err(ExactDeliveryError::Stale),
            ExactDeliveryCarrier::Tracked(permit) => permit.begin_native().map(Some),
        }
    }
}
