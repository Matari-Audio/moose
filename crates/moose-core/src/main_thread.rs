//! The main-thread part of a plugin: what the host tells it and asks of it
//! outside the audio path. Wrappers keep one [`MainThreadHooks`] object per
//! instance in a [`MainThreadCell`](crate::thread::MainThreadCell) and call
//! it only from host main-thread callbacks; the audio thread never sees it.
//!
//! Every method has a default, so a plugin overrides only what it supports.
//! A wrapper maps each method to whatever its format offers (CLAP
//! `track-info`, `note-name`, `voice-info`, `param-indication`,
//! `context-menu`; VST3 `IInfoListener`, `IMidiLearn2`).
//!
//! The plugin tells the host its note names or voice info changed through a
//! [`HostNotifier`], handed to [`MainThreadHooks::connect`] and installed on
//! the editor's [`PluginContext`](crate::editor::PluginContext).

use std::sync::Arc;

use moose_params::MidiSource;

use crate::host_queue::{HostQueue, HostSignal};
use crate::thread::MainThread;

/// Plugin-held handle asking the host to resume calling `process` on an
/// instance the host may have put to sleep (CLAP `host.request_process`).
/// Obtained from [`crate::tasks::InitContext::process_waker`]; `None` for
/// formats with no such request (they keep processing anyway).
///
/// `Clone + Send + Sync`. [`Self::wake`] is lock- and allocation-free and
/// callable from any thread, including the audio thread and background
/// tasks; after the instance is destroyed it is a no-op.
#[derive(Clone)]
pub struct ProcessWaker(Arc<dyn Fn() + Send + Sync>);

impl ProcessWaker {
    /// Wrap a wrapper's wake hook. `wake` runs on the caller's thread and
    /// must not lock or allocate.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(wake))
    }

    /// Ask the host to process this instance again.
    pub fn wake(&self) {
        (self.0)();
    }
}

impl std::fmt::Debug for ProcessWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProcessWaker")
    }
}

bitflags::bitflags! {
    /// What the plugin tells the host changed, so it re-reads it.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct HostNotice: u8 {
        /// [`MainThreadHooks::note_names`] changed (CLAP
        /// `note_name.changed`).
        const NOTE_NAMES = 0b01;
        /// [`MainThreadHooks::voice_info`] changed (CLAP
        /// `voice_info.changed`).
        const VOICE_INFO = 0b10;
        /// The audio ports changed: which layouts are declared, their
        /// names or channel counts (CLAP `audio_ports.rescan`, VST3
        /// `kIoChanged`). An active plugin is restarted first; the host
        /// re-reads the ports while it is inactive.
        const AUDIO_PORTS = 0b100;
        /// [`MainThreadHooks::keyswitches`] changed (VST3
        /// `kKeyswitchChanged`).
        const KEYSWITCHES = 0b1000;
    }
}

/// Sends [`HostNotice`]s to the host: a handle on the instance's
/// [`HostQueue`]. Cloning shares the queue.
///
/// Notices coalesce: several before the wrapper's next main-thread callback
/// reach the host once. Formats without a matching call drop them.
#[derive(Clone, Debug)]
pub struct HostNotifier(Arc<HostQueue>);

impl HostNotifier {
    /// A notifier on its own queue, whose `wake` asks the host for a
    /// main-thread callback where the wrapper calls [`Self::take`]. See
    /// [`HostQueue::new`] for what `wake` may do.
    #[doc(hidden)]
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(HostQueue::new(wake)))
    }

    /// A notifier on the wrapper's per-instance queue. Format wrappers build
    /// this and drain the queue themselves.
    #[doc(hidden)]
    #[must_use]
    pub fn from_queue(queue: Arc<HostQueue>) -> Self {
        Self(queue)
    }

    /// Tell the host `notice` changed. Any thread except the audio thread
    /// (a wrapper's wake may not be audio-safe).
    pub fn notify(&self, notice: HostNotice) {
        self.0.signal(notice);
    }

    /// The notices since the last call, leaving the queue's other signals.
    /// Wrappers call this on the host's main thread.
    #[doc(hidden)]
    #[must_use]
    pub fn take(&self) -> HostNotice {
        self.0.take(HostSignal::from(HostNotice::all())).notices()
    }

    /// Stop waking the host, after any wake in flight returns. Wrappers
    /// call this before the instance is freed; later notices stay pending.
    #[doc(hidden)]
    pub fn close(&self) {
        self.0.close();
    }
}

