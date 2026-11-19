//! Per-instance state behind the COM layer's opaque context, and the note-id maps.

use crate::output_events::MidiMap;

use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::editor::Editor;
use moose_core::editor::EditorBuilder;
use moose_core::events::{EVENT_LIST_PREALLOC, EventList, LosslessEventCursor};
use moose_core::export::PluginExport;
use moose_core::main_thread::{HostNotifier, MainThreadHooks};
use moose_core::meters::MeterStore;
use moose_core::plugin::PluginRuntime;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::tasks::AnyTaskSpawner;
use moose_core::thread::MainThreadCell;
use moose_core::wrapper::{PluginCell, SharedPlugin};
use moose_params::{ParamRange, PersistChangeSignal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Instance wrapper
// ---------------------------------------------------------------------------

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
pub(crate) type StateLoadQueue = state::StateLoadQueue;

/// VST3 `RestartFlags::kLatencyChanged`. The audio thread flags this
/// through `moose_vst3_mark_restart` (an atomic bit-set, no host call);
/// the shim drains it via `IComponentHandler::restartComponent` on the
/// next host main-thread callback, so the actual notification lands on
/// the UI thread - never the render thread or a moose-owned thread.
pub(crate) const K_LATENCY_CHANGED: i32 = 8;

/// VST3 `RestartFlags::kParamValuesChanged`: values moved without the host
/// asking (a state load, or a queued state the audio thread applied), so
/// the host re-reads them.
pub(crate) const K_PARAM_VALUES_CHANGED: i32 = 4;
/// VST3 `RestartFlags::kIoChanged`: the buses changed.
pub(crate) const K_IO_CHANGED: i32 = 2;

pub(crate) struct Vst3Instance<P: PluginExport> {
    pub(crate) persist_signal: std::sync::Mutex<Option<Arc<PersistChangeSignal>>>,
    /// The bound `CatalogSignal` (same type as the persist signal).
    pub(crate) catalog_signal: std::sync::Mutex<Option<Arc<PersistChangeSignal>>>,
    /// The plugin's main-thread part; reached only from edit-controller
    /// callbacks, which VST3 runs on the UI thread.
    pub(crate) main_thread: MainThreadCell<Box<dyn MainThreadHooks>>,
    /// The plugin in the wrapper-standard ownership cell: the audio
    /// thread owns it per block, host lifecycle callbacks own it while
    /// processing is stopped, and the two never overlap. `cb_state_save`
    /// and the editor's `get_state` read the lock-free snapshot instead,
    /// so they never touch it. See `moose_core::wrapper::SharedPlugin`.
    pub(crate) plugin: SharedPlugin<P>,
    /// Stable handle to the params Arc, set once at instance creation.
    /// Host-thread callbacks (`cb_param_*`) read params through this
    /// handle so a param query never touches the plugin.
    /// Params are atomic-backed and `Sync`.
    pub(crate) params_arc: Arc<P::Params>,
    /// Shared meter storage, set once at instance creation. The
    /// editor's `get_meter` closure reads these atomic slots instead
    /// of the plugin instance.
    pub(crate) meter_store: Arc<MeterStore>,
    /// Lock-free custom-state slot the audio thread publishes
    /// into, read by `save_state` so a snapshot-capable plugin's
    /// save never touches the plugin. Cached on the instance.
    pub(crate) snapshot: Arc<SnapshotSlot>,
    /// Background-task spawner (`None` unless the plugin wired `tasks:`),
    /// cached at creation so the editor schedules without touching the plugin.
    pub(crate) task_spawner: Option<AnyTaskSpawner>,
    /// Lock-free editor factory, cached at creation - building
    /// the editor never touches the plugin (`--shell` rebuilds
    /// from the reloaded dylib, so GUI edits hot-reload).
    pub(crate) editor_builder: EditorBuilder<P::Params>,
    /// Full param-info cache for the chunker's `is_chunked(id)`
    /// lookup. Built once at `cb_create`; static for the life of
    /// the instance.
    pub(crate) param_infos: Arc<moose_params::ParamCatalog>,
    /// The automation mode from `moose.toml`'s `[automation]`
    /// table. Read at instance construction and passed to
    /// `chunked_process::process_chunked` every block.
    pub(crate) automation: moose_core::Automation,
    pub(crate) plugin_id_hash: u64,
    /// `true` between `setActive(true)` and `setActive(false)`.
    /// `cb_state_load` and `cb_state_save` read it to decide whether the
    /// audio thread will drain `pending_state`: if inactive, no
    /// `cb_process` runs, so the host thread applies the custom-state
    /// blob synchronously rather than leaving it stranded (which would
    /// let a following `getState` re-serialize stale extra state).
    /// Written only from `cb_set_active` (main thread); unlike
    /// `prepared`, it tracks deactivation too.
    pub(crate) active: AtomicBool,
    /// `true` between `setProcessing(true)` and `setProcessing(false)` /
    /// `setActive(false)`, and set by the first `process` for hosts that
    /// never call `setProcessing`. While `false` the host thread owns the
    /// plugin, so state save / load run synchronously there.
    pub(crate) processing: AtomicBool,
    /// Cached `(id, range)` pairs sorted by id. Built once in
    /// `cb_create` from `params().param_infos()`. Hosts call
    /// `cb_param_normalize` / `cb_param_denormalize` extremely often
    /// while reading automation; rebuilding the full `Vec<ParamInfo>`
    /// per call would heap-allocate on a tight host read path. Ranges
    /// are static for the life of the plugin instance, so caching is
    /// safe.
    pub(crate) param_ranges: Vec<(u32, ParamRange)>,
    /// Precomputed MIDI-controller bindings, sorted by param id, for the
    /// audio-thread bridge in `process_block`. Only params with a
    /// `midi_map` appear, so it's empty for the common no-mapping plugin
    /// and the per-change lookup short-circuits (`binary_search` on an
    /// empty slice is `O(1)`).
    pub(crate) midi_maps: Vec<(u32, MidiMap)>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    pub(crate) transport_slot: Arc<TransportSlot>,
    /// Bounded SPSC handoff for state loads. Host (`cb_state_load`)
    /// and editor (`set_state` callback) deserialize on their thread
    /// and push the result; the audio thread pops at the top of
    /// `cb_process` and calls [`state::apply_state`]
    /// under its exclusive `&mut plugin`. While inactive no `cb_process`
    /// runs, so the host thread drains it in `cb_state_save` /
    /// `cb_set_active` - the editor pushes unconditionally rather than
    /// entering the cell from its own (possibly third) thread.
    pub(crate) pending_state: Arc<StateLoadQueue>,
    /// Set by a panic in `process`; cleared by the next successful prepare.
    pub(crate) panic_latch: moose_core::wrapper::PanicLatch,
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()` reports. Updated by the audio thread (or `cb_reset`)
    /// so host-thread callbacks (`cb_get_latency`, `cb_get_tail`) read
    /// the value without forming a `&Inst.plugin` reference. Initial
    /// value is whatever the plugin reports immediately after `init()`.
    pub(crate) latency_cache: AtomicU32,
    pub(crate) tail_cache: AtomicU32,
    /// Collision-free hidden MIDI proxy IDs in logical
    /// `(port, channel, controller)` order. Allocated deterministically
    /// from the top of the host-safe parameter domain while skipping
    /// every real parameter ID.
    pub(crate) midi_proxy_ids: Vec<u32>,
    /// Last-seen values of the hidden MIDI proxy params (f64 bits), in
    /// the same logical order as `midi_proxy_ids`. Empty when the plugin
    /// doesn't accept MIDI input. Written by `cb_param_set_value` and
    /// read by `cb_param_get_value` - both host-thread; atomic for
    /// interior mutability through the shared `&Inst` those callbacks
    /// hold.
    pub(crate) midi_proxy_values: Vec<AtomicU64>,
    /// Content scale from `setContentScaleFactor` (f64 bits, `0` until the
    /// host sends one; ignored on macOS); converts the editor's logical size
    /// to physical pixels for `getSize`.
    /// GUI-thread-only, but atomic so the host-thread GUI callbacks and the
    /// `request_resize` closure reach it through the shared `&Inst` without a
    /// `&mut *ctx` - and so it stays outside the `gui` cell, which the
    /// resize closure would otherwise re-enter while `cb_gui_open` holds it.
    pub(crate) host_scale: AtomicU64,
    /// The open editor's own window scale (`Editor::window_scale`, f64 bits,
    /// `0` = none), refreshed by the size callbacks. Converts sizes while the
    /// host never sent a content scale, so `getSize` matches the child window
    /// the editor created at the monitor DPI / `Xft.dpi`.
    pub(crate) window_scale: AtomicU64,
    /// Editor resize the plugin requested through `PluginContext::request_resize`
    /// that landed back in a GUI callback (`onSize` → `cb_gui_set_size`)
    /// synchronously while the `gui` cell was already held - the wrapper stashes
    /// the physical `(w << 32) | h` here (both non-zero, `0` = none) instead of
    /// re-entering the cell, and `cb_gui_get_size` applies it on the next size
    /// query. GUI thread only; atomic for interior mutability through `&Inst`.
    pub(crate) pending_resize: AtomicU64,
    /// Audio + lifecycle-owned per-block scratch. Behind a `PluginCell` so
    /// every callback reaches it through a shared `&Vst3Instance` - never a
    /// whole-struct `&mut *ctx`, which would alias a concurrent host-thread
    /// `&*ctx` (param reads, GUI) and is UB under the aliasing model. The
    /// VST3 host contract serializes its owners (process, reset, setup,
    /// activate, and the per-block sysex / output-event callbacks that run
    /// within the process cycle), so the cell is never held twice at once.
    pub(crate) audio: PluginCell<Vst3Scratch<P>>,
    /// Main/UI-thread-owned editor state, behind a `PluginCell` for the same
    /// reason. Its owners - the GUI callbacks and `cb_state_load`'s
    /// editor-notify - all run on the host main thread; `cb_process` never
    /// touches it, so it can't overlap the audio thread.
    pub(crate) gui: PluginCell<Vst3Gui>,
    /// The hooks' notifier; see `host_notice_woke`.
    pub(crate) host_notifier: HostNotifier,
}

impl<P: PluginExport> Vst3Instance<P> {
    /// The scale between the host's physical pixels and the editor's logical
    /// size: the host's content scale, else the open editor's window scale,
    /// else `1.0`. Never both multiplied.
    pub(crate) fn host_scale(&self) -> f64 {
        self.reported_host_scale()
            .or_else(|| match self.window_scale.load(Ordering::Relaxed) {
                0 => None,
                bits => Some(f64::from_bits(bits)),
            })
            .unwrap_or(1.0)
    }

    /// The last content scale the host sent, if it ever did.
    pub(crate) fn reported_host_scale(&self) -> Option<f64> {
        match self.host_scale.load(Ordering::Relaxed) {
            0 => None,
            bits => Some(f64::from_bits(bits)),
        }
    }

    /// Cache `editor`'s window scale for [`Self::host_scale`].
    pub(crate) fn note_window_scale(&self, editor: &dyn Editor) {
        let bits = editor
            .window_scale()
            .filter(|s| s.is_finite() && *s > 0.0)
            .map_or(0, f64::to_bits);
        self.window_scale.store(bits, Ordering::Relaxed);
    }

    pub(crate) fn set_host_scale(&self, scale: f64) {
        self.host_scale.store(scale.to_bits(), Ordering::Relaxed);
    }
}

/// Audio + lifecycle-owned per-block scratch (see [`Vst3Instance::audio`]).
pub(crate) struct Vst3Scratch<P: PluginExport> {
    pub(crate) event_list: EventList,
    pub(crate) input_num_frames: u32,
    pub(crate) output_events: EventList,
    /// Per-sub-block scratch for `chunked_process::process_chunked`.
    pub(crate) sub_event_scratch: EventList,
    pub(crate) sample_rate: f64,
    /// Max block size declared by the host in `setupProcessing`; used to
    /// debug-assert `cb_process` never exceeds the sized block.
    pub(crate) max_block_size: usize,
    /// `true` once `cb_reset` ran (host `setActive(true)`); `cb_process`
    /// early-returns and zeros outputs until then.
    pub(crate) prepared: bool,
    /// Reused per-block scratch for `RawBufferScratch::build`, parameterized
    /// by `P::Sample` (widening path for `prelude64` plugins).
    pub(crate) scratch: RawBufferScratch<<P as PluginRuntime>::Sample>,
    pub(crate) output_cursor: LosslessEventCursor,
    pub(crate) output_preflight_status: u32,
    pub(crate) output_note_ids: OutputNoteIds,
    pub(crate) pending_output_mutation: PendingOutputMutation,
}

/// Main/UI-thread-owned editor state (see [`Vst3Instance::gui`]).
pub(crate) struct Vst3Gui {
    pub(crate) editor: Option<Box<dyn Editor>>,
}

pub(crate) const VST3_NOTE_ID_LOWER: i32 = -10_000;
pub(crate) const VST3_NOTE_ID_UPPER: i32 = -1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputSourceIdentity {
    Exact { bus: u16, note_id: i32 },
    Anonymous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputVoiceLifecycle {
    Active,
    Released,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputNoteSlot {
    pub(crate) used: bool,
    pub(crate) source: OutputSourceIdentity,
    pub(crate) vst3_note_id: i32,
    pub(crate) bus: u8,
    pub(crate) channel: u8,
    pub(crate) pitch: u8,
    pub(crate) lifecycle: OutputVoiceLifecycle,
    pub(crate) age: u64,
}

impl OutputNoteSlot {
    const FREE: Self = Self {
        used: false,
        source: OutputSourceIdentity::Anonymous,
        vst3_note_id: VST3_NOTE_ID_LOWER,
        bus: 0,
        channel: 0,
        pitch: 0,
        lifecycle: OutputVoiceLifecycle::Released,
        age: 0,
    };
}

#[derive(Clone, Copy)]
pub(crate) struct OutputNoteIds {
    pub(crate) slots: [OutputNoteSlot; EVENT_LIST_PREALLOC],
    pub(crate) next_id: i32,
    pub(crate) next_age: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PendingOutputMutation {
    None,
    Insert {
        index: usize,
        slot: OutputNoteSlot,
        next_id: i32,
    },
    Release {
        index: usize,
    },
}

impl OutputNoteIds {
    pub(crate) fn new() -> Self {
        Self {
            slots: [OutputNoteSlot::FREE; EVENT_LIST_PREALLOC],
            next_id: VST3_NOTE_ID_LOWER,
            next_age: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.slots = [OutputNoteSlot::FREE; EVENT_LIST_PREALLOC];
        self.next_id = VST3_NOTE_ID_LOWER;
        self.next_age = 0;
    }

    pub(crate) fn propose_note_on(
        &self,
        source: OutputSourceIdentity,
        bus: u8,
        channel: u8,
        pitch: u8,
    ) -> Option<(i32, PendingOutputMutation)> {
        if source != OutputSourceIdentity::Anonymous
            && self.slots.iter().any(|slot| {
                slot.used && slot.source == source && slot.lifecycle == OutputVoiceLifecycle::Active
            })
        {
            return None;
        }

        let index = self
            .slots
            .iter()
            .position(|slot| {
                source != OutputSourceIdentity::Anonymous
                    && slot.used
                    && slot.source == source
                    && slot.lifecycle == OutputVoiceLifecycle::Released
            })
            .or_else(|| self.slots.iter().position(|slot| !slot.used))
            .or_else(|| {
                self.slots
                    .iter()
                    .enumerate()
                    .filter(|(_, slot)| slot.lifecycle == OutputVoiceLifecycle::Released)
                    .min_by_key(|(_, slot)| slot.age)
                    .map(|(index, _)| index)
            })?;
        let (vst3_note_id, next_id) = self.available_id(index)?;
        let slot = OutputNoteSlot {
            used: true,
            source,
            vst3_note_id,
            bus,
            channel,
            pitch,
            lifecycle: OutputVoiceLifecycle::Active,
            age: self.next_age,
        };
        Some((
            vst3_note_id,
            PendingOutputMutation::Insert {
                index,
                slot,
                next_id,
            },
        ))
    }

    pub(crate) fn propose_note_off(
        &self,
        source: OutputSourceIdentity,
        bus: u8,
        channel: u8,
        pitch: u8,
    ) -> Option<(i32, PendingOutputMutation)> {
        let index = if source == OutputSourceIdentity::Anonymous {
            self.slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| {
                    slot.used
                        && slot.bus == bus
                        && slot.channel == channel
                        && slot.pitch == pitch
                        && slot.lifecycle == OutputVoiceLifecycle::Active
                })
                .min_by_key(|(_, slot)| slot.age)
                .map(|(index, _)| index)?
        } else {
            self.slots.iter().position(|slot| {
                slot.used && slot.source == source && slot.lifecycle == OutputVoiceLifecycle::Active
            })?
        };
        Some((
            self.slots[index].vst3_note_id,
            PendingOutputMutation::Release { index },
        ))
    }

    pub(crate) fn note_id_for_expression(
        &self,
        source: OutputSourceIdentity,
        bus: u8,
        channel: u8,
        pitch: u8,
    ) -> Option<i32> {
        let index = if source == OutputSourceIdentity::Anonymous {
            self.unique_pck(bus, channel, pitch, true)
                .or_else(|| self.unique_pck(bus, channel, pitch, false))?
        } else {
            self.slots
                .iter()
                .position(|slot| slot.used && slot.source == source)?
        };
        Some(self.slots[index].vst3_note_id)
    }

    pub(crate) fn unique_pck(
        &self,
        bus: u8,
        channel: u8,
        pitch: u8,
        active_only: bool,
    ) -> Option<usize> {
        let mut matches = self.slots.iter().enumerate().filter(|(_, slot)| {
            slot.used
                && slot.bus == bus
                && slot.channel == channel
                && slot.pitch == pitch
                && (!active_only || slot.lifecycle == OutputVoiceLifecycle::Active)
        });
        let (index, _) = matches.next()?;
        matches.next().is_none().then_some(index)
    }

    pub(crate) fn available_id(&self, replacing: usize) -> Option<(i32, i32)> {
        let span = i64::from(VST3_NOTE_ID_UPPER) - i64::from(VST3_NOTE_ID_LOWER) + 1;
        for offset in 0..span {
            let candidate = i64::from(self.next_id) + offset;
            let candidate = if candidate > i64::from(VST3_NOTE_ID_UPPER) {
                candidate - span
            } else {
                candidate
            };
            let candidate = i32::try_from(candidate).ok()?;
            if !self.slots.iter().enumerate().any(|(index, slot)| {
                index != replacing && slot.used && slot.vst3_note_id == candidate
            }) {
                let next_id = if candidate == VST3_NOTE_ID_UPPER {
                    VST3_NOTE_ID_LOWER
                } else {
                    candidate + 1
                };
                return Some((candidate, next_id));
            }
        }
        None
    }

    pub(crate) fn commit(&mut self, mutation: PendingOutputMutation) {
        match mutation {
            PendingOutputMutation::None => {}
            PendingOutputMutation::Insert {
                index,
                slot,
                next_id,
            } => {
                self.slots[index] = slot;
                self.next_id = next_id;
                self.next_age = self.next_age.wrapping_add(1);
            }
            PendingOutputMutation::Release { index } => {
                if let Some(slot) = self.slots.get_mut(index) {
                    slot.lifecycle = OutputVoiceLifecycle::Released;
                }
            }
        }
    }
}

#[cfg(test)]
#[repr(C)]
pub(crate) struct Vst3MidiEvent {
    pub(crate) sample_offset: u32,
    pub(crate) status: u8,
    pub(crate) data1: u8,
    pub(crate) data2: u8,
    pub(crate) port: u8,
    pub(crate) note_id: i32,
    pub(crate) ne_value: f64,
}

#[cfg(test)]
pub(crate) struct NoteIdMap {
    pub(crate) slots: [TestNoteIdSlot; Self::CAPACITY],
    pub(crate) cursor: usize,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) struct TestNoteIdSlot {
    pub(crate) note_id: i32,
    pub(crate) port: u8,
    pub(crate) channel: u8,
    pub(crate) note: u8,
}

#[cfg(test)]
impl NoteIdMap {
    pub(crate) const CAPACITY: usize = 128;
    const FREE: TestNoteIdSlot = TestNoteIdSlot {
        note_id: -1,
        port: 0,
        channel: 0,
        note: 0,
    };

    pub(crate) fn new() -> Self {
        Self {
            slots: [Self::FREE; Self::CAPACITY],
            cursor: 0,
        }
    }

    pub(crate) fn insert(&mut self, port: u8, note_id: i32, channel: u8, note: u8) {
        if note_id < 0 {
            return;
        }
        let index = self
            .slots
            .iter()
            .position(|slot| slot.note_id == note_id && slot.port == port)
            .or_else(|| self.slots.iter().position(|slot| slot.note_id < 0))
            .unwrap_or_else(|| {
                let index = self.cursor;
                self.cursor = (index + 1) % Self::CAPACITY;
                index
            });
        self.slots[index] = TestNoteIdSlot {
            note_id,
            port,
            channel,
            note,
        };
    }

    pub(crate) fn lookup(&self, port: u8, note_id: i32) -> Option<(u8, u8)> {
        self.slots
            .iter()
            .find(|slot| note_id >= 0 && slot.note_id == note_id && slot.port == port)
            .map(|slot| (slot.channel, slot.note))
    }

    pub(crate) fn clear(&mut self) {
        self.slots = [Self::FREE; Self::CAPACITY];
        self.cursor = 0;
    }
}
