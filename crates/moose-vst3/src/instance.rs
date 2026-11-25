//! Per-instance state behind [`crate::backend::Vst3Backend`], and the output note-id table.

use crate::com::Host;
use crate::output_events::MidiMap;

use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::editor::{Editor, EditorBuilder, EditorFeedback};
use moose_core::events::{EVENT_LIST_PREALLOC, EventList, LosslessEventCursor};
use moose_core::export::PluginExport;
use moose_core::host_queue::HostQueue;
use moose_core::main_thread::MainThreadHooks;
use moose_core::meters::MeterStore;
use moose_core::plugin::PluginRuntime;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::tasks::AnyTaskSpawner;
use moose_core::thread::MainThreadCell;
use moose_core::wrapper::PluginCell;
use moose_params::{ParamRange, PersistChangeSignal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Instance wrapper
// ---------------------------------------------------------------------------

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
pub(crate) type StateLoadQueue = state::StateLoadQueue;

/// VST3 `RestartFlags::kReloadComponent`: ask the host to reload / restart
/// the component.
pub(crate) const K_RELOAD_COMPONENT: i32 = 1;
/// VST3 `RestartFlags::kIoChanged`: the buses changed.
pub(crate) const K_IO_CHANGED: i32 = 2;
/// VST3 `RestartFlags::kParamValuesChanged`: values moved without the host
/// asking (a state load, or a queued state the audio thread applied), so
/// the host re-reads them.
pub(crate) const K_PARAM_VALUES_CHANGED: i32 = 4;
/// VST3 `RestartFlags::kLatencyChanged`.
pub(crate) const K_LATENCY_CHANGED: i32 = 8;
/// VST3 `RestartFlags::kKeyswitchChanged`.
pub(crate) const K_KEYSWITCH_CHANGED: i32 = 1024;

/// Shared DPI scale state between [`Vst3Instance`] and [`crate::gui::Vst3Host`],
/// so editor callbacks read/write scales without raw instance pointers.
#[derive(Default)]
pub(crate) struct EditorScaleState {
    /// Content scale from `setContentScaleFactor` (f64 bits, `0` until the
    /// host sends one; ignored on macOS).
    pub(crate) host_scale: AtomicU64,
    /// Last content scale actually applied to the editor via `set_scale_factor`
    /// (`0` = none applied yet). Tracked separately from `host_scale` so scale
    /// changes reported while the GUI guard is held are applied after release.
    pub(crate) last_applied: AtomicU64,
    /// Open editor's own window scale (`Editor::window_scale`, f64 bits, `0` = none).
    pub(crate) window_scale: AtomicU64,
}

impl EditorScaleState {
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

    /// Scale factor between VST3 `ViewRect` coordinates and editor logical
    /// points: always `1.0` on macOS (VST3/AppKit `ViewRect` is in logical
    /// points even on Retina displays where `window_scale` is `2.0`), and
    /// [`Self::host_scale`] on Windows/Linux.
    pub(crate) fn view_rect_scale(&self) -> f64 {
        #[cfg(target_os = "macos")]
        {
            1.0
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.host_scale()
        }
    }

    /// The last content scale the host sent, if it ever did.
    pub(crate) fn reported_host_scale(&self) -> Option<f64> {
        match self.host_scale.load(Ordering::Relaxed) {
            0 => None,
            bits => Some(f64::from_bits(bits)),
        }
    }

    /// The last host content scale actually applied to the editor.
    pub(crate) fn applied_host_scale(&self) -> Option<f64> {
        match self.last_applied.load(Ordering::Relaxed) {
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

    pub(crate) fn set_applied_host_scale(&self, scale: f64) {
        self.last_applied.store(scale.to_bits(), Ordering::Relaxed);
    }
}

pub(crate) struct Vst3Instance<P: PluginExport> {
    /// Wrapper format and host name; the name may arrive late, from
    /// `IComponent::initialize`. UI thread only (`initialize`, `prepare`).
    pub(crate) host: std::sync::Mutex<moose_core::info::HostIdentity>,
    pub(crate) persist_signal: std::sync::Mutex<Option<Arc<PersistChangeSignal>>>,
    /// The bound `CatalogSignal` (same type as the persist signal).
    pub(crate) catalog_signal: std::sync::Mutex<Option<Arc<PersistChangeSignal>>>,
    /// The plugin's main-thread part; reached only from edit-controller
    /// callbacks, which VST3 runs on the UI thread.
    pub(crate) main_thread: MainThreadCell<Box<dyn MainThreadHooks>>,
    /// The plugin owned directly by this VST3 instance inside [`PluginCell`].
    pub(crate) plugin: PluginCell<P>,
    /// Stable handle to the params Arc, set once at instance creation.
    pub(crate) params_arc: Arc<P::Params>,
    /// Shared meter storage, set once at instance creation.
    pub(crate) meter_store: Arc<MeterStore>,
    /// Shared reactive UI feedback (indications, modulation, 0 Hz editor waker).
    pub(crate) feedback: Arc<EditorFeedback>,
    /// Lock-free custom-state slot the audio thread publishes into.
    pub(crate) snapshot: Arc<SnapshotSlot>,
    /// Background-task spawner (`None` unless the plugin wired `tasks:`).
    pub(crate) task_spawner: Option<AnyTaskSpawner>,
    /// Lock-free editor factory, cached at creation.
    pub(crate) editor_builder: EditorBuilder<P::Params>,
    /// Full param-info cache for the chunker's `is_chunked(id)` lookup.
    pub(crate) param_infos: Arc<moose_params::ParamCatalog>,
    /// The automation mode from `moose.toml`'s `[automation]` table.
    pub(crate) automation: moose_core::Automation,
    pub(crate) plugin_id_hash: u64,
    /// `true` between `setActive(true)` and `setActive(false)`.
    pub(crate) active: AtomicBool,
    /// `true` between `setProcessing(true)` and `setProcessing(false)` /
    /// `setActive(false)`, and set by the first `process` for hosts that
    /// never call `setProcessing`.
    pub(crate) processing: AtomicBool,
    /// Sorted `(id, range)` lookup for O(log n) normalization without hashing.
    pub(crate) param_ranges: Vec<(u32, ParamRange)>,
    /// Sorted `(id, dense_index)` table built at instantiation for O(log n)
    /// dense-index parameter dispatch (`*_by_index`).
    pub(crate) param_id_indices: Vec<(u32, usize)>,
    /// Precomputed MIDI-controller bindings, sorted by param id.
    pub(crate) midi_maps: Vec<(u32, MidiMap)>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    pub(crate) transport_slot: Arc<TransportSlot>,
    /// Bounded SPSC handoff for state loads.
    pub(crate) pending_state: Arc<StateLoadQueue>,
    /// Set by a panic in `process`; cleared by the next successful prepare.
    pub(crate) panic_latch: moose_core::wrapper::PanicLatch,
    /// The latency and tail reported to the host.
    pub(crate) delay: moose_core::delay::ReportedDelay,
    /// Collision-free hidden MIDI proxy IDs in logical `(port, channel, controller)` order.
    pub(crate) midi_proxy_ids: Vec<u32>,
    /// Last-seen values of the hidden MIDI proxy params (f64 bits).
    pub(crate) midi_proxy_values: Vec<AtomicU64>,
    /// Shared host / window DPI scale state.
    pub(crate) scale: Arc<EditorScaleState>,
    /// Deferred physical resize `(w << 32) | h` stashed while `gui` cell is busy.
    pub(crate) pending_resize: AtomicU64,
    /// Last known physical editor size `(w << 32) | h` (`0` until queried or opened).
    pub(crate) last_phys_size: AtomicU64,
    /// Audio + lifecycle-owned per-block scratch.
    pub(crate) audio: PluginCell<Vst3Scratch<P>>,
    /// Main/UI-thread-owned editor state.
    pub(crate) gui: PluginCell<Vst3Gui>,
    /// Per-instance main-thread mailbox for signals and context-menu requests.
    pub(crate) host_queue: Arc<HostQueue>,
    /// Monotonic COM instance registry key (`0` when unbound / standalone unit test).
    pub(crate) host_id: AtomicUsize,
    /// Borrowed pointer to the COM `Host`, set by `bind_host_queue` in `Host::new`
    /// and cleared + joined via `host_queue.close()` in `Host::drop`.
    pub(crate) host_wake_target: Arc<AtomicPtr<Host>>,
}

impl<P: PluginExport> Drop for Vst3Instance<P> {
    fn drop(&mut self) {
        self.host_wake_target
            .store(std::ptr::null_mut(), Ordering::Release);
        self.host_queue.close();
        crate::lifecycle::rebind_signal(&self.persist_signal, None, None);
        crate::lifecycle::rebind_signal(&self.catalog_signal, None, None);
    }
}

impl<P: PluginExport> Vst3Instance<P> {
    /// Resolve `id` to its dense catalog slot in `O(log n)`.
    #[inline]
    pub(crate) fn param_index_of(&self, id: u32) -> Option<usize> {
        self.param_id_indices
            .binary_search_by_key(&id, |&(pid, _)| pid)
            .ok()
            .map(|at| self.param_id_indices[at].1)
    }

    #[inline]
    pub(crate) fn host_id(&self) -> usize {
        self.host_id.load(Ordering::Acquire)
    }

    /// Cache the most recently computed non-zero physical editor size.
    pub(crate) fn store_phys_size(&self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.last_phys_size
                .store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
        }
    }

    pub(crate) fn host_scale(&self) -> f64 {
        self.scale.host_scale()
    }

    pub(crate) fn reported_host_scale(&self) -> Option<f64> {
        self.scale.reported_host_scale()
    }

    pub(crate) fn note_window_scale(&self, editor: &dyn Editor) {
        self.scale.note_window_scale(editor);
    }

    pub(crate) fn set_host_scale(&self, scale: f64) {
        self.scale.set_host_scale(scale);
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
    /// Max block size declared by the host in `setupProcessing`.
    pub(crate) max_block_size: usize,
    /// `true` once `prepare` ran (host `setActive(true)`).
    pub(crate) prepared: bool,
    /// Reused per-block scratch for `RawBufferScratch::build`.
    pub(crate) scratch: RawBufferScratch<<P as PluginRuntime>::Sample>,
    pub(crate) output_cursor: LosslessEventCursor,
    pub(crate) output_preflight_status: u32,
    pub(crate) output_note_ids: OutputNoteIds,
    pub(crate) pending_output_mutation: PendingOutputMutation,
    /// Coalesced flag set by `push_vst3_param_change` so `process_block`
    /// signals `HostSignal::EDITOR_WAKE` at most once per audio block.
    pub(crate) param_changed_this_block: bool,
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