/// A MIDI 2 per-note-agnostic controller (VST3 `Midi2Controller`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Midi2ControllerKind {
    /// A registered controller (RPN).
    Registered,
    /// An assignable controller (NRPN).
    Assignable,
}

/// One MIDI 2 controller. `bank` and `index` are 7-bit (`0..=127`); the
/// wrappers drop input outside that range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Midi2Controller {
    pub kind: Midi2ControllerKind,
    pub bank: u8,
    pub index: u8,
}

/// One MIDI 2.0 controller-to-parameter assignment advertised to a host
/// (VST3 SDK 3.8 `IMidiMapping2::getMidi2ControllerAssignments`).
///
/// `bus_index` must be non-negative, `channel` must be `0..=15`, and
/// `controller.bank` / `controller.index` must be 7-bit (`0..=127`); entries
/// outside those ranges are ignored by the wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Midi2ControllerAssignment {
    /// Target parameter ID.
    pub param_id: u32,
    /// Event bus index (`0` for the primary MIDI bus).
    pub bus_index: i32,
    /// MIDI channel (`0..=15`).
    pub channel: u8,
    /// MIDI 2.0 Registered (RPN) or Assignable (NRPN) controller.
    pub controller: Midi2Controller,
}

/// The controller a host reports during MIDI learn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LearnSource {
    /// A MIDI 1 controller (VST3 `IMidiLearn`).
    Midi1(MidiSource),
    /// A MIDI 2 controller (VST3 `IMidiLearn2`).
    Midi2(Midi2Controller),
}

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

/// Combined host indication snapshot for one parameter: its current automation
/// state/colour plus any mapped host controller (`clap.param-indication/4`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParamIndicationState {
    /// Current automation lane state (`None`, `Present`, `Playing`, `Recording`, `Overriding`).
    pub automation: AutomationState,
    /// Host-supplied automation lane colour, if any.
    pub automation_color: Option<HostColor>,
    /// Active host controller/macro mapping, if any.
    pub mapping: Option<HostMapping>,
}

impl ParamIndicationState {
    /// Apply one incremental [`ParamIndication`] update from the host.
    /// Returns `true` when the state actually changed.
    pub fn apply(&mut self, indication: &ParamIndication) -> bool {
        match indication {
            ParamIndication::Mapping(mapping) => {
                if &self.mapping == mapping {
                    false
                } else {
                    self.mapping.clone_from(mapping);
                    true
                }
            }
            ParamIndication::Automation { state, color } => {
                if self.automation == *state && self.automation_color == *color {
                    false
                } else {
                    self.automation = *state;
                    self.automation_color = *color;
                    true
                }
            }
        }
    }
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

/// Direction of an audio bus reported by the host (e.g. VST3
/// `IAudioPresentationLatency`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusDirection {
    Input,
    Output,
}

/// Articulation trigger kind exposed through VST3 `IKeyswitchController`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum KeyswitchKind {
    /// Triggered by a note-on event (`kNoteOnKeyswitchTypeID`).
    #[default]
    NoteOn,
    /// Triggered while playing (`kOnTheFlyKeyswitchTypeID`).
    OnTheFly,
    /// Triggered on note release (`kOnReleaseKeyswitchTypeID`).
    OnRelease,
    /// Covers a key range (`kKeyRangeTypeID`).
    KeyRange,
}

/// One keyswitch articulation advertised to the host (`IKeyswitchController`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyswitch {
    pub kind: KeyswitchKind,
    pub title: String,
    pub short_title: String,
    /// Lowest triggering MIDI key (`0..=127`).
    pub min_key: u8,
    /// Highest triggering MIDI key (`0..=127`).
    pub max_key: u8,
    /// Optional remapped key (`-1` when not remapped).
    pub remapped_key: i32,
    /// Unit ID this keyswitch applies to (`0` for the root unit).
    pub unit_id: i32,
}

/// One program-list entry exposed via VST3 `IUnitInfo` + `IProgramListData`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramPreset {
    pub name: String,
    pub category: String,
    /// Serialized MOOSE state envelope bytes for this program.
    pub state: Vec<u8>,
}

