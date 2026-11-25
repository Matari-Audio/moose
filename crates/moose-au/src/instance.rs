//! Per-instance state behind the AU shim's opaque context.

use crate::catalog::{AuExposedParams, RenderCatalog};
use crate::host_drain::{self, HostSink};
use crate::output_events::NthCursor;
use crate::output_events::UmpDrainCursor;

use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::editor::Editor;
use moose_core::editor::EditorBuilder;
use moose_core::events::{EventList, LosslessEventCursor};
use moose_core::export::PluginExport;
use moose_core::host_queue::HostQueue;
use moose_core::main_thread::MainThreadHooks;
use moose_core::meters::MeterStore;
use moose_core::plugin::PluginRuntime;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::tasks::AnyTaskSpawner;
use moose_core::thread::MainThreadCell;
use moose_core::ump::SysExAssembler;
use moose_core::wrapper::PluginCell;
use moose_params::{CatalogSignal, ParamCatalog, PersistChangeSignal};
use std::ffi::CString;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Instance wrapper - one per plugin instance, stored as the opaque ctx
// ---------------------------------------------------------------------------

/// Opaque `AuInstance<P>` handle carried across AU host threads and the
/// main-queue drain source.
///
/// Only dereferenced inside `HostQueue::with_open`. `finalize_destroy` closes
/// the queue before dropping the instance, and that drop cancels the drain
/// source (`host_drain::Dispatcher`).
#[derive(Clone, Copy)]
pub(crate) struct AuCtx(pub(crate) *mut std::ffi::c_void);

// SAFETY: `AuCtx` is an opaque instance pointer, dereferenced only inside
// `HostQueue::with_open`. `finalize_destroy` calls `close()`, which waits for
// every active `with_open` closure to return, before it drops the instance, so
// no dereference outlives the allocation. (`cb_destroy` itself is not
// serialized against those closures: it closes signals and `begin_close`s
// while one may still be running.)
unsafe impl Send for AuCtx {}
// SAFETY: Shared access is no different: `AuCtx` is a `Copy` address with no interior state, and
// dereferences happen only inside `HostQueue::with_open` as described above.
unsafe impl Sync for AuCtx {}

impl AuCtx {
    #[inline]
    pub(crate) fn as_ptr(self) -> *mut std::ffi::c_void {
        self.0
    }
}

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
pub(crate) type StateLoadQueue = state::StateLoadQueue;

pub(crate) use moose_core::c_abi_driver::{
    EditorGesture, GestureQueue, SysExQueue, drain_sysex_input, new_sysex_queues,
    queue_sysex_input, try_queue_sysex_input,
};
#[cfg(test)]
pub(crate) use moose_core::c_abi_driver::{SYSEX_INPUT_SLOTS, SysExInput};

/// Cutoffs for asynchronous edits and accepted render automation respectively.
#[derive(Clone, Copy, Default)]
pub(crate) struct FeedbackEpoch {
    pub(crate) edits: u64,
    pub(crate) automation: u64,
}

/// Wait-free queue for process-emitted feedback drained by the host main thread.
pub(crate) type ParamNotifyQueue = crossbeam_queue::ArrayQueue<(u32, f32, FeedbackEpoch)>;

/// Explicit host lifecycle state for `AuInstance::lifecycle_state`.
pub(crate) const LIFECYCLE_INACTIVE: u8 = 1;
pub(crate) const LIFECYCLE_ACTIVE: u8 = 2;

/// Retained `AUv3` ramp state spanning multiple render slices (Apple delivers an
/// `AUParameterEvent` ramp only in the slice where it begins).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ActiveAuRamp {
    pub(crate) param_id: u32,
    pub(crate) current_val: f64,
    pub(crate) target_val: f64,
    pub(crate) remaining_frames: u32,
}

/// Per-block parameter trajectory entry used to derive exact starting values
/// for subsequent or mid-ramp replacement events without heap allocation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParamTrackEntry {
    pub(crate) id: u32,
    pub(crate) start_offset: u32,
    pub(crate) end_offset: u32,
    pub(crate) start_val: f64,
    pub(crate) end_val: f64,
}

