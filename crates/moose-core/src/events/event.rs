//! Typed events and the transport snapshot.

/// A timestamped event within a process block.
///
/// `Copy` because every [`EventBody`] variant is POD - lets the
/// audio path move events without per-event clones.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    /// Sample offset within the block (`0..num_samples`).
    pub sample_offset: u32,
    /// MIDI port this event arrived on / goes out on (0-based). Single-
    /// port plugins - the vast majority - always see `0` and can ignore
    /// it. A plugin that declares more than one MIDI port (see
    /// `PluginInfo::midi_input_ports` / `midi_output_ports`) filters
    /// inbound events by `port` and stamps outbound ones with the port
    /// they should leave on. Formats without a multi-port MIDI transport
    /// clamp everything to `0`.
    pub port: u8,
    pub body: EventBody,
}

impl Event {
    /// Event on the default MIDI port (`0`). The common constructor -
    /// single-port plugins and every non-MIDI event use this.
    #[must_use]
    pub fn new(sample_offset: u32, body: EventBody) -> Self {
        Self {
            sample_offset,
            port: 0,
            body,
        }
    }

    /// Event addressed to / from a specific MIDI port. Only meaningful
    /// for plugins that declared more than one MIDI port; wrappers on
    /// single-port formats route it to port `0` regardless.
    #[must_use]
    pub fn on_port(sample_offset: u32, port: u8, body: EventBody) -> Self {
        Self {
            sample_offset,
            port,
            body,
        }
    }
}

/// A validated raw MIDI 1.0 short message.
///
/// The length is part of the value because system-common messages may use
/// fewer than three bytes. `SysEx` remains in [`EventBody::SysEx`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawMidi1 {
    bytes: [u8; 3],
    len: u8,
}

impl RawMidi1 {
    /// Construct a one-, two-, or three-byte MIDI 1.0 message.
    #[must_use]
    pub fn new(bytes: [u8; 3], len: u8) -> Option<Self> {
        (1..=3).contains(&len).then_some(Self { bytes, len })
    }

    /// The meaningful bytes in this message.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Original three-byte storage, including trailing bytes outside the
    /// semantic message length. Useful for fixed-width host transports.
    #[must_use]
    pub fn storage(&self) -> &[u8; 3] {
        &self.bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

/// A validated Universal MIDI Packet containing one to four 32-bit words.
///
/// No message-type filtering is performed: utility, system, data, flex-data,
/// stream, and future packets remain byte-for-byte observable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawUmp {
    words: [u32; 4],
    word_count: u8,
}

impl RawUmp {
    /// Construct a UMP with its original packet length.
    #[must_use]
    pub fn new(words: [u32; 4], word_count: u8) -> Option<Self> {
        (1..=4)
            .contains(&word_count)
            .then_some(Self { words, word_count })
    }

    /// The meaningful words in this packet.
    #[must_use]
    pub fn words(&self) -> &[u32] {
        &self.words[..usize::from(self.word_count)]
    }

    /// Original four-word storage, including words outside the packet length.
    #[must_use]
    pub fn storage(&self) -> &[u32; 4] {
        &self.words
    }