/// The main-thread part of a plugin. See the module docs.
pub trait MainThreadHooks: Send + 'static {
    /// The wrapper's [`HostNotifier`], passed once after the hooks are
    /// built. Keep it to tell the host when note names or voice info change.
    fn connect(&mut self, _notifier: HostNotifier) {}

    /// The host's track changed (name, colour, channel count).
    fn track_info_changed(&mut self, _cx: &MainThread, _info: &TrackInfo) {}

    /// Key names for the host's note editor. Ask the host to re-read them
    /// with [`HostNotice::NOTE_NAMES`].
    fn note_names(&self, _cx: &MainThread) -> Vec<NoteName> {
        Vec::new()
    }

    /// Articulation keyswitches for `(bus, channel)` (VST3
    /// `IKeyswitchController`). Ask the host to re-read them with
    /// [`HostNotice::KEYSWITCHES`].
    fn keyswitches(&self, _cx: &MainThread, _bus: u32, _channel: u8) -> Vec<Keyswitch> {
        Vec::new()
    }

    /// Host-visible program list exposed via VST3 `IUnitInfo` and
    /// `IProgramListData`. Returning an empty list keeps `getProgramListCount`
    /// at `0`.
    fn program_list(&self, _cx: &MainThread) -> Vec<ProgramPreset> {
        Vec::new()
    }

    /// Replace the stored data for program `index` (VST3
    /// `IProgramListData::setProgramData` / `IUnitInfo::setUnitProgramData`).
    /// Return `true` if the bank accepted the update, or `false` if the bank
    /// is immutable or `index` is out of range.
    fn set_program_data(&mut self, _cx: &MainThread, _index: usize, _data: &[u8]) -> bool {
        false
    }

    /// Voice usage; `None` when the plugin doesn't report it. Tell the host
    /// it changed with [`HostNotice::VOICE_INFO`].
    fn voice_info(&self, _cx: &MainThread) -> Option<VoiceInfo> {
        None
    }

    /// Live MIDI controller input on `channel` (`0..=15`) while the host is
    /// in MIDI-learn mode. Return `true` if the plugin bound it to something
    /// (VST3 `IMidiLearn2`).
    fn midi_learn(
        &mut self,
        _cx: &MainThread,
        _bus: u32,
        _channel: u8,
        _source: LearnSource,
    ) -> bool {
        false
    }

    /// MIDI 2.0 controller-to-parameter assignments for `direction` (VST3 SDK
    /// 3.8 `IMidiMapping2`). Default: empty (no dynamic MIDI 2.0 assignments).
    fn midi2_controller_assignments(
        &self,
        _cx: &MainThread,
        _direction: BusDirection,
    ) -> Vec<Midi2ControllerAssignment> {
        Vec::new()
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

    /// Map a parameter ID from an older or replaced plugin class (`old_plugin_cid`)
    /// to a compatible parameter ID in this plugin (VST3 `IRemapParamID`).
    /// Return `None` when no compatible parameter exists.
    fn remap_param_id(
        &self,
        _cx: &MainThread,
        _old_plugin_cid: [u8; 16],
        _old_id: u32,
    ) -> Option<u32> {
        None
    }

    /// The host reported upstream/downstream presentation latency in samples
    /// for one audio bus (VST3 `IAudioPresentationLatency`). Useful for
    /// visualizers and meters that align UI frames with audible output.
    fn audio_presentation_latency_changed(
        &mut self,
        _cx: &MainThread,
        _direction: BusDirection,
        _bus_index: u32,
        _latency_samples: u32,
    ) {
    }
}

/// No main-thread part.
impl MainThreadHooks for () {}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn notices_coalesce_and_wake_until_closed() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let notifier = HostNotifier::new({
            let wakes = Arc::clone(&wakes);
            move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }
        });
        let clone = notifier.clone();
        notifier.notify(HostNotice::NOTE_NAMES);
        clone.notify(HostNotice::VOICE_INFO);
        clone.notify(HostNotice::NOTE_NAMES);
        assert_eq!(wakes.load(Ordering::Relaxed), 1, "one wake until a drain");
        assert_eq!(
            notifier.take(),
            HostNotice::NOTE_NAMES | HostNotice::VOICE_INFO
        );
        assert_eq!(notifier.take(), HostNotice::empty());
        clone.notify(HostNotice::VOICE_INFO);
        assert_eq!(wakes.load(Ordering::Relaxed), 2, "take re-arms the wake");
        assert_eq!(notifier.take(), HostNotice::VOICE_INFO);

        notifier.close();
        clone.notify(HostNotice::VOICE_INFO);
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            2,
            "a closed notifier never wakes"
        );
        assert_eq!(notifier.take(), HostNotice::VOICE_INFO);
    }
}