pub(crate) struct AuInstance<P: PluginExport> {
    /// Wrapper format and host process name, for every `prepare`.
    pub(crate) host_identity: moose_core::info::HostIdentity,
    // First fields: close worker requests and dispatch sources before any
    // native/editor/host-sink teardown.
    pub(crate) persist_signal: Option<Arc<PersistChangeSignal>>,
    pub(crate) catalog_signal: Option<Arc<CatalogSignal>>,
    pub(crate) host_queue: Arc<HostQueue>,
    /// Per-instance main-queue `DATA_OR` dispatch source for event-driven
    /// main-thread drains (open..close).
    pub(crate) host_dispatch: Option<host_drain::Dispatcher>,
    pub(crate) render_signals: Arc<host_drain::RenderSignals>,
    pub(crate) pending_context_name: Mutex<Option<moose_core::main_thread::TrackInfo>>,
    /// Main-thread plugin hooks (`connect`, `track_info_changed`, ...).
    pub(crate) main_thread: MainThreadCell<Box<dyn MainThreadHooks>>,
    /// The plugin owned directly by this AU instance inside [`PluginCell`]:
    /// the audio thread owns it per block, lifecycle callbacks own it while
    /// stopped, and no other thread holds a cloned `Arc` to it.
    pub(crate) plugin: PluginCell<P>,
    /// Stable handle to the params Arc, set once at instance creation.
    pub(crate) params_arc: Arc<P::Params>,
    /// Per-instance live exposed parameter catalog & clump names, queried by
    /// main/control-thread property callbacks and rebuilt on `PRESENTATION`.
    pub(crate) exposed: Mutex<Arc<AuExposedParams>>,
    /// Shared meter storage, set once at instance creation.
    pub(crate) meter_store: Arc<MeterStore>,
    /// Lock-free custom-state slot the audio thread publishes into.
    pub(crate) snapshot: Arc<SnapshotSlot>,
    /// Background-task spawner (`None` unless the plugin wired `tasks:`).
    pub(crate) task_spawner: Option<AnyTaskSpawner>,
    /// Lock-free editor factory, cached at creation.
    pub(crate) editor_builder: EditorBuilder<P::Params>,
    /// The latency and tail reported to the host. Updated on activation
    /// (`publish`) and promoted on main-thread drain (`commit_observed`)
    /// right before firing `kAudioUnitProperty_Latency` / `TailTime` / KVO.
    pub(crate) delay: moose_core::delay::ReportedDelay,
    /// Current render mode as a [`ProcessMode`] discriminant.
    pub(crate) render_mode: AtomicU8,
    /// Atomic mirrors of the last prepared `(prepared, sample_rate, max_frames, mode)`
    /// so render-thread `AudioUnitReset` can detect `same_config` without taking a
    /// blocking `audio.enter()`.
    pub(crate) prepared_flag: std::sync::atomic::AtomicBool,
    pub(crate) prepared_rate_bits: AtomicU64,
    pub(crate) prepared_max_frames: std::sync::atomic::AtomicUsize,
    pub(crate) prepared_mode: AtomicU8,
    /// Explicit host lifecycle state (`LIFECYCLE_UNKNOWN` for legacy `cb_create`,
    /// `LIFECYCLE_INACTIVE` for stopped v13 instances, `LIFECYCLE_ACTIVE` while
    /// rendering).
    pub(crate) lifecycle_state: AtomicU8,
    /// Thread that instantiated this instance (used on non-Apple targets and
    /// under `#[cfg(test)]`, where `libtest` spawns worker threads with
    /// `pthread_main_np() == 0`, to distinguish UI-thread vs worker calls).
    #[cfg(any(not(target_vendor = "apple"), test))]
    pub(crate) owner_thread: std::thread::ThreadId,
    /// Main/control-thread lock serializing activation/deactivation against
    /// idle `state_load` handoffs.
    /// NEVER acquired on the audio thread.
    pub(crate) lifecycle_lock: Mutex<()>,
    pub(crate) render_catalog: RenderCatalog,
    /// Monotonic host/editor edit clock sampled once at block start before
    /// `process()` runs.
    pub(crate) global_edit_seq: AtomicU64,
    /// Per-parameter last host/editor edit sequence numbers, grouped by catalog update.
    pub(crate) param_edit_seqs: EditSequences,
    /// Off-UI-thread editor gestures queued for main/UI-thread delivery so
    /// worker threads never invoke `MooseAuHostSink` callbacks inline.
    pub(crate) editor_gestures: std::sync::Mutex<GestureQueue>,
    /// Wait-free queue for process-emitted `ParamChange` host feedback,
    /// drained on the main thread via `HostSink::set_param`.
    pub(crate) param_out_queue: ParamNotifyQueue,
    /// The automation mode from `moose.toml`'s `[automation]`.
    pub(crate) automation: moose_core::Automation,
    pub(crate) input_bus_count: usize,
    pub(crate) output_bus_count: usize,
    /// AU exposes one main input plus one sidechain element.
    pub(crate) sidechain_channels: u32,
    pub(crate) plugin_id_hash: u64,
    /// Audio + lifecycle-owned per-block scratch.
    pub(crate) audio: PluginCell<AuAudio<P>>,
    /// Main/UI-thread-owned editor state.
    pub(crate) gui: PluginCell<AuGui>,
    /// Deferred editor resize requested re-entrantly through `request_resize`.
    pub(crate) pending_resize: AtomicU64,
    /// Shared transport slot: audio thread writes each block, editor reads.
    pub(crate) transport_slot: Arc<TransportSlot>,
    /// Bounded SPSC handoff for state loads.
    pub(crate) pending_state: Arc<StateLoadQueue>,
    /// Set by a panic in `process`; cleared by the next successful prepare.
    pub(crate) panic_latch: moose_core::wrapper::PanicLatch,
    /// `SysEx` input queued from the host thread (`MusicDeviceSysEx`) plus
    /// recycled free-list.
    pub(crate) sysex_ready: SysExQueue,
    pub(crate) sysex_free: SysExQueue,
    /// `legacy_au_keys` from `PluginInfo`, NUL-terminated for `ClassInfo`.
    pub(crate) legacy_key_cstrings: Vec<CString>,
    /// Host notification vtable. MUST remain the **last** field so its `Drop`
    /// (`release_user_data`) runs only after every signal, dispatch source,
    /// editor, and plugin resource has dropped.
    pub(crate) host_sink: HostSink,
}

