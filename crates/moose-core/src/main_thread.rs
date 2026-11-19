//! The main-thread part of a plugin: what the host tells it and asks of it
//! outside the audio path. Wrappers keep one [`MainThreadHooks`] object per
//! instance in a [`MainThreadCell`](crate::thread::MainThreadCell) and call
//! it only from host main-thread callbacks; the audio thread never sees it.
//!
//! Every method has a default, so a plugin overrides only what it supports.
//! A wrapper maps each method to whatever its format offers (CLAP
//! `track-info`, `note-name`, `voice-info`, `param-indication`,
//! `context-menu`; VST3 `IInfoListener`, `IMidiLearn2`).

use moose_params::MidiSource;

use crate::thread::MainThread;

/// A colour the host shows for a track or a parameter mapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct HostColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// What kind of channel the plugin sits on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrackKind {
    #[default]
    Track,
    Bus,
    Return,
    Master,
}

/// The host's channel / track the plugin is on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TrackInfo {
    pub name: Option<String>,
    pub color: Option<HostColor>,
    /// The track's audio channel count, when the host says.
    pub audio_channels: Option<u32>,
    pub kind: TrackKind,
}

impl TrackInfo {
    /// Empty info; set the fields that are known.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// A name for one key (or every key) the host shows in its piano roll.
/// `None` fields mean "any".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteName {
    pub name: String,
    pub port: Option<u16>,
    pub key: Option<u8>,
    pub channel: Option<u8>,
}

/// The plugin's voice usage, for hosts that size their note handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceInfo {
    /// Voices the plugin plays now (or its fixed count).
    pub count: u32,
    /// The most it can play.
    pub capacity: u32,
    /// Whether two notes on one key/channel can sound at once (needs note ids).
    pub overlapping_notes: bool,
}

/// How the host's automation stands for a parameter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum AutomationState {
    #[default]
    None,
    Present,
    Playing,
    Recording,
    Overriding,
}

/// A host control (a controller knob, a macro) mapped to a parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostMapping {
    pub color: Option<HostColor>,
    pub label: String,
    pub description: String,
}

/// What the host wants a parameter's control to show.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParamIndication {
    /// A host control is mapped to it (`Some`), or no longer is (`None`).
    Mapping(Option<HostMapping>),
    /// Its automation state and the lane's colour.
    Automation {
        state: AutomationState,
        color: Option<HostColor>,
    },
}

/// What a context menu is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MenuTarget {
    /// The plugin as a whole.
    Global,
    /// One parameter, by host id.
    Param(u32),
}

/// One entry of a context menu. Submenus open with [`Self::Submenu`] and
/// close with [`Self::EndSubmenu`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MenuItem {
    /// A command; `action` comes back through [`MainThreadHooks::perform_menu`].
    Entry {
        label: String,
        action: u32,
        enabled: bool,
    },
    /// A checkable command.
    Check {
        label: String,
        action: u32,
        enabled: bool,
        checked: bool,
    },
    Separator,
    Submenu {
        label: String,
        enabled: bool,
    },
    EndSubmenu,
    Title {
        label: String,
    },
}

/// The main-thread part of a plugin. See the module docs.
pub trait MainThreadHooks: Send + 'static {
    /// The host's track changed (name, colour, channel count).
    fn track_info_changed(&mut self, _cx: &MainThread, _info: &TrackInfo) {}

    /// Key names for the host's note editor. Ask the host to re-read them
    /// with the wrapper's note-name changed notice.
    fn note_names(&self, _cx: &MainThread) -> Vec<NoteName> {
        Vec::new()
    }

    /// Voice usage; `None` when the plugin doesn't report it.
    fn voice_info(&self, _cx: &MainThread) -> Option<VoiceInfo> {
        None
    }

    /// Live MIDI controller input while the host is in MIDI-learn mode.
    /// Return `true` if the plugin bound it to something (VST3 `IMidiLearn2`).
    fn midi_learn(
        &mut self,
        _cx: &MainThread,
        _bus: u32,
        _channel: u8,
        _source: MidiSource,
    ) -> bool {
        false
    }

    /// The host's mapping or automation indication for parameter `id`.
    fn param_indication(&mut self, _cx: &MainThread, _id: u32, _indication: ParamIndication) {}

    /// Items the plugin adds to a context menu for `target`.
    fn menu_items(&self, _cx: &MainThread, _target: MenuTarget) -> Vec<MenuItem> {
        Vec::new()
    }

    /// The user picked the plugin's item `action` for `target`. Return
    /// `false` if `action` is unknown.
    fn perform_menu(&mut self, _cx: &MainThread, _target: MenuTarget, _action: u32) -> bool {
        false
    }
}

/// No main-thread part.
impl MainThreadHooks for () {}
