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

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use moose_params::{MidiSource, PersistChangeSignal};

use crate::thread::MainThread;

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
    }
}

/// Sends [`HostNotice`]s to the host. Cloning shares one notifier.
///
/// Notices coalesce: several before the wrapper's next main-thread callback
/// reach the host once. Formats without a matching call drop them.
#[derive(Clone)]
pub struct HostNotifier(Arc<NotifierInner>);

struct NotifierInner {
    pending: AtomicU8,
    wake: PersistChangeSignal,
}

impl HostNotifier {
    /// A notifier whose `wake` asks the host for a main-thread callback,
    /// where the wrapper calls [`Self::take`]. `wake` runs on the notifying
    /// thread. Format wrappers build this.
    #[doc(hidden)]
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(NotifierInner {
            pending: AtomicU8::new(0),
            wake: PersistChangeSignal::new(wake),
        }))
    }

    /// Tell the host `notice` changed. Callable from any thread except the
    /// audio thread: the wake may lock and call into the host.
    pub fn notify(&self, notice: HostNotice) {
        self.0.pending.fetch_or(notice.bits(), Ordering::AcqRel);
        self.0.wake.notify();
    }

    /// The notices since the last call. Wrappers call this on the host's
    /// main thread.
    #[doc(hidden)]
    #[must_use]
    pub fn take(&self) -> HostNotice {
        HostNotice::from_bits_truncate(self.0.pending.swap(0, Ordering::AcqRel))
    }

    /// Stop waking the host, after any wake in flight returns. Wrappers
    /// call this before the instance is freed; later notices stay pending.
    #[doc(hidden)]
    pub fn close(&self) {
        self.0.wake.close();
    }
}

impl fmt::Debug for HostNotifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostNotifier")
            .field(
                "pending",
                &HostNotice::from_bits_truncate(self.0.pending.load(Ordering::Relaxed)),
            )
            .finish_non_exhaustive()
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

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
        assert_eq!(wakes.load(Ordering::Relaxed), 3);
        assert_eq!(notifier.take(), HostNotice::all());
        assert_eq!(notifier.take(), HostNotice::empty());

        notifier.close();
        clone.notify(HostNotice::VOICE_INFO);
        assert_eq!(wakes.load(Ordering::Relaxed), 3, "a closed notifier wakes");
        assert_eq!(notifier.take(), HostNotice::VOICE_INFO);
    }
}