impl<P: PluginExport> AuInstance<P> {
    /// Current global host/editor edit clock (wait-free, O(1), RT-safe).
    #[inline]
    pub(crate) fn current_edit_seq(&self) -> u64 {
        self.global_edit_seq
            .load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn param_edit_seq(&self, id: u32) -> u64 {
        self.param_edit_seqs
            .get(id)
            .map_or(0, |slot| slot.edit.load(Ordering::Acquire))
    }

    #[inline]
    fn next_edit_seq(&self) -> u64 {
        self.global_edit_seq
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    /// Register an edit across the actual parameter store write.
    pub(crate) fn edit_param<R>(&self, id: u32, write: impl FnOnce() -> R) -> R {
        let Some(slot) = self.param_edit_seqs.get(id) else {
            return write();
        };
        let _edit = slot.begin_edit();
        let result = write();
        slot.edit.fetch_max(self.next_edit_seq(), Ordering::AcqRel);
        result
    }

    /// Accepted automation invalidates older queued feedback, but must not
    /// hide an asynchronous edit that happened during this block's staging.
    pub(crate) fn accept_automation(&self, id: u32) {
        if let Some(slot) = self.param_edit_seqs.get(id) {
            slot.automation
                .store(self.next_edit_seq(), Ordering::Release);
        }
    }

    pub(crate) fn publish_feedback(
        &self,
        id: u32,
        epoch: FeedbackEpoch,
        write: impl FnOnce(),
    ) -> bool {
        let Some(slot) = self.param_edit_seqs.get(id) else {
            return false;
        };
        let Some(_publication) = slot.try_begin_publication() else {
            return false;
        };
        if !slot.matches(epoch) {
            return false;
        }
        write();
        true
    }

    pub(crate) fn feedback_is_current(&self, id: u32, epoch: FeedbackEpoch) -> bool {
        self.param_edit_seqs.get(id).is_some_and(|slot| {
            // An in-flight edit might have written its value but not recorded the sequence yet.
            slot.access.load(Ordering::Acquire) & !PUBLISHING == 0 && slot.matches(epoch)
        })
    }

    /// `true` only when the adapter explicitly confirmed this instance is
    /// stopped (`cb_create_with_sink` with a non-null v13 sink, or after
    /// `cb_set_active(0)`). Legacy `cb_create()` instances stay `UNKNOWN`.
    #[inline]
    pub(crate) fn is_known_inactive(&self) -> bool {
        self.lifecycle_state
            .load(std::sync::atomic::Ordering::Acquire)
            == LIFECYCLE_INACTIVE
    }

    /// `true` when `cb_set_active(1)` is currently in effect.
    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.lifecycle_state
            .load(std::sync::atomic::Ordering::Acquire)
            == LIFECYCLE_ACTIVE
    }

    /// `true` when called on the host main/UI thread.
    #[inline]
    #[cfg_attr(
        all(target_vendor = "apple", not(test)),
        allow(
            clippy::unused_self,
            reason = "non-Apple targets and test builds compare against owner_thread"
        )
    )]
    pub(crate) fn is_ui_thread(&self) -> bool {
        #[cfg(all(target_vendor = "apple", not(test)))]
        {
            unsafe extern "C" {
                fn pthread_main_np() -> std::os::raw::c_int;
            }
            // SAFETY: `pthread_main_np` takes no arguments and only reads the calling thread's
            // identity.
            unsafe { pthread_main_np() != 0 }
        }
        #[cfg(any(not(target_vendor = "apple"), test))]
        {
            std::thread::current().id() == self.owner_thread
        }
    }
}

