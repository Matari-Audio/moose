//! Per-instance state behind the AU shim's opaque context.

#[cfg(target_os = "macos")]
use crate::gui::moose_au_v2_host_latency_changed;
#[cfg(target_os = "macos")]
use crate::gui::moose_au_v2_host_set_param;
use crate::output_events::NthCursor;
use crate::output_events::UmpDrainCursor;
#[cfg(target_os = "macos")]
use crate::persist_notify;

use moose_core::SYSEX_POOL_PREALLOC;
use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::editor::Editor;
use moose_core::editor::EditorBuilder;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::editor::SendPtr;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::events::EVENT_LIST_PREALLOC;
use moose_core::events::{EventList, ExactEvent, ExactEventBody, LosslessEventCursor};
use moose_core::export::PluginExport;
use moose_core::host_queue::HostQueue;
use moose_core::meters::MeterStore;
use moose_core::plugin::PluginRuntime;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::tasks::AnyTaskSpawner;
use moose_core::ump::SysExAssembler;
use moose_core::wrapper::{PluginCell, SharedPlugin};
use moose_params::PersistChangeSignal;
use std::ffi::CString;
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicBool;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64};
#[cfg(target_os = "macos")]
use std::thread::{self, JoinHandle, Thread};

// ---------------------------------------------------------------------------
// Instance wrapper - one per plugin instance, stored as the opaque ctx
// ---------------------------------------------------------------------------

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
pub(crate) type StateLoadQueue = state::StateLoadQueue;

/// One `SysEx` message handed from the host thread to the audio thread.
/// Recycled through a free-list (see [`SysExQueue`]) so the audio thread
/// never allocates or frees: it copies the bytes into the event pool and
/// returns the buffer.
pub(crate) struct SysExInput {
    pub(crate) sample_offset: u32,
    pub(crate) bytes: Vec<u8>,
}

impl Default for SysExInput {
    fn default() -> Self {
        Self {
            sample_offset: 0,
            bytes: Vec::with_capacity(SYSEX_POOL_PREALLOC),
        }
    }
}

/// Lock-free `SysEx` input handoff. `SYSEX_INPUT_SLOTS` buffers cycle
/// between the *free* queue (empty, ready to fill) and the *ready* queue
/// (filled, awaiting render). The host thread's `MusicDeviceSysEx`
/// callback - which AU v2 lets the host call off the render thread - pops
/// a free buffer, copies the message in, and pushes it ready; the audio
/// thread drains ready into the event list and returns each buffer to
/// free. No lock, allocation, or free on the audio thread. Since every
/// buffer is one of `SYSEX_INPUT_SLOTS` total, the ready/free pushes
/// always have room; only the host-thread pop can come up empty (queue
/// saturated), in which case the incoming message is dropped rather than
/// backlogged.
pub(crate) type SysExQueue = crossbeam_queue::ArrayQueue<SysExInput>;

/// `SysEx` input buffers cycling through [`SysExQueue`]. Generous for the
/// small control messages (MMC, device inquiry, MPE config) that are the
/// realistic input to an effect / instrument between two render blocks.
pub(crate) const SYSEX_INPUT_SLOTS: usize = 64;

/// Host-thread side of the `SysEx` input handoff: copy one message into a
/// recycled buffer and queue it. Drops the message if no buffer is free
/// (the audio thread hasn't drained - queue saturated) rather than
/// building an unbounded backlog. Never touches the audio-thread scratch,
/// so it is safe to call from whatever thread the host runs
/// `MusicDeviceSysEx` on.
pub(crate) fn try_queue_sysex_input(
    free: &SysExQueue,
    ready: &SysExQueue,
    sample_offset: u32,
    bytes: &[u8],
) -> bool {
    let Some(mut slot) = free.pop() else {
        return false;
    };
    if bytes.len() > slot.bytes.capacity() {
        let _ = free.push(slot);
        return false;
    }
    slot.sample_offset = sample_offset;
    slot.bytes.clear();
    slot.bytes.extend_from_slice(bytes);
    // We hold one of the fixed set of buffers, so `ready` has room; the
    // recycle-on-error keeps the invariant sound regardless.
    if let Err(slot) = ready.push(slot) {
        let _ = free.push(slot);
        return false;
    }
    true
}