    #[must_use]
    pub fn word_count(&self) -> usize {
        usize::from(self.word_count)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventBody {
    // -- MIDI 1.0 channel voice (wire-native 7-bit / 14-bit) --
    /// Note on. MIDI 1.0 quirk: a `NoteOn` with `velocity == 0` is
    /// a `NoteOff`. Format wrappers normalize that at parse time so
    /// plugin code can match `NoteOn` without checking velocity.
    NoteOn {
        group: u8,
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOff {
        group: u8,
        channel: u8,
        note: u8,
        velocity: u8,
    },
    /// Polyphonic key pressure (per-note aftertouch).
    Aftertouch {
        group: u8,
        channel: u8,
        note: u8,
        pressure: u8,
    },
    ChannelPressure {
        group: u8,
        channel: u8,
        pressure: u8,
    },
    ControlChange {
        group: u8,
        channel: u8,
        cc: u8,
        value: u8,
    },
    /// 14-bit pitch bend, raw code `0..=16383`. `8192` is center.
    /// See `moose_utils::midi::norm_pitch_bend` for the
    /// asymmetric-range conversion helper.
    PitchBend {
        group: u8,
        channel: u8,
        value: u16,
    },
    ProgramChange {
        group: u8,
        channel: u8,
        program: u8,
    },

    // -- MIDI 2.0 channel voice (wire-native 16/32-bit) --
    /// MIDI 2.0 `NoteOn`. `velocity` is `0..=65535`; unlike MIDI 1.0,
    /// a zero velocity is a genuine zero (`NoteOff` is its own
    /// dedicated message). `attribute_type` indicates how
    /// `attribute` should be interpreted: 0 = no attribute, 1 =
    /// manufacturer-specific, 2 = profile-specific, 3 = Pitch 7.9.
    NoteOn2 {
        group: u8,
        channel: u8,
        note: u8,
        velocity: u16,
        attribute_type: u8,
        attribute: u16,
    },
    NoteOff2 {
        group: u8,
        channel: u8,
        note: u8,
        velocity: u16,
        attribute_type: u8,
        attribute: u16,
    },
    /// MIDI 2.0 polyphonic key pressure (`pressure: u32`).
    PolyPressure2 {
        group: u8,
        channel: u8,
        note: u8,
        pressure: u32,
    },
    /// MIDI 2.0 per-note controller. `registered = true` for
    /// Registered Per-Note (RPN-like indexed list); `false` for
    /// Assignable Per-Note (free-form per-controller mapping).
    PerNoteCC {
        group: u8,
        channel: u8,
        note: u8,
        cc: u8,
        value: u32,
        registered: bool,
    },
    /// MIDI 2.0 per-note pitch bend (`value: u32`). `0x8000_0000`
    /// is center; full-scale is ±48 semitones
    /// ([`crate::midi::PER_NOTE_TUNING_SEMITONES`]) wherever a
    /// wrapper maps it onto a semitone-denominated host domain.
    PerNotePitchBend {
        group: u8,
        channel: u8,
        note: u8,
        value: u32,
    },
    /// MIDI 2.0 per-note management flags. Bit 0 = detach
    /// per-note controllers from active note; bit 1 = reset
    /// (set) per-note controllers to default values.
    PerNoteManagement {
        group: u8,
        channel: u8,
        note: u8,
        flags: u8,
    },
    /// MIDI 2.0 channel-wide control change (32-bit).
    ControlChange2 {
        group: u8,
        channel: u8,
        cc: u8,
        value: u32,
    },
    /// MIDI 2.0 channel pressure (32-bit aftertouch on the whole
    /// channel).
    ChannelPressure2 {
        group: u8,
        channel: u8,
        pressure: u32,
    },
    /// MIDI 2.0 channel pitch bend (32-bit). `0x8000_0000` is
    /// center.
    PitchBend2 {
        group: u8,
        channel: u8,
        value: u32,
    },
    /// MIDI 2.0 program change. Optional bank pair (MSB, LSB);
    /// MIDI 2.0's "B" flag is encoded as `Some` / `None`. When
    /// `None`, the host hasn't selected a bank and the program
    /// applies in the current bank.
    ProgramChange2 {
        group: u8,
        channel: u8,
        program: u8,
        bank: Option<(u8, u8)>,
    },
    /// MIDI 2.0 Registered Controller (the spec's RPN replacement,
    /// 32-bit). `bank` and `index` are the two 7-bit identifiers
    /// the spec reserves for Registered Parameter Numbers.
    RegisteredController {
        group: u8,
        channel: u8,
        bank: u8,
        index: u8,
        value: u32,
    },
    /// MIDI 2.0 Assignable Controller (the spec's NRPN
    /// replacement, 32-bit). `bank` and `index` are
    /// manufacturer-defined.
    AssignableController {
        group: u8,
        channel: u8,
        bank: u8,
        index: u8,
        value: u32,
    },

    // -- moose-internal automation --
    ParamChange {
        id: u32,
        value: f64,
    },
    /// Parameter modulation offset (CLAP-specific, zero on other
    /// formats). Effective value is `base + value`. The base value
    /// is unchanged.
    ParamMod {
        id: u32,
        note_id: i32,
        value: f64,
    },

    // -- Transport --
    Transport(TransportInfo),

    // -- System layer --
    /// System Exclusive (`SysEx`) message - MIDI 1.0 and MIDI 2.0
    /// alike. The payload bytes live in [`EventList::sysex_bytes`];
    /// resolve a body to its slice with
    /// `event_list.sysex_bytes(&body)` rather than indexing the
    /// pool directly. The bytes are the inner `SysEx` data
    /// **without** the leading `0xF0` start byte or trailing `0xF7`
    /// end byte - format wrappers strip those at the boundary so
    /// plugin code doesn't have to.
    ///
    /// Inlining the bytes in the variant would balloon every event's
    /// footprint to the worst-case (~64 KiB) - channel-voice events
    /// are <8 bytes today and we want to keep the per-event memory
    /// pressure on the audio thread proportional to that. The
    /// indices-into-a-pool layout pays the price (two-step access)
    /// for the `SysEx`-handling path only.
    SysEx {
        pool_offset: u32,
        len: u32,
    },
}

/// Host-populated transport snapshot. Constructed by every format
/// wrapper from the host's own transport struct via struct-literal
/// expressions, so this stays "exhaustive" (no `#[non_exhaustive]`,
/// which would block cross-crate construction). Adding a new field
/// is a coordinated workspace-wide change.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TransportInfo {
    pub playing: bool,
    pub recording: bool,
    pub tempo: f64,
    pub time_sig_num: u8,
    pub time_sig_den: u8,
    pub position_samples: i64,
    pub position_seconds: f64,
    pub position_beats: f64,
    pub bar_start_beats: f64,
    pub loop_active: bool,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
}

impl TransportInfo {
    /// Synthetic transport for snapshot tests - playing at 120 BPM,
    /// 4/4, position 4.0 beats. Used as the default by every snapshot
    /// helper (`moose-egui`, `moose-slint`, `moose-iced`,
    /// `moose-test`) so that transport-aware widgets render a
    /// populated readout in marketing screenshots instead of a
    /// `(no host transport)` placeholder.
    #[must_use]
    pub fn for_screenshot() -> Self {
        Self {
            playing: true,
            tempo: 120.0,
            time_sig_num: 4,
            time_sig_den: 4,
            position_beats: 4.0,
            // 4 beats at 120 BPM is 2.0 s = 96000 samples at 48 kHz;
            // keeps the sample + beat positions consistent in readouts.
            position_samples: 96_000,
            ..Self::default()
        }
    }
}