// Low bits count host/editor writes. The high bit reserves publication for
// render. Host/editor writes remain concurrent with one another; render only
// attempts the reservation once and skips feedback if an edit is in progress.
const PUBLISHING: u64 = 1 << 63;

#[derive(Default)]
struct ParamEditSlot {
    edit: AtomicU64,
    automation: AtomicU64,
    access: AtomicU64,
}

impl ParamEditSlot {
    fn begin_edit(&self) -> ParamAccess<'_> {
        if self.access.fetch_add(1, Ordering::AcqRel) & PUBLISHING != 0 {
            // Only the short render store can be in progress, never another
            // host/editor writer. The AUv2 render thread's own SetParameter
            // call and output commit run serially for the same instance.
            while self.access.load(Ordering::Acquire) & PUBLISHING != 0 {
                std::hint::spin_loop();
            }
        }
        ParamAccess {
            access: &self.access,
            reservation: 1,
        }
    }

    fn try_begin_publication(&self) -> Option<ParamAccess<'_>> {
        self.access
            .compare_exchange(0, PUBLISHING, Ordering::AcqRel, Ordering::Relaxed)
            .ok()
            .map(|_| ParamAccess {
                access: &self.access,
                reservation: PUBLISHING,
            })
    }

    fn matches(&self, epoch: FeedbackEpoch) -> bool {
        self.edit.load(Ordering::Acquire) <= epoch.edits
            && self.automation.load(Ordering::Acquire) <= epoch.automation
    }
}

struct ParamAccess<'a> {
    access: &'a AtomicU64,
    reservation: u64,
}

impl Drop for ParamAccess<'_> {
    fn drop(&mut self) {
        self.access.fetch_sub(self.reservation, Ordering::AcqRel);
    }
}

/// Build sorted per-parameter-ID host-edit sequence slots from `catalog` (init-time).
pub(crate) struct EditSequences {
    slots: Box<[(u32, ParamEditSlot)]>,
    extra: std::sync::OnceLock<Box<EditSequences>>,
}

impl EditSequences {
    fn get(&self, id: u32) -> Option<&ParamEditSlot> {
        let mut group = self;
        loop {
            if let Ok(index) = group.slots.binary_search_by_key(&id, |s| s.0) {
                return Some(&group.slots[index].1);
            }
            group = group.extra.get()?;
        }
    }