pub(crate) fn queue_sysex_input(
    free: &SysExQueue,
    ready: &SysExQueue,
    sample_offset: u32,
    bytes: &[u8],
) {
    let _ = try_queue_sysex_input(free, ready, sample_offset, bytes);
}

/// Audio-thread side: drain every queued `SysEx` into `event_list`,
/// returning each buffer to `free`. Lock-free, and it only copies into
/// the pre-allocated event pool and moves buffer handles between queues -
/// no allocation or free on the audio thread.
pub(crate) fn drain_sysex_input(ready: &SysExQueue, free: &SysExQueue, event_list: &mut EventList) {
    while let Some(slot) = ready.pop() {
        let exact = ExactEvent::new(slot.sample_offset, ExactEventBody::SysEx { port: 0 });
        let _ =
            event_list.try_push_sysex_with_exact_on_port(slot.sample_offset, 0, &slot.bytes, exact);
        let _ = free.push(slot);
    }
}

/// Wait-free queue for AU v2 process-emitted `ParamChange` feedback:
/// the audio thread pushes `(param_id, plain_value)`, the notifier
/// thread drains and forwards them to the host. Sized to a full
/// block's worth of emissions so a normal block never drops.
#[cfg(target_os = "macos")]
pub(crate) type ParamNotifyQueue = crossbeam_queue::ArrayQueue<(u32, f32)>;

/// Off-audio-thread host-notify pump for AU v2 (macOS only). When a
/// plugin changes its own parameters during `process()`, the host UI /
/// automation must be told - but `AudioUnitSetParameter` +
/// `AUEventListenerNotify` take locks and dispatch host callbacks,
/// forbidden on the audio path. So `cb_process` pushes the changes to a
/// wait-free queue and unparks this thread, which flushes them off the
/// render thread. `Drop` stops and joins the thread before the
/// `AuInstance` (and the AU-shim component the notify targets) is torn
/// down.
#[cfg(target_os = "macos")]
pub(crate) struct ParamNotifier {
    pub(crate) queue: Arc<ParamNotifyQueue>,
    pub(crate) stop: Arc<AtomicBool>,
    /// Set by the audio thread when `latency()` changes; drained on the
    /// same wakeup as the param queue. The notifier thread broadcasts a
    /// `kAudioUnitProperty_Latency` change, off the audio thread.
    pub(crate) latency_dirty: Arc<AtomicBool>,
    /// Cached for a cheap `unpark` from the audio thread.
    pub(crate) thread: Thread,
    pub(crate) handle: Option<JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl ParamNotifier {
    /// Spawn the notifier for the instance the AU shim maps to `ctx`.
    /// Returns `None` if the thread can't be spawned (feedback is then
    /// silently skipped rather than run on the audio thread).
    ///
    /// # Safety
    /// `ctx` must stay a valid AU-shim instance handle until this
    /// notifier is dropped - guaranteed because it lives in the
    /// `AuInstance` and `cb_destroy` drops it (joining the thread)
    /// before the shim frees the component.
    pub(crate) unsafe fn spawn(ctx: SendPtr<std::ffi::c_void>) -> Option<Self> {
        let queue = Arc::new(ParamNotifyQueue::new(EVENT_LIST_PREALLOC));
        let stop = Arc::new(AtomicBool::new(false));
        let latency_dirty = Arc::new(AtomicBool::new(false));
        let (q, s, ld) = (
            Arc::clone(&queue),
            Arc::clone(&stop),
            Arc::clone(&latency_dirty),
        );
        let drain = move || {
            // SAFETY (both calls): `ctx` is an opaque key the C side maps
            // to its component; the Rust instance is never dereferenced
            // through it, and this thread is joined before the component
            // is freed.
            if ld.swap(false, Ordering::AcqRel) {
                unsafe { moose_au_v2_host_latency_changed(ctx.as_ptr().cast_mut()) };
            }
            while let Some((id, value)) = q.pop() {
                unsafe { moose_au_v2_host_set_param(ctx.as_ptr().cast_mut(), id, value) };
            }
        };
        let handle = thread::Builder::new()
            .name("moose-au-param-notify".to_string())
            .spawn(move || {
                while !s.load(Ordering::Acquire) {
                    drain();
                    // Poll off-thread at a short bounded cadence. The audio
                    // callback only writes the lock-free queue / dirty bit;
                    // it never invokes the scheduler through `Thread::unpark`.
                    thread::park_timeout(std::time::Duration::from_millis(4));
                }
                // Flush anything queued between the last drain and stop.
                drain();
            })
            .ok()?;
        let thread = handle.thread().clone();
        Some(Self {
            queue,
            stop,
            latency_dirty,
            thread,
            handle: Some(handle),
        })
    }

    /// Flag a latency change for the polling notifier. Audio-thread work is
    /// one coalescing atomic store; waking and host callbacks stay off-thread.
    pub(crate) fn notify_latency(&self) {
        self.latency_dirty.store(true, Ordering::Release);
    }
}

#[cfg(target_os = "macos")]
impl Drop for ParamNotifier {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.unpark();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) struct AuInstance<P: PluginExport> {
    // First fields: close worker requests before any native/editor teardown.
    pub(crate) persist_signal: Option<Arc<PersistChangeSignal>>,
    pub(crate) host_queue: Arc<HostQueue>,
    #[cfg(target_os = "macos")]
    pub(crate) persist_dispatch: Option<persist_notify::Dispatcher>,
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
    /// editor's `get_meter` reads these atomic slots instead
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
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()`. Updated by the audio thread (or `cb_reset`).
    pub(crate) latency_cache: AtomicU32,
    pub(crate) tail_cache: AtomicU32,
    /// Current render mode as a [`ProcessMode`] discriminant. The host
    /// toggles AU offline rendering on the main thread (`cb_set_render_
    /// mode`); `cb_reset` and every `cb_process` block read it so a
    /// bounce can relax realtime discipline. Defaults to `Realtime` (0).
    pub(crate) render_mode: AtomicU8,
    /// AU v2 (macOS) process-emitted `ParamChange` feedback pump. Set
    /// once in `cb_create` (after the instance has its final address);
    /// `None` only if the notifier thread failed to spawn.
    #[cfg(target_os = "macos")]
    pub(crate) param_notify: Option<ParamNotifier>,
    /// Cached param-info table for the chunker's split predicate.
    pub(crate) param_infos: Arc<moose_params::ParamCatalog>,
    /// The automation mode from `moose.toml`'s `[automation]`.
    pub(crate) automation: moose_core::Automation,
    pub(crate) input_bus_count: usize,
    pub(crate) output_bus_count: usize,
    /// AU exposes one main input plus one sidechain element.
    pub(crate) sidechain_channels: u32,
    pub(crate) plugin_id_hash: u64,
    /// Audio + lifecycle-owned per-block scratch. Behind a `PluginCell` so
    /// every callback reaches it through a shared `&AuInstance` - never a
    /// whole-struct `&mut *ctx`, which would alias a concurrent host-thread
    /// `&*ctx` (param reads, GUI) and is UB under the aliasing model.
    /// `cb_process` on the audio thread and the lifecycle callbacks never
    /// overlap, so the cell is never held twice at once.
    pub(crate) audio: PluginCell<AuAudio<P>>,
    /// Main/UI-thread-owned editor state, behind a `PluginCell` for the same
    /// reason. Its owners - the GUI callbacks and `cb_state_load`'s
    /// editor-notify - all run on the host main thread; `cb_process` never
    /// touches it.
    pub(crate) gui: PluginCell<AuGui>,
    /// Deferred editor resize requested through `PluginContext::request_resize`
    /// while the `gui` cell was already held (author code re-entering from
    /// inside `set_size` / `state_changed`). Packed `(w << 32) | h`, both
    /// non-zero, `0` = none. The closure stashes here rather than re-entering
    /// the cell; `cb_gui_get_size` applies it before reporting the size. GUI
    /// thread only; atomic for interior mutability through the shared `&inst`.
    pub(crate) pending_resize: AtomicU64,
    /// Shared transport slot: audio thread writes each block, editor reads.
    pub(crate) transport_slot: Arc<TransportSlot>,
    /// Bounded SPSC handoff for state loads. Host (`cb_state_load`)
    /// and editor (`set_state` callback) deserialize on their thread
    /// and push the result; the audio thread pops at the top of
    /// `cb_process` and calls [`state::apply_state`]
    /// under its exclusive `&mut plugin`.
    pub(crate) pending_state: Arc<StateLoadQueue>,
    /// Set by a panic in `process`; cleared by the next successful prepare.
    pub(crate) panic_latch: moose_core::wrapper::PanicLatch,
    /// `SysEx` input queued from the host thread (`MusicDeviceSysEx`, which
    /// AU v2 may call off the render thread) plus the free-list it recycles
    /// through. Kept off the `audio` cell so the host thread never aliases
    /// the audio-thread-owned scratch; `cb_process` drains `sysex_ready`
    /// at the block top. See [`SysExQueue`].
    pub(crate) sysex_ready: SysExQueue,
    pub(crate) sysex_free: SysExQueue,
    /// `legacy_au_keys` from `PluginInfo`, NUL-terminated for the
    /// shim's `ClassInfo` foreign-key probe. Built once at create;
    /// pointers handed out via `cb_legacy_state_key_at` stay valid
    /// for the instance lifetime.
    pub(crate) legacy_key_cstrings: Vec<CString>,
}

/// Audio + lifecycle-owned per-block scratch (see [`AuInstance::audio`]).
pub(crate) struct AuAudio<P: PluginExport> {
    pub(crate) event_list: EventList,
    pub(crate) output_events: EventList,
    /// Resume point for the appex's sequential `output_ump_at` drain;
    /// reset alongside `output_events` each block.
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
    pub(crate) native_output_param_feedback_available: bool,
    pub(crate) native_output_status: u32,
    /// Per-sub-block scratch for `chunked_process::process_chunked`.
    pub(crate) sub_event_scratch: EventList,
    /// Per-instance UMP `SysEx` reassembler. AU v3 hosts deliver
    /// long `SysEx` payloads as a chain of `SysEx`-7 (6-byte) or
    /// `SysEx`-8 (13-byte) UMPs; the assembler concatenates them
    /// into one logical [`EventBody::SysEx`] before pushing to the
    /// plugin's `event_list`. Holds
    /// [`moose_core::ump::SYSEX_ASSEMBLER_SLOTS`] ×
    /// [`SYSEX_POOL_PREALLOC`] (4 × 128 KiB = 512 KiB) of buffer
    /// space so concurrent streams across UMP groups don't bleed
    /// into each other. Cleared at the top of `cb_process` so a
    /// partial message can't bleed across blocks.
    pub(crate) sysex_assembler: SysExAssembler,
    pub(crate) sample_rate: f64,
    /// Max block size declared by the host via
    /// `kAudioUnitProperty_MaximumFramesPerSlice` (delivered through
    /// `cb_reset`'s `max_frames`). A generous default keeps the
    /// contract assert in `cb_process` from tripping for hosts that
    /// send process before declaring a max.
    pub(crate) max_block_size: usize,
    /// `true` once `cb_reset` has run. `cb_process` early-returns and
    /// zeros outputs while false so DSP doesn't run with un-snapped
    /// smoothers / unset sample rate.
    pub(crate) prepared: bool,
    /// `ProcessMode` discriminant the last `prepare` ran with.
    pub(crate) prepared_mode: u8,
    /// Reused per-block scratch for `RawBufferScratch::build`. Lives
    /// on the instance so the audio thread doesn't heap-allocate.
    ///
    /// Parameterised by `P::Sample`; widens/narrows host-`f32`
    /// buffers around `plugin.process()` for plugins on `prelude64`.
    pub(crate) scratch: RawBufferScratch<<P as PluginRuntime>::Sample>,
}

/// Main/UI-thread-owned editor state (see [`AuInstance::gui`]).
pub(crate) struct AuGui {
    pub(crate) editor: Option<Box<dyn Editor>>,
    /// The host-owned container view the editor last opened in (`0` = none).
    /// `cb_destroy` detaches it so a view outliving its `AudioUnit` never
    /// calls back into the freed instance.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(dead_code, reason = "only read by the macOS view detach and fit")
    )]
    pub(crate) view: usize,
}