    // Single catalog writer; existing slots stay live for concurrent readers.
    #[cfg_attr(
        not(any(target_vendor = "apple", test)),
        allow(
            dead_code,
            reason = "catalog changes are delivered by the Apple main queue"
        )
    )]
    pub(crate) fn extend_catalog(&self, catalog: &ParamCatalog) {
        let mut slots: Vec<_> = catalog
            .iter()
            .filter(|info| self.get(info.id).is_none())
            .map(|info| (info.id, ParamEditSlot::default()))
            .collect();
        if !slots.is_empty() {
            slots.sort_unstable_by_key(|s| s.0);
            let mut tail = self;
            while let Some(next) = tail.extra.get() {
                tail = next;
            }
            tail.extra.get_or_init(|| {
                Box::new(Self {
                    slots: slots.into(),
                    extra: std::sync::OnceLock::new(),
                })
            });
        }
    }
}

pub(crate) fn new_param_edit_seqs(catalog: &ParamCatalog) -> EditSequences {
    let mut slots: Vec<_> = catalog
        .iter()
        .map(|info| (info.id, ParamEditSlot::default()))
        .collect();
    slots.sort_unstable_by_key(|s| s.0);
    EditSequences {
        slots: slots.into(),
        extra: std::sync::OnceLock::new(),
    }
}

/// Audio + lifecycle-owned per-block scratch (see [`AuInstance::audio`]).
pub(crate) struct AuAudio<P: PluginExport> {
    /// Audio-thread snapshot of the parameter catalog (for `CHUNKED` split
    /// lookups and output param ID validation). Swapped only while inactive.
    pub(crate) catalog: Arc<ParamCatalog>,
    /// Retained `AUv3` multi-slice ramps (preallocated at init, zero RT alloc).
    pub(crate) active_ramps: Vec<ActiveAuRamp>,
    /// Preallocated scratch copy of `active_ramps` so a rejected render slice
    /// (`AU_PROCESS_QUEUE_FULL` / `AU_PROCESS_INVALID`) leaves `active_ramps` intact.
    pub(crate) active_ramps_scratch: Vec<ActiveAuRamp>,
    /// Per-block parameter trajectory tracker (preallocated capacity 512, zero RT alloc).
    pub(crate) param_tracker: Vec<ParamTrackEntry>,
    /// Async edit cutoff captured before staging; automation cutoff captured
    /// after this block's accepted host events advance their own sequences.
    pub(crate) feedback_epoch: FeedbackEpoch,
    pub(crate) event_list: EventList,
    pub(crate) output_events: EventList,
    /// Resume point for the appex's sequential `output_ump_at` drain.
    pub(crate) ump_drain_cursor: UmpDrainCursor,
    /// Same for `output_event_at` and `output_sysex_at`.
    pub(crate) midi_drain_cursor: NthCursor,
    pub(crate) sysex_drain_cursor: NthCursor,
    pub(crate) native_output_cursor: LosslessEventCursor,
    pub(crate) native_output_carriers: u32,
    pub(crate) native_output_num_frames: u32,
    pub(crate) native_output_ump_protocol: u8,
    pub(crate) native_output_max_absolute_offset: u32,
    pub(crate) native_output_ump_time_valid: bool,
    pub(crate) native_output_status: u32,
    /// Per-sub-block scratch for `chunked_process::process_chunked`.
    pub(crate) sub_event_scratch: EventList,
    /// Per-instance UMP `SysEx` reassembler.
    pub(crate) sysex_assembler: SysExAssembler,
    pub(crate) sample_rate: f64,
    pub(crate) max_block_size: usize,
    pub(crate) prepared: bool,
    pub(crate) prepared_mode: u8,
    /// Reused per-block scratch for `RawBufferScratch::build`.
    pub(crate) scratch: RawBufferScratch<<P as PluginRuntime>::Sample>,
}

/// Main/UI-thread-owned editor state (see [`AuInstance::gui`]).
pub(crate) struct AuGui {
    pub(crate) editor: Option<Box<dyn Editor>>,
    /// The host-owned container view the editor last opened in (`0` = none).
    #[cfg_attr(
        not(target_os = "macos"),
        expect(dead_code, reason = "only read by the macOS view detach and fit")
    )]
    pub(crate) view: usize,
}
