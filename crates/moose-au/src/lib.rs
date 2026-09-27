//! Audio Unit v3 format wrapper for moose.
//!
//! Uses an Objective-C shim compiled via `cc` that implements the
//! `AUAudioUnit` subclass. The shim calls back into Rust for all
//! plugin logic via C FFI.

pub mod ffi;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::slice;

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
// The AU v2 param-notify pump is macOS-only (v2 doesn't exist on iOS).
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "macos")]
use std::thread::{self, JoinHandle, Thread};

// `Float::from_f64` is only invoked from the macOS-only `set_param`
// closure in `cb_gui_open` (the AU v2 host notifier path). Gate the
// import so iOS builds, which take a `_id`-no-op branch instead,
// don't flag it as unused.
#[cfg(target_os = "macos")]
use moose_core::Float;
use moose_core::SYSEX_POOL_PREALLOC;
use moose_core::cast::{len_u32, sample_pos_i64};
use moose_core::editor::Editor;
// `ClosureBridge`, `PluginContext`, `SendPtr`, `RawWindowHandle` are
// consumed only inside the apple-gated body of `cb_gui_open` - the
// AppKit/UiKit variants don't exist on Linux/Windows. Importing them
// from a non-apple module would also trigger the unused-import lint
// there.
use moose_core::chunked_process::{ChunkedProcess, process_chunked_with_bus_routing};
use moose_core::config::{AudioConfig, ProcessMode};
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::editor::{ClosureBridge, PluginContext, RawWindowHandle, SendPtr};
// Used by `cb_gui_set_size`, which the platform-agnostic `AuCallbacks` FFI
// struct references on every target, so this import can't be apple-gated.
use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::bus::{BusConfig, BusKind, BusLayout};
use moose_core::bus_routing::{BusActivation, BusRouting, bus_layouts_fit_routing};
use moose_core::editor::fit_logical_size;
use moose_core::events::{
    AuEventMetadata, EVENT_LIST_PREALLOC, Event, EventBody, EventList, ExactEvent, ExactEventBody,
    ExactEventMetadata, ExactEventQualifiers, LosslessEventCursor, LosslessEventRef,
    OutputEventStatus, PushError, RawMidi1, RawUmp, TransportInfo,
};
use moose_core::export::PluginExport;
use moose_core::info::{MidiDialect, PluginInfo, resolve_name_override};
// The AU editor (and its meter reads) exist on macOS / iOS only,
// matching the `meter_store` field's gate.
use moose_core::editor::EditorBuilder;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::meters::MeterStore;
use moose_core::midi::{
    decode_short_message, downconvert_to_midi1, event_to_midi1, pitch_bend_to_bytes,
    route_midi_port, upconvert_to_midi2,
};
use moose_core::plugin::PluginRuntime;
use moose_core::presets::{PresetScope, enumerate_scope, load_preset_file};
use moose_core::rt::{RtSection, audit};
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
// Only the Apple-only editor path reads the spawner (see `AuInstance`).
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::tasks::AnyTaskSpawner;
use moose_core::ump::{
    SysExAssembler, SysExFeed, decode_ump_channel_voice_2, encode_sysex7_packet,
    encode_ump_channel_voice_1, encode_ump_channel_voice_2, sysex7_packet_count,
};
use moose_core::wrapper::{
    ParamCStrings, PluginCell, SharedPlugin, copy_c_str, default_io_channels, enter_plugin,
    log_missing_bus_layout, run_audio_block, run_extern_callback_with, run_register, save_extra,
    shared_plugin,
};
use moose_params::{MidiSource, ParamFlags, ParamInfo, Params};

use ffi::{
    AU_NATIVE_CARRIER_BYTES, AU_NATIVE_CARRIER_UMP, AU_NATIVE_EVENT_MIDI1, AU_NATIVE_EVENT_SYSEX,
    AU_NATIVE_EVENT_UMP, AU_OUTPUT_EMITTED, AU_OUTPUT_END, AU_OUTPUT_INVALID, AU_OUTPUT_QUEUE_FULL,
    AU_OUTPUT_UNSUPPORTED, AU_PROCESS_INVALID, AU_PROCESS_OK, AU_PROCESS_QUEUE_FULL, AuCallbacks,
    AuMidi2Event, AuMidiEvent, AuNativeEvent, AuParamDescriptor, AuParamEvent, AuPluginDescriptor,
    AuTransportSnapshot, AuUmpEvent,
};

// ---------------------------------------------------------------------------
// Instance wrapper - one per plugin instance, stored as the opaque ctx
// ---------------------------------------------------------------------------

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
type StateLoadQueue = crossbeam_queue::ArrayQueue<state::DeserializedState>;

/// One `SysEx` message handed from the host thread to the audio thread.
/// Recycled through a free-list (see [`SysExQueue`]) so the audio thread
/// never allocates or frees: it copies the bytes into the event pool and
/// returns the buffer.
struct SysExInput {
    sample_offset: u32,
    bytes: Vec<u8>,
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
type SysExQueue = crossbeam_queue::ArrayQueue<SysExInput>;

/// `SysEx` input buffers cycling through [`SysExQueue`]. Generous for the
/// small control messages (MMC, device inquiry, MPE config) that are the
/// realistic input to an effect / instrument between two render blocks.
const SYSEX_INPUT_SLOTS: usize = 64;

/// Host-thread side of the `SysEx` input handoff: copy one message into a
/// recycled buffer and queue it. Drops the message if no buffer is free
/// (the audio thread hasn't drained - queue saturated) rather than
/// building an unbounded backlog. Never touches the audio-thread scratch,
/// so it is safe to call from whatever thread the host runs
/// `MusicDeviceSysEx` on.
fn try_queue_sysex_input(
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

fn queue_sysex_input(free: &SysExQueue, ready: &SysExQueue, sample_offset: u32, bytes: &[u8]) {
    let _ = try_queue_sysex_input(free, ready, sample_offset, bytes);
}

/// Audio-thread side: drain every queued `SysEx` into `event_list`,
/// returning each buffer to `free`. Lock-free, and it only copies into
/// the pre-allocated event pool and moves buffer handles between queues -
/// no allocation or free on the audio thread.
fn drain_sysex_input(ready: &SysExQueue, free: &SysExQueue, event_list: &mut EventList) {
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
type ParamNotifyQueue = crossbeam_queue::ArrayQueue<(u32, f32)>;

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
struct ParamNotifier {
    queue: Arc<ParamNotifyQueue>,
    stop: Arc<AtomicBool>,
    /// Set by the audio thread when `latency()` changes; drained on the
    /// same wakeup as the param queue. The notifier thread broadcasts a
    /// `kAudioUnitProperty_Latency` change, off the audio thread.
    latency_dirty: Arc<AtomicBool>,
    /// Cached for a cheap `unpark` from the audio thread.
    thread: Thread,
    handle: Option<JoinHandle<()>>,
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
    unsafe fn spawn(ctx: SendPtr<std::ffi::c_void>) -> Option<Self> {
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
    fn notify_latency(&self) {
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

struct AuInstance<P: PluginExport> {
    /// The plugin in the wrapper-standard ownership cell: the audio
    /// thread owns it per block, host lifecycle callbacks own it while
    /// processing is stopped, and the two never overlap. `cb_state_save`
    /// and the editor's `get_state` read the lock-free snapshot instead,
    /// so they never touch it. See `moose_core::wrapper::SharedPlugin`.
    plugin: SharedPlugin<P>,
    /// Stable handle to the params Arc, set once at instance creation.
    /// Host-thread callbacks (`cb_param_*`) read params through this
    /// handle so a param query never touches the plugin.
    /// Params are atomic-backed and `Sync`.
    params_arc: Arc<P::Params>,
    /// Shared meter storage, set once at instance creation. The
    /// editor's `get_meter` closure reads these atomic slots instead
    /// of the plugin instance. Editor wiring is macOS/iOS-only (the
    /// AU GUI section), so the field follows it.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    meter_store: Arc<MeterStore>,
    /// Lock-free custom-state slot the audio thread publishes
    /// into, read by `save_state` so a snapshot-capable plugin's
    /// save never touches the plugin. Cached on the instance.
    snapshot: Arc<SnapshotSlot>,
    /// Background-task spawner (`None` unless the plugin wired `tasks:`),
    /// cached at creation so the editor schedules without touching the plugin. Only
    /// the editor path reads it, and that path is Apple-only (`AppKit` /
    /// `UIKit`); off-Apple the field would be dead.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    task_spawner: Option<AnyTaskSpawner>,
    /// Lock-free editor factory, cached at creation - building
    /// the editor never touches the plugin (`--shell` rebuilds
    /// from the reloaded dylib, so GUI edits hot-reload).
    editor_builder: EditorBuilder<P::Params>,
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()`. Updated by the audio thread (or `cb_reset`).
    latency_cache: AtomicU32,
    tail_cache: AtomicU32,
    /// Current render mode as a [`ProcessMode`] discriminant. The host
    /// toggles AU offline rendering on the main thread (`cb_set_render_
    /// mode`); `cb_reset` and every `cb_process` block read it so a
    /// bounce can relax realtime discipline. Defaults to `Realtime` (0).
    render_mode: AtomicU8,
    /// AU v2 (macOS) process-emitted `ParamChange` feedback pump. Set
    /// once in `cb_create` (after the instance has its final address);
    /// `None` only if the notifier thread failed to spawn.
    #[cfg(target_os = "macos")]
    param_notify: Option<ParamNotifier>,
    /// Cached param-info table for the chunker's split predicate.
    param_infos: Vec<ParamInfo>,
    /// `min_subblock_samples` from `moose.toml`'s `[automation]`.
    min_subblock_samples: u32,
    input_bus_count: usize,
    output_bus_count: usize,
    /// AU exposes one main input plus one sidechain element.
    sidechain_channels: u32,
    plugin_id_hash: u64,
    /// Audio + lifecycle-owned per-block scratch. Behind a `PluginCell` so
    /// every callback reaches it through a shared `&AuInstance` - never a
    /// whole-struct `&mut *ctx`, which would alias a concurrent host-thread
    /// `&*ctx` (param reads, GUI) and is UB under the aliasing model.
    /// `cb_process` on the audio thread and the lifecycle callbacks never
    /// overlap, so the cell is never held twice at once.
    audio: PluginCell<AuAudio<P>>,
    /// Main/UI-thread-owned editor state, behind a `PluginCell` for the same
    /// reason. Its owners - the GUI callbacks and `cb_state_load`'s
    /// editor-notify - all run on the host main thread; `cb_process` never
    /// touches it.
    gui: PluginCell<AuGui>,
    /// Deferred editor resize requested through `PluginContext::request_resize`
    /// while the `gui` cell was already held (author code re-entering from
    /// inside `set_size` / `state_changed`). Packed `(w << 32) | h`, both
    /// non-zero, `0` = none. The closure stashes here rather than re-entering
    /// the cell; `cb_gui_get_size` applies it before reporting the size. GUI
    /// thread only; atomic for interior mutability through the shared `&inst`.
    pending_resize: AtomicU64,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<TransportSlot>,
    /// Bounded SPSC handoff for state loads. Host (`cb_state_load`)
    /// and editor (`set_state` callback) deserialize on their thread
    /// and push the result; the audio thread pops at the top of
    /// `cb_process` and calls [`state::apply_state`]
    /// under its exclusive `&mut plugin`.
    pending_state: Arc<StateLoadQueue>,
    /// `SysEx` input queued from the host thread (`MusicDeviceSysEx`, which
    /// AU v2 may call off the render thread) plus the free-list it recycles
    /// through. Kept off the `audio` cell so the host thread never aliases
    /// the audio-thread-owned scratch; `cb_process` drains `sysex_ready`
    /// at the block top. See [`SysExQueue`].
    sysex_ready: SysExQueue,
    sysex_free: SysExQueue,
    /// `legacy_au_keys` from `PluginInfo`, NUL-terminated for the
    /// shim's `ClassInfo` foreign-key probe. Built once at create;
    /// pointers handed out via `cb_legacy_state_key_at` stay valid
    /// for the instance lifetime.
    legacy_key_cstrings: Vec<CString>,
}

/// Audio + lifecycle-owned per-block scratch (see [`AuInstance::audio`]).
struct AuAudio<P: PluginExport> {
    event_list: EventList,
    output_events: EventList,
    /// Resume point for the appex's sequential `output_ump_at` drain;
    /// reset alongside `output_events` each block.
    ump_drain_cursor: UmpDrainCursor,
    native_output_cursor: LosslessEventCursor,
    native_output_carriers: u32,
    native_output_num_frames: u32,
    native_output_ump_protocol: u8,
    native_output_max_absolute_offset: u32,
    native_output_ump_time_valid: bool,
    native_output_param_feedback_available: bool,
    native_output_status: u32,
    /// Per-sub-block scratch for `chunked_process::process_chunked`.
    sub_event_scratch: EventList,
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
    sysex_assembler: SysExAssembler,
    sample_rate: f64,
    /// Max block size declared by the host via
    /// `kAudioUnitProperty_MaximumFramesPerSlice` (delivered through
    /// `cb_reset`'s `max_frames`). A generous default keeps the
    /// contract assert in `cb_process` from tripping for hosts that
    /// send process before declaring a max.
    max_block_size: usize,
    /// `true` once `cb_reset` has run. `cb_process` early-returns and
    /// zeros outputs while false so DSP doesn't run with un-snapped
    /// smoothers / unset sample rate.
    prepared: bool,
    /// Reused per-block scratch for `RawBufferScratch::build`. Lives
    /// on the instance so the audio thread doesn't heap-allocate.
    ///
    /// Parameterised by `P::Sample`; widens/narrows host-`f32`
    /// buffers around `plugin.process()` for plugins on `prelude64`.
    scratch: RawBufferScratch<<P as PluginRuntime>::Sample>,
}

/// Main/UI-thread-owned editor state (see [`AuInstance::gui`]).
struct AuGui {
    editor: Option<Box<dyn Editor>>,
}

// ---------------------------------------------------------------------------
// Intentional leaks
//
// Every `CString::into_raw()` and `Vec::leak()` / `param_descs.leak()`
// in this file feeds a `*const c_char` (or `*const SomeDesc`) into a
// descriptor that the AU host caches for the process lifetime. Hosts
// re-read these pointers on demand (display, parameter sweeps,
// validation) - there's no signal back to Rust saying "you may free
// this now". Freeing is therefore unsound.
//
// The leak is bounded: O(plugin_count × (param_count + a few strings))
// per process, allocated once at registration time. No leak per audio
// callback, per render, per editor open. AU bundles get unloaded with
// the host process, which reclaims the allocation.
//
// `Box::into_raw(boxed_instance)` in `cb_create` follows the same
// pattern but is *paired* with `cb_destroy` reconstituting the Box -
// so it isn't a leak, just a C-lifetime handoff.
//
// ---------------------------------------------------------------------------
// C callback implementations (generic over P)
//
// SAFETY for all unsafe extern "C" fn below:
// - `ctx` is a *mut c_void created by Box::into_raw in cb_create().
//   Valid until cb_destroy() (called exactly once by the AU shim).
// - The AU v2 shim (au_v2_shim.c) and v3 shim (au_shim.m) own the
//   Rust context. The AU host guarantees: render callback on the
//   audio thread with exclusive access; all other callbacks on the
//   main thread, serialized.
// - Audio buffer pointers come from the host's AudioBufferList and
//   are valid for the declared channel count × frame count.
// - MIDI events come from MusicDeviceMIDIEvent (v2) or
//   AURenderEvent linked list (v3).
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_create<P: PluginExport>() -> *mut std::ffi::c_void {
    // `P::create` + author `init` run here; firewall them so a panic can't
    // unwind across the C ABI (null = "instantiation failed").
    run_extern_callback_with::<P, *mut std::ffi::c_void>(
        "au",
        "create",
        std::ptr::null_mut(),
        || {
            let mut plugin = P::create();
            plugin.init();
            let info = P::info();
            let layouts = P::bus_layouts();
            let input_bus_count = layouts.first().map_or(0, |layout| layout.inputs.len());
            let output_bus_count = layouts.first().map_or(0, |layout| layout.outputs.len());
            let sidechain_channels = layouts
                .first()
                .and_then(|layout| layout.inputs.get(1))
                .map_or(0, |bus| bus.channels.channel_count());
            let param_infos = plugin.params().param_infos();
            let params_arc = plugin.params_arc();
            let latency_cache = AtomicU32::new(plugin.latency());
            let tail_cache = AtomicU32::new(plugin.tail());
            let instance = Box::new(AuInstance::<P> {
                params_arc,
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                meter_store: plugin.meter_store(),
                snapshot: plugin.snapshot_slot(),
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                task_spawner: plugin.task_spawner(),
                editor_builder: plugin.editor_builder(),
                plugin: shared_plugin(plugin),
                latency_cache,
                tail_cache,
                render_mode: AtomicU8::new(ProcessMode::Realtime.as_u8()),
                #[cfg(target_os = "macos")]
                param_notify: None,
                param_infos,
                min_subblock_samples: info.automation.min_subblock_samples,
                input_bus_count,
                output_bus_count,
                sidechain_channels,
                plugin_id_hash: state::shared_plugin_state_hash(&info),
                audio: PluginCell::new(AuAudio {
                    event_list: EventList::with_capacity(EVENT_LIST_PREALLOC),
                    output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
                    ump_drain_cursor: UmpDrainCursor::HEAD,
                    native_output_cursor: LosslessEventCursor::default(),
                    native_output_carriers: 0,
                    native_output_num_frames: 0,
                    native_output_ump_protocol: 0,
                    native_output_max_absolute_offset: u32::MAX,
                    native_output_ump_time_valid: true,
                    native_output_param_feedback_available: false,
                    native_output_status: AU_OUTPUT_END,
                    sub_event_scratch: EventList::with_capacity(EVENT_LIST_PREALLOC),
                    sysex_assembler: SysExAssembler::with_capacity(SYSEX_POOL_PREALLOC),
                    sample_rate: 44100.0,
                    max_block_size: 8192,
                    prepared: false,
                    scratch: RawBufferScratch::default(),
                }),
                gui: PluginCell::new(AuGui { editor: None }),
                pending_resize: AtomicU64::new(0),
                transport_slot: TransportSlot::new(),
                pending_state: Arc::new(StateLoadQueue::new(1)),
                sysex_ready: SysExQueue::new(SYSEX_INPUT_SLOTS),
                sysex_free: {
                    // Prefill the free-list so the host thread has a buffer to
                    // pop. Every payload buffer is fully reserved here so a
                    // host that invokes SysEx on the audio thread cannot make
                    // the queue allocate.
                    let free = SysExQueue::new(SYSEX_INPUT_SLOTS);
                    for _ in 0..SYSEX_INPUT_SLOTS {
                        let _ = free.push(SysExInput::default());
                    }
                    free
                },
                legacy_key_cstrings: info
                    .legacy_au_keys
                    .iter()
                    .filter_map(|k| CString::new(*k).ok())
                    .collect(),
            });
            let raw = Box::into_raw(instance);
            // Spawn the param-notify thread now that the instance has its final
            // address: the AU shim will map exactly this pointer to its
            // component, and the notifier hands it back to the host-set FFI.
            #[cfg(target_os = "macos")]
            unsafe {
                // SAFETY: `raw` is live until `cb_destroy`, which drops
                // `param_notify` (joining the thread) before the shim frees the
                // component; the notifier only uses the pointer as an opaque key.
                let ctx = SendPtr::new(raw.cast::<std::ffi::c_void>().cast_const());
                (*raw).param_notify = ParamNotifier::spawn(ctx);
            }
            raw.cast::<std::ffi::c_void>()
        },
    )
}

unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        if !ctx.is_null() {
            // Wrap the drop in `catch_unwind`: dropping the
            // `AuInstance` cascades into the editor's `Drop`,
            // which tears down wgpu surfaces / `NSView` /
            // baseview / runloop timers. A panic anywhere in
            // that chain propagates across this `extern "C"`
            // boundary as UB - in practice the host catches it
            // as an Objective-C exception, `objc_exception_rethrow`
            // can't recover, and `std::terminate` aborts the host
            // (the REAPER / Cubase SIGABRT pattern). Hosts destroy
            // instances routinely mid-session - removing a plugin
            // from a track, closing a project - not just at quit,
            // so catching here keeps a live host alive; the
            // instance is being torn down anyway.
            let raw = ctx.cast::<AuInstance<P>>();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drop(Box::from_raw(raw));
            }));
        }
    }
}

unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    // Author `reset` runs here; firewall it so a panic can't unwind across
    // the C ABI.
    run_extern_callback_with::<P, ()>("au", "reset", (), || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut audio = inst.audio.enter();
        // Clamp host-supplied max_frames to a sane minimum.
        let max_frames = (max_frames as usize).max(1024);
        audio.sample_rate = sample_rate;
        audio.max_block_size = max_frames;
        // Match the exact AU flattening: main + first sidechain input and
        // one main output across only the layouts AU actually advertises.
        // AUv3's transport-only dummy output stays in the Swift adapter and
        // is never exposed to Rust processing.
        let (num_in, num_out) = au_process_capacity(&P::bus_layouts());
        audio
            .scratch
            .ensure_capacity(num_in as usize, num_out as usize, max_frames);
        // Host-set offline flag (`kAudioUnitProperty_OfflineRender` /
        // `isRenderingOffline`), forwarded by the shim before the host
        // (re)initializes. Prepares the plugin for the render mode the
        // host is about to drive.
        let mode = ProcessMode::from_u8(inst.render_mode.load(Ordering::Relaxed));
        {
            let mut plugin = enter_plugin(&inst.plugin);
            plugin.reset(&AudioConfig::new(sample_rate, max_frames).with_process_mode(mode));
            inst.latency_cache
                .store(plugin.latency(), Ordering::Relaxed);
            inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
        }
        audio.prepared = true;
    });
}

/// Host toggled AU offline rendering. `mode` is a [`ProcessMode`]
/// discriminant; stash it so `cb_reset` (buffer / quality prep) and
/// every `cb_process` block pick it up. Unknown values fold to
/// `Realtime` via [`ProcessMode::from_u8`].
unsafe extern "C" fn cb_set_render_mode<P: PluginExport>(ctx: *mut std::ffi::c_void, mode: u32) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        // Normalize through `from_u8` so an out-of-range discriminant
        // folds to `Realtime` before it reaches the audio thread.
        let disc = u8::try_from(mode).unwrap_or(0);
        inst.render_mode
            .store(ProcessMode::from_u8(disc).as_u8(), Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
enum AuProcessInput {
    Legacy {
        midi: *const AuMidiEvent,
        midi_len: u32,
        ump: *const AuMidi2Event,
        ump_len: u32,
    },
    Native {
        events: *const AuNativeEvent,
        len: u32,
        overflow: u32,
    },
}

fn push_exact_with_fallback(
    list: &mut EventList,
    exact: ExactEvent,
    fallback: Event,
) -> Result<(), PushError> {
    if list.len() >= list.capacity() {
        list.try_push_exact(exact)
    } else {
        list.try_push_with_exact(fallback, exact)
    }
}

fn ump_word_count_for_type(word0: u32) -> u8 {
    match ((word0 >> 28) & 0x0f) as u8 {
        0x0 | 0x1 | 0x2 | 0x6 | 0x7 => 1,
        0x3 | 0x4 | 0x8 | 0x9 | 0xA => 2,
        0xB | 0xC => 3,
        _ => 4,
    }
}

fn ump_protocol_accepts(protocol: u8, message_type: u8) -> bool {
    match message_type {
        0x2 => protocol == 1,
        0x4 => protocol == 2,
        _ => matches!(protocol, 1 | 2),
    }
}

fn declared_input_protocol<P: PluginExport>() -> u8 {
    if P::info().midi_input_dialect == MidiDialect::Midi2 {
        2
    } else {
        1
    }
}

fn set_midi1_group(body: &mut EventBody, group: u8) {
    match body {
        EventBody::NoteOn { group: value, .. }
        | EventBody::NoteOff { group: value, .. }
        | EventBody::ControlChange { group: value, .. }
        | EventBody::Aftertouch { group: value, .. }
        | EventBody::ChannelPressure { group: value, .. }
        | EventBody::PitchBend { group: value, .. }
        | EventBody::ProgramChange { group: value, .. } => *value = group,
        _ => {}
    }
}

fn declared_input_port<P: PluginExport>(port: u16) -> Option<u8> {
    (port < u16::from(P::info().midi_input_ports))
        .then(|| u8::try_from(port).ok())
        .flatten()
}

#[allow(clippy::too_many_lines)]
fn push_native_input<P: PluginExport>(
    list: &mut EventList,
    assembler: &mut SysExAssembler,
    ev: &AuNativeEvent,
    num_frames: u32,
) -> u32 {
    if ev.sample_offset >= num_frames {
        return AU_PROCESS_INVALID;
    }
    match ev.kind {
        AU_NATIVE_EVENT_MIDI1 => {
            let Ok(len) = u8::try_from(ev.data_len) else {
                return AU_PROCESS_INVALID;
            };
            let Some(message) = RawMidi1::new(ev.midi, len) else {
                return AU_PROCESS_INVALID;
            };
            let exact = ExactEvent::new(
                ev.sample_offset,
                ExactEventBody::Midi1 {
                    port: ev.port,
                    message,
                },
            );
            let fallback =
                decode_short_message(ev.midi[0], ev.midi[1], ev.midi[2]).filter(|body| {
                    typed_midi1_bytes(body).is_some_and(|(bytes, data_len)| {
                        let Ok(len) = usize::try_from(data_len) else {
                            return false;
                        };
                        data_len == ev.data_len && bytes[..len] == ev.midi[..len]
                    })
                });
            if let Some((port, body)) = declared_input_port::<P>(ev.port).zip(fallback) {
                if push_exact_with_fallback(
                    list,
                    exact,
                    Event::on_port(ev.sample_offset, port, body),
                )
                .is_err()
                {
                    return AU_PROCESS_QUEUE_FULL;
                }
            } else if list.try_push_exact(exact).is_err() {
                return AU_PROCESS_QUEUE_FULL;
            }
            AU_PROCESS_OK
        }
        AU_NATIVE_EVENT_SYSEX => {
            if ev.sysex.is_null() && ev.data_len != 0 {
                return AU_PROCESS_INVALID;
            }
            let bytes = if ev.data_len == 0 {
                &[][..]
            } else {
                unsafe { slice::from_raw_parts(ev.sysex, ev.data_len as usize) }
            };
            let Some(bytes) = bytes.strip_prefix(&[0xF0]) else {
                return AU_PROCESS_INVALID;
            };
            let Some(bytes) = bytes.strip_suffix(&[0xF7]) else {
                return AU_PROCESS_INVALID;
            };
            let exact = ExactEvent::new(ev.sample_offset, ExactEventBody::SysEx { port: ev.port });
            let Ok(token) = list.try_push_exact_sysex_token(ev.sample_offset, bytes, exact) else {
                return AU_PROCESS_QUEUE_FULL;
            };
            if let Some(port) = declared_input_port::<P>(ev.port)
                && list
                    .try_push_exact_sysex_view_companion(token, ev.sample_offset, port)
                    .is_err()
            {
                return AU_PROCESS_QUEUE_FULL;
            }
            AU_PROCESS_OK
        }
        AU_NATIVE_EVENT_UMP => {
            let Ok(word_count) = u8::try_from(ev.data_len) else {
                return AU_PROCESS_INVALID;
            };
            let Some(packet) = RawUmp::new(ev.words, word_count) else {
                return AU_PROCESS_INVALID;
            };
            let Some(metadata) = AuEventMetadata::new(ev.protocol) else {
                return AU_PROCESS_INVALID;
            };
            let mt = ((ev.words[0] >> 28) & 0x0F) as u8;
            if word_count != ump_word_count_for_type(ev.words[0])
                || ev.protocol != declared_input_protocol::<P>()
                || !ump_protocol_accepts(ev.protocol, mt)
            {
                return AU_PROCESS_INVALID;
            }
            let exact = ExactEvent::new(
                ev.sample_offset,
                ExactEventBody::Ump {
                    port: ev.port,
                    packet,
                },
            )
            .with_metadata(ExactEventMetadata::Au(metadata));
            let group = ((ev.words[0] >> 24) & 0x0F) as u8;
            let fallback = match mt {
                0x2 if word_count == 1 => {
                    let mut body = decode_short_message(
                        ((ev.words[0] >> 16) & 0xFF) as u8,
                        ((ev.words[0] >> 8) & 0xFF) as u8,
                        (ev.words[0] & 0xFF) as u8,
                    );
                    if let Some(body) = body.as_mut() {
                        set_midi1_group(body, group);
                    }
                    body.filter(|body| {
                        encode_ump_channel_voice_1(body)
                            .is_some_and(|words| words[0] == ev.words[0])
                    })
                }
                0x4 if word_count == 2 => decode_ump_channel_voice_2(ev.words)
                    .filter(|body| encode_ump_channel_voice_2(body).as_ref() == Some(&ev.words)),
                _ => None,
            };
            if let Some((port, body)) = declared_input_port::<P>(ev.port).zip(fallback) {
                return if push_exact_with_fallback(
                    list,
                    exact,
                    Event::on_port(ev.sample_offset, port, body),
                )
                .is_ok()
                {
                    AU_PROCESS_OK
                } else {
                    AU_PROCESS_QUEUE_FULL
                };
            }
            let Ok(token) = list.try_push_exact_token(exact) else {
                return AU_PROCESS_QUEUE_FULL;
            };
            let feed = match (mt, word_count) {
                (0x3, 2) => {
                    assembler.push_sysex7_packet_on_route(ev.port, [ev.words[0], ev.words[1]])
                }
                (0x5, 4) => assembler.push_sysex8_packet_on_route(ev.port, ev.words),
                _ => return AU_PROCESS_OK,
            };
            if let (Some(port), SysExFeed::Complete(payload)) =
                (declared_input_port::<P>(ev.port), feed)
                && list
                    .try_push_sysex_exact_companion(token, ev.sample_offset, port, payload.bytes)
                    .is_err()
            {
                return AU_PROCESS_QUEUE_FULL;
            }
            AU_PROCESS_OK
        }
        _ => AU_PROCESS_INVALID,
    }
}

unsafe extern "C" fn cb_process<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_input_channels: u32,
    num_output_channels: u32,
    num_frames: u32,
    events: *const AuMidiEvent,
    num_events: u32,
    events2: *const AuMidi2Event,
    num_events2: u32,
    param_events: *const AuParamEvent,
    num_param_events: u32,
    transport_ptr: *const AuTransportSnapshot,
) {
    unsafe {
        let _ = cb_process_impl::<P>(
            ctx,
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            num_frames,
            AuProcessInput::Legacy {
                midi: events,
                midi_len: num_events,
                ump: events2,
                ump_len: num_events2,
            },
            param_events,
            num_param_events,
            0,
            None,
            transport_ptr,
        );
    }
}

unsafe extern "C" fn cb_process_native<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_input_channels: u32,
    num_output_channels: u32,
    num_frames: u32,
    events: *const AuNativeEvent,
    num_events: u32,
    input_overflow: u32,
    param_events: *const AuParamEvent,
    num_param_events: u32,
    param_overflow: u32,
    transport_ptr: *const AuTransportSnapshot,
) -> u32 {
    unsafe {
        cb_process_impl::<P>(
            ctx,
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            num_frames,
            AuProcessInput::Native {
                events,
                len: num_events,
                overflow: input_overflow,
            },
            param_events,
            num_param_events,
            param_overflow,
            None,
            transport_ptr,
        )
    }
}

unsafe extern "C" fn cb_process_native_v11<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_input_channels: u32,
    num_output_channels: u32,
    input_bus_active: u32,
    output_bus_active: u32,
    num_frames: u32,
    events: *const AuNativeEvent,
    num_events: u32,
    input_overflow: u32,
    param_events: *const AuParamEvent,
    num_param_events: u32,
    param_overflow: u32,
    transport_ptr: *const AuTransportSnapshot,
) -> u32 {
    unsafe {
        cb_process_impl::<P>(
            ctx,
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            num_frames,
            AuProcessInput::Native {
                events,
                len: num_events,
                overflow: input_overflow,
            },
            param_events,
            num_param_events,
            param_overflow,
            Some((input_bus_active, output_bus_active)),
            transport_ptr,
        )
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
unsafe fn cb_process_impl<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_input_channels: u32,
    num_output_channels: u32,
    num_frames: u32,
    input: AuProcessInput,
    param_events: *const AuParamEvent,
    num_param_events: u32,
    param_overflow: u32,
    bus_active: Option<(u32, u32)>,
    transport_ptr: *const AuTransportSnapshot,
) -> u32 {
    let nf = num_frames as usize;
    let mut process_status = AU_PROCESS_OK;
    let ok = run_audio_block::<P>("AU", || unsafe {
        // Shared `&AuInstance` (never a whole-struct `&mut`) - the audio
        // scratch is reached through its ownership cell, so a concurrent
        // host-thread `&*ctx` (param reads, GUI) can't alias us.
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut audio = inst.audio.enter();
        let num_frames = nf;

        // Host called render before AU initialized us - sample rate
        // and smoothers haven't been primed. Zero outputs and bail.
        if !audio.prepared {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, num_frames);
                }
            }
            audio.event_list.clear();
            // Clear last block's output too: the shim drains
            // `output_*_count` unconditionally after every render, so
            // leaving these would re-deliver the previous block's
            // MIDI/SysEx (duplicated note-ons, stuck notes).
            audio.output_events.clear();
            audio.output_events.clear_overflow();
            audio.ump_drain_cursor = UmpDrainCursor::HEAD;
            // Not prepared: discard any host-queued `SysEx`, recycling the
            // buffers so the queue can't accumulate stale messages.
            while let Some(slot) = inst.sysex_ready.pop() {
                let _ = inst.sysex_free.push(slot);
            }
            return;
        }

        // Start the output transaction before any author-controlled state or
        // DSP can panic. `clear` preserves the preceding delivery status for
        // this block to inspect; clearing the old overflow ensures a failed
        // current block cannot inherit an earlier block's staging failure.
        audio.output_events.clear();
        audio.output_events.clear_overflow();
        audio.ump_drain_cursor = UmpDrainCursor::HEAD;

        // Take ownership of the plugin for the whole block: an
        // uncontended `Acquire`, never a wait, since the host contract
        // keeps `process` from overlapping a lifecycle callback and host
        // saves read the snapshot instead of the plugin. Enter through a
        // local Arc clone so the guard doesn't pin a borrow of `inst`.
        let plugin_arc = Arc::clone(&inst.plugin);
        let mut plugin = enter_plugin(&plugin_arc);

        // Apply any pending state-load before per-block work so the
        // plugin sees consistent params and extra state for the
        // entire block. See `pending_state` field comment for the
        // queue-overflow policy.
        if let Some(state) = inst.pending_state.pop() {
            state::apply_state(&mut *plugin, &state);
        }

        // Paranoid allocation check (the `rt-paranoid` feature): guard the
        // wrapper's per-block glue - event conversion, transport, process,
        // output encode, snapshot publish - as well as the plugin. Placed
        // after the state-load apply above, since `load_state` legitimately
        // allocates. No-op and zero-sized when the feature is off.
        let _rt = RtSection::enter();

        // One reborrow of the scratch so the disjoint fields below (event
        // list, assembler, sub-block scratch, output events, build scratch)
        // can be borrowed simultaneously - the guard's `Deref` can't
        // split-borrow.
        let scr = &mut *audio;

        // Start the block's event list fresh, then drain any `SysEx` the
        // host queued through `MusicDeviceSysEx` (the shim's `au_v2_sysex`
        // → `cb_au_push_sysex_input`). That callback may run on any host
        // thread, so it queues rather than touching this scratch: pop each
        // recycled buffer, copy its bytes into the pre-allocated event
        // pool, return the buffer to the free list. Lock-free, no alloc or
        // free on the audio thread. AU v3 `SysEx` arrives in-line via
        // `events2` below instead.
        scr.event_list.clear();
        scr.event_list.clear_overflow();
        drain_sysex_input(&inst.sysex_ready, &inst.sysex_free, &mut scr.event_list);
        scr.sysex_assembler.reset();
        match input {
            AuProcessInput::Legacy {
                midi,
                midi_len,
                ump,
                ump_len,
            } => {
                if !midi.is_null() && midi_len > 0 {
                    for ev in slice::from_raw_parts(midi, midi_len as usize) {
                        if let Some(body) = decode_short_message(ev.status, ev.data1, ev.data2) {
                            scr.event_list.push(Event {
                                sample_offset: ev.sample_offset,
                                port: 0,
                                body,
                            });
                        }
                    }
                }
                if !ump.is_null() && ump_len > 0 {
                    for ev in slice::from_raw_parts(ump, ump_len as usize) {
                        if let Some(body) = decode_ump_channel_voice_2(ev.words) {
                            scr.event_list.push(Event {
                                sample_offset: ev.sample_offset,
                                port: 0,
                                body,
                            });
                        }
                    }
                }
            }
            AuProcessInput::Native {
                events,
                len,
                overflow,
            } => {
                if overflow != 0 {
                    process_status = AU_PROCESS_QUEUE_FULL;
                    return;
                }
                if events.is_null() && len != 0 {
                    process_status = AU_PROCESS_INVALID;
                    return;
                }
                if !events.is_null() && len > 0 {
                    for event in slice::from_raw_parts(events, len as usize) {
                        let status = push_native_input::<P>(
                            &mut scr.event_list,
                            &mut scr.sysex_assembler,
                            event,
                            len_u32(num_frames),
                        );
                        if status != AU_PROCESS_OK {
                            process_status = status;
                            return;
                        }
                    }
                }
            }
        }

        // Host-side parameter automation. The AU v3 Swift shim
        // decodes `AURenderEvent.parameter` / `.parameterRamp`
        // entries into `AuParamEvent` rows with within-block
        // sample offsets; convert each into an
        // `EventBody::ParamChange` so the chunker
        // (`process_chunked` below) splits the audio block at the
        // automation point. Ramps are treated as a step at the
        // ramp's start - the plugin's own smoother handles the
        // actual interpolation, matching moose-vst3's treatment of
        // VST3 parameter queues. The v2 path passes
        // `param_events = NULL, num_param_events = 0` because AU v2
        // has no per-sample automation API at the format boundary.
        if param_overflow != 0 {
            process_status = AU_PROCESS_QUEUE_FULL;
            return;
        }
        if param_events.is_null() && num_param_events != 0 {
            process_status = AU_PROCESS_INVALID;
            return;
        }
        if !param_events.is_null() && num_param_events > 0 {
            let pe_slice = slice::from_raw_parts(param_events, num_param_events as usize);
            for pe in pe_slice {
                if pe.sample_offset >= len_u32(num_frames) {
                    process_status = AU_PROCESS_INVALID;
                    return;
                }
                scr.event_list.push(Event {
                    sample_offset: pe.sample_offset,
                    port: 0,
                    body: EventBody::ParamChange {
                        id: pe.param_id,
                        value: f64::from(pe.value),
                    },
                });
            }
        }
        if scr.event_list.overflow().is_some() {
            process_status = AU_PROCESS_QUEUE_FULL;
            return;
        }

        scr.event_list.ensure_sorted_by_offset();

        // Build AudioBuffer from raw pointers, reusing the per-instance scratch.
        debug_assert!(
            num_frames <= scr.max_block_size,
            "host violated AU contract: render() got {num_frames} frames \
             but kAudioUnitProperty_MaximumFramesPerSlice declared max {}",
            scr.max_block_size
        );
        // Fail safe in release: `scratch` is only pre-grown to
        // `max_block_size` (in `cb_reset`), so a host rendering more frames
        // than it declared - or after changing the max without
        // reinitializing - would make `scratch.build` grow on demand (heap
        // alloc + memcpy) on the audio thread, exactly the RT violation the
        // wrapper otherwise avoids. Zero the outputs and skip the block
        // instead of allocating.
        if num_frames > scr.max_block_size {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, num_frames);
                }
            }
            scr.event_list.clear();
            // Clear last block's output too: the shim drains
            // `output_*_count` unconditionally after every render, so
            // leaving these would re-deliver the previous block's
            // MIDI/SysEx (duplicated note-ons, stuck notes).
            scr.output_events.clear();
            scr.ump_drain_cursor = UmpDrainCursor::HEAD;
            return;
        }
        let mut audio_buffer = scr.scratch.build(
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            len_u32(num_frames),
            P::supports_in_place(),
        );
        let sidechain_channels = inst.sidechain_channels.min(num_input_channels);
        let main_input_channels = num_input_channels.saturating_sub(sidechain_channels);
        let activation = |direction_mask: Option<u32>, bus: usize| match direction_mask {
            Some(mask) if mask & (1_u32 << bus) == 0 => BusActivation::Inactive,
            Some(_) => BusActivation::Active,
            None => BusActivation::Unknown,
        };
        let mut bus_routing = BusRouting::new();
        for index in 0..inst.input_bus_count {
            let channels = match index {
                0 => main_input_channels,
                1 => sidechain_channels,
                _ => 0,
            };
            let state = if channels == 0 {
                BusActivation::Inactive
            } else {
                activation(bus_active.map(|m| m.0), index)
            };
            debug_assert!(bus_routing.push_input(channels, state));
        }
        for index in 0..inst.output_bus_count {
            let channels = if index == 0 { num_output_channels } else { 0 };
            let state = if channels == 0 {
                BusActivation::Inactive
            } else {
                activation(bus_active.map(|m| m.1), index)
            };
            debug_assert!(bus_routing.push_output(channels, state));
        }

        let transport = if !transport_ptr.is_null() && (*transport_ptr).valid != 0 {
            let t = &*transport_ptr;
            TransportInfo {
                playing: t.playing != 0,
                recording: t.recording != 0,
                tempo: t.tempo,
                // The two `as u8` casts are post-clamped to `0..=255`.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                time_sig_num: t.time_sig_num.clamp(0, i32::from(u8::MAX)) as u8,
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                time_sig_den: t.time_sig_den.clamp(0, i32::from(u8::MAX)) as u8,
                position_samples: sample_pos_i64(t.position_samples),
                // Derived from samples for a consistent cross-format value
                // (CLAP fills it directly). Guard the pre-reset zero SR.
                position_seconds: if scr.sample_rate > 0.0 {
                    t.position_samples / scr.sample_rate
                } else {
                    0.0
                },
                position_beats: t.position_beats,
                bar_start_beats: t.bar_start_beats,
                loop_active: t.loop_active != 0,
                loop_start_beats: t.loop_start_beats,
                loop_end_beats: t.loop_end_beats,
            }
        } else {
            TransportInfo::default()
        };
        inst.transport_slot.write(&transport);

        let mut transport_snap = transport;
        let chunk_args = ChunkedProcess {
            events: &scr.event_list,
            sub_event_scratch: &mut scr.sub_event_scratch,
            transport: &mut transport_snap,
            sample_rate: scr.sample_rate,
            // Host offline flag, forwarded by the shim (`cb_set_render_
            // mode`); read per block so a mid-session toggle applies to
            // the next block without waiting on a re-prep.
            process_mode: ProcessMode::from_u8(inst.render_mode.load(Ordering::Relaxed)),
            output_events: &mut scr.output_events,
            params_fn: None,
            meters_fn: None,
            param_infos: &inst.param_infos,
            min_subblock_samples: inst.min_subblock_samples,
        };
        process_chunked_with_bus_routing(
            &mut *plugin,
            inst.params_arc.as_ref() as &dyn Params,
            &mut audio_buffer,
            chunk_args,
            bus_routing,
        );
        let _ = audio_buffer;
        // Narrow rendered f64 output back to host f32 when needed.
        // No-op for `f32` plugins.
        scr.scratch
            .finish_widening(outputs, num_output_channels, len_u32(num_frames));
        scr.output_events.ensure_sorted_by_offset();

        // Refresh latency / tail caches so the host's main-thread
        // queries don't have to touch the plugin. On an actual
        // change, flag the polling notifier to broadcast a
        // `kAudioUnitProperty_Latency` change (AU v2 / macOS). AU v3
        // (iOS) has no Rust->appex notify path; its host re-reads the
        // cached value on its own, unchanged.
        let prev_latency = inst.latency_cache.swap(plugin.latency(), Ordering::Relaxed);
        #[cfg(target_os = "macos")]
        if prev_latency != inst.latency_cache.load(Ordering::Relaxed)
            && let Some(notifier) = &inst.param_notify
        {
            notifier.notify_latency();
        }
        #[cfg(not(target_os = "macos"))]
        let _ = prev_latency;
        inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
    });
    if !ok {
        process_status = AU_PROCESS_INVALID;
    }
    if !ok || process_status != AU_PROCESS_OK {
        unsafe {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, nf);
                }
            }
        }
    }
    process_status
}

/// Test-only smoke helper for the `rt-paranoid` CI gate: drives a few
/// real process blocks through this wrapper's per-block glue via the
/// shared `cb_process` body (null events / param events / transport,
/// small stereo buffers), returning the steady-state audio-thread
/// allocation count (0 = clean). Vacuously 0 unless the `rt-paranoid`
/// feature installs the checking allocator. Not public API.
#[doc(hidden)]
#[must_use]
pub fn rt_paranoid_smoke<P: PluginExport>() -> u32 {
    const FRAMES: u32 = 512;
    const CH: u32 = 2;
    let frames = FRAMES as usize;
    // SAFETY: constructs, drives, and destroys its own instance; all
    // pointers below outlive each `cb_process` call, buffers sized to
    // `FRAMES`, and the event / param / transport pointers are null
    // (which `cb_process` tolerates).
    unsafe {
        let ctx = cb_create::<P>();
        cb_reset::<P>(ctx, 48_000.0, FRAMES);

        let in_left = vec![0.5f32; frames];
        let in_right = vec![0.5f32; frames];
        let mut out_left = vec![0f32; frames];
        let mut out_right = vec![0f32; frames];
        let in_ptrs: [*const f32; 2] = [in_left.as_ptr(), in_right.as_ptr()];
        let mut out_ptrs: [*mut f32; 2] = [out_left.as_mut_ptr(), out_right.as_mut_ptr()];

        let mut count = 0;
        for _ in 0..3 {
            let ((), n) = audit(|| {
                cb_process::<P>(
                    ctx,
                    in_ptrs.as_ptr(),
                    out_ptrs.as_mut_ptr(),
                    CH,
                    CH,
                    FRAMES,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                );
            });
            count = n;
        }

        assert!(
            out_left.iter().any(|s| s.abs() > 0.0),
            "au smoke: process did not run (output stayed zero)"
        );
        cb_destroy::<P>(ctx);
        count
    }
}

unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        len_u32(inst.params_arc.count())
    }
}

unsafe extern "C" fn cb_param_get_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        inst.params_arc.get_plain(id).unwrap_or(0.0)
    }
}

unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        inst.params_arc.set_plain(id, value);
    }
}

unsafe extern "C" fn cb_param_format_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    // Author `format_value` can panic; firewall it (0 = no display text).
    run_extern_callback_with::<P, u32>("au", "format_value", 0, || unsafe {
        // `out_len == 0` would underflow on `out_len as usize - 1`
        // and let `copy_nonoverlapping` write past the host-supplied
        // buffer. Treat zero capacity as "host wants nothing".
        if out_len == 0 || out.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        match inst.params_arc.format_value(id, value) {
            Some(text) => len_u32(copy_c_str(out, out_len as usize, &text)),
            None => 0,
        }
    })
}

unsafe extern "C" fn cb_param_parse_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    text: *const c_char,
    out_plain: *mut f64,
) -> i32 {
    // Author `parse_value` can panic; firewall it (0 = "not parsed").
    run_extern_callback_with::<P, i32>("au", "parse_value", 0, || unsafe {
        if text.is_null() || out_plain.is_null() {
            return 0;
        }
        let Ok(text) = CStr::from_ptr(text).to_str() else {
            return 0;
        };
        let inst = &*ctx.cast::<AuInstance<P>>();
        // AU parameter values are plain (min..max), so `parse_value`'s
        // result goes straight through - no normalize step like VST3.
        match inst.params_arc.parse_value(id, text) {
            Some(v) => {
                *out_plain = v;
                1
            }
            None => 0,
        }
    })
}

unsafe extern "C" fn cb_state_save<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
    out_len: *mut u32,
) {
    // Pre-zero the out pointers so a panic anywhere in the body below
    // leaves the host seeing an empty blob rather than a stale buffer
    // pointer paired with whatever length was last written.
    unsafe {
        *out_data = std::ptr::null_mut();
        *out_len = 0;
    }
    run_extern_callback_with::<P, ()>("au", "save_state", (), || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let (ids, values) = inst.params_arc.collect_values();
        // Read the custom state from the lock-free snapshot the audio
        // thread publishes each block. Never touches the plugin, so it
        // neither stalls a block in flight nor races `process`.
        //
        // Allocator pin: this wrapper allocates with libc `malloc` and
        // the AU shim frees with libc `free`. The Rust global allocator
        // must not appear on either side; mixing allocators is UB.
        let extra = save_extra(&inst.snapshot);
        let persist = inst.params_arc.serialize_persist();
        let blob = state::serialize_state(inst.plugin_id_hash, &ids, &values, &extra, &persist);

        let len = blob.len();
        let ptr = malloc(len).cast::<u8>();
        if ptr.is_null() {
            // malloc failed - `*out_data` is already null and
            // `*out_len` already 0 from the pre-zero above.
            return;
        }
        std::ptr::copy_nonoverlapping(blob.as_ptr(), ptr, len);
        *out_data = ptr;
        *out_len = len_u32(len);
    });
}

unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) {
    run_extern_callback_with::<P, ()>("au", "load_state", (), || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        // `slice::from_raw_parts(null, n)` for `n > 0` is UB. Treat
        // `(null, *)` and `(_, 0)` the same as "host gave us nothing".
        if data.is_null() || len == 0 {
            return;
        }
        let blob = slice::from_raw_parts(data, len as usize);
        // Not this plugin's envelope? Offer the bytes to the plugin's
        // `migrate_state` hook (legacy sessions from a pre-moose
        // build that stored foreign bytes under moose's key, or a
        // renamed plugin's envelope).
        if let Some(deserialized) = state::parse_or_migrate::<P>(
            blob,
            inst.plugin_id_hash,
            state::PluginFormat::Au,
            Some("truce_state"),
        ) {
            // Apply params synchronously on the host thread (atomic-safe)
            // so host queries that read parameter values right after
            // `setFullState:` see the restored values without first
            // running a render block.
            state::apply_params(&*inst.params_arc, &deserialized);
            // Hand the deserialized state to the audio thread for
            // application. `force_push` overwrites any older pending
            // blob - see the `pending_state` field comment for why
            // newest-wins is the right policy.
            let _ = inst.pending_state.force_push(deserialized);
            if let Some(ref mut editor) = inst.gui.enter().editor {
                editor.state_changed();
            }
        }
    });
}

unsafe extern "C" fn cb_legacy_state_key_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        len_u32(inst.legacy_key_cstrings.len())
    }
}

unsafe extern "C" fn cb_legacy_state_key_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
) -> *const c_char {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        inst.legacy_key_cstrings
            .get(index as usize)
            .map_or(std::ptr::null(), |k| k.as_ptr())
    }
}

/// Bytes found under a legacy `ClassInfo` key (moose's own entry was
/// absent): offer them to the plugin's `migrate_state` hook and ride
/// the normal restore pipeline on acceptance. Returns 1 when the
/// plugin translated the bytes, 0 when it didn't recognize them.
unsafe extern "C" fn cb_state_load_foreign<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    key: *const c_char,
    data: *const u8,
    len: u32,
) -> i32 {
    run_extern_callback_with::<P, i32>("au", "migrate_state", 0, || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        if key.is_null() || data.is_null() || len == 0 {
            return 0;
        }
        let Ok(key) = std::ffi::CStr::from_ptr(key).to_str() else {
            return 0;
        };
        let bytes = slice::from_raw_parts(data, len as usize);
        let Some(migrated) = P::migrate_state(&state::ForeignState::Raw {
            format: state::PluginFormat::Au,
            source_key: Some(key),
            bytes,
        }) else {
            return 0;
        };
        let deserialized: state::DeserializedState = migrated.into();
        state::apply_params(&*inst.params_arc, &deserialized);
        let _ = inst.pending_state.force_push(deserialized);
        if let Some(ref mut editor) = inst.gui.enter().editor {
            editor.state_changed();
        }
        1
    })
}

unsafe extern "C" fn cb_state_free(data: *mut u8, _len: u32) {
    unsafe {
        if !data.is_null() {
            free(data.cast::<std::ffi::c_void>());
        }
    }
}

// ---------------------------------------------------------------------------
// Factory presets (kAudioUnitProperty_FactoryPresets backing)
// ---------------------------------------------------------------------------

/// One bundled factory preset: the display name handed to the shim
/// (C-string, lives for the process) plus the file to load.
struct FactoryPresetEntry {
    name: CString,
    path: std::path::PathBuf,
}

/// Lazily-enumerated factory presets from the component bundle's
/// `Contents/Resources/Presets/`. One static per shared library, like
/// the registration statics: an AU dylib ships exactly one plugin
/// type, so the single `OnceLock` never sees a second
/// monomorphization. Empty when the bundle ships no presets (or when
/// the dylib isn't inside a component bundle, e.g. the AU v3 appex
/// layout - the shim then reports the property as invalid).
static FACTORY_PRESETS: OnceLock<Vec<FactoryPresetEntry>> = OnceLock::new();

fn factory_presets<P: PluginExport>() -> &'static [FactoryPresetEntry] {
    FACTORY_PRESETS.get_or_init(|| {
        let Some(root) = component_presets_root::<P>() else {
            return Vec::new();
        };
        let info = P::info();
        let mut refs = enumerate_scope(
            &root,
            PresetScope::Factory,
            info.vendor,
            info.name,
            state::shared_plugin_state_hash(&info),
            info.preset_extension,
        );
        // The library's `default = true` preset leads the list: hosts
        // treat factory preset 0 as the de-facto initial sound. The
        // stable sort keeps the walk's alphabetical order behind it.
        refs.sort_by_key(|preset| !preset.default);
        refs.into_iter()
            .filter_map(|preset| {
                // Hosts show the factory list flat; keep the category
                // visible the way the LV2 labels do.
                let display = match &preset.category {
                    Some(category) => format!("{category}/{}", preset.name),
                    None => preset.name.clone(),
                };
                Some(FactoryPresetEntry {
                    name: CString::new(display).ok()?,
                    path: preset.path,
                })
            })
            .collect()
    })
}

/// Mirrors the layout `<libc/dlfcn.h>` defines; bound directly like
/// the `malloc` / `free` externs above to keep the crate free of a
/// libc dependency. Field names keep dlfcn's `dli_` prefix so they
/// line up with the C declaration they shadow.
#[repr(C)]
#[allow(clippy::struct_field_names)]
struct DlInfo {
    dli_fname: *const c_char,
    dli_fbase: *mut std::ffi::c_void,
    dli_sname: *const c_char,
    dli_saddr: *mut std::ffi::c_void,
}

unsafe extern "C" {
    fn dladdr(addr: *const std::ffi::c_void, info: *mut DlInfo) -> i32;
}

/// Resolve the factory-presets directory of the bundle this code lives
/// in, via `dladdr` on one of our own functions. Three layouts exist:
///
/// - AU v2 component (macOS): `<X>.component/Contents/MacOS/<X>` with
///   presets in `Contents/Resources/Presets/` (two levels up).
/// - AU v3 framework (macOS): `<F>.framework/Versions/A/<F>` with presets
///   in `Versions/A/Resources/Presets/` (one level up).
/// - AU v3 framework (iOS): `<F>.framework/<F>` - a shallow bundle, so
///   presets live in a flat `<F>.framework/Presets/` (one level up; a
///   `Resources/` subdir would make iOS installd reject the framework).
fn component_presets_root<P: PluginExport>() -> Option<std::path::PathBuf> {
    let mut info = DlInfo {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    let probe = cb_factory_preset_count::<P> as *const std::ffi::c_void;
    // SAFETY: `probe` is a function in this image; `dladdr` only
    // writes into the out-struct on success.
    if unsafe { dladdr(probe, &raw mut info) } == 0 || info.dli_fname.is_null() {
        return None;
    }
    // SAFETY: `dli_fname` is a NUL-terminated path owned by dyld.
    let exe = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
    let exe = std::path::Path::new(exe.to_str().ok()?);
    let parent = exe.parent()?;
    // Probe order: macOS `Resources/Presets` two levels up (AU v2
    // component) and one level up (AU v3 framework), then the iOS flat
    // `Presets/` one level up.
    [
        parent.parent().map(|d| d.join("Resources/Presets")),
        Some(parent.join("Resources/Presets")),
        Some(parent.join("Presets")),
    ]
    .into_iter()
    .flatten()
    .find(|root| root.is_dir())
}

unsafe extern "C" fn cb_factory_preset_count<P: PluginExport>(_ctx: *mut std::ffi::c_void) -> u32 {
    run_extern_callback_with::<P, u32>("au", "factory_preset_count", 0, || {
        len_u32(factory_presets::<P>().len())
    })
}

unsafe extern "C" fn cb_factory_preset_name<P: PluginExport>(
    _ctx: *mut std::ffi::c_void,
    index: u32,
) -> *const c_char {
    run_extern_callback_with::<P, *const c_char>(
        "au",
        "factory_preset_name",
        std::ptr::null(),
        || {
            factory_presets::<P>()
                .get(index as usize)
                .map_or(std::ptr::null(), |entry| entry.name.as_ptr())
        },
    )
}

unsafe extern "C" fn cb_factory_preset_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
) -> i32 {
    run_extern_callback_with::<P, i32>("au", "factory_preset_load", 0, || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(entry) = factory_presets::<P>().get(index as usize) else {
            return 0;
        };
        let Some(deserialized) = load_preset_file(&entry.path, inst.plugin_id_hash) else {
            return 0;
        };
        // Same apply path as cb_state_load: params synchronously on
        // the host thread, full state through the audio-thread
        // handoff, editor notified.
        state::apply_params(&*inst.params_arc, &deserialized);
        let _ = inst.pending_state.force_push(deserialized);
        if let Some(ref mut editor) = inst.gui.enter().editor {
            editor.state_changed();
        }
        1
    })
}

// ---------------------------------------------------------------------------
// Output event callbacks (plugin → host MIDI)
// ---------------------------------------------------------------------------

// UMP MIDI 2.0 CV decoder lives in `moose-core::ump` so the same
// codec backs CLAP's `CLAP_EVENT_MIDI2` path and AU's MIDIEventList
// path.

/// Map a moose `Event` body to a 3-byte AU MIDI packet. Returns
/// `None` for event types that don't fit (MIDI 2.0, `ParamChange`,
/// Transport, etc.).
fn try_encode_au_midi(event: &Event) -> Option<AuMidiEvent> {
    // The MIDI 1.0 byte output path (AU v2, and AU v3 in 1.0-protocol
    // mode). AU v3 in 2.0 mode emits UMP via `try_encode_au_ump`, so a
    // 2.0 variant only reaches here on a 1.0 transport - down-convert it.
    let body = downconvert_to_midi1(&event.body).unwrap_or(event.body);
    let (status, data1, data2) = match &body {
        EventBody::NoteOn {
            channel,
            note,
            velocity,
            ..
        } => (0x90 | (channel & 0x0F), *note, *velocity),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
            ..
        } => (0x80 | (channel & 0x0F), *note, *velocity),
        EventBody::ControlChange {
            channel, cc, value, ..
        } => (0xB0 | (channel & 0x0F), *cc, *value),
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
            ..
        } => (0xA0 | (channel & 0x0F), *note, *pressure),
        EventBody::ChannelPressure {
            channel, pressure, ..
        } => (0xD0 | (channel & 0x0F), *pressure, 0),
        EventBody::PitchBend { channel, value, .. } => {
            let (lsb, msb) = pitch_bend_to_bytes(*value);
            (0xE0 | (channel & 0x0F), lsb, msb)
        }
        EventBody::ProgramChange {
            channel, program, ..
        } => (0xC0 | (channel & 0x0F), *program, 0),
        _ => return None,
    };
    Some(AuMidiEvent {
        sample_offset: event.sample_offset,
        status,
        data1,
        data2,
        port: event.port,
    })
}

/// Plugin latency in samples, for the host's delay compensation.
/// Reads the atomic cache the audio thread refreshes each block (and
/// `cb_reset` / `cb_create` seed) rather than touching the plugin,
/// so a host's main-thread property query never contends with render.
unsafe extern "C" fn cb_latency_samples<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        if ctx.is_null() {
            return 0;
        }
        (*ctx.cast::<AuInstance<P>>())
            .latency_cache
            .load(Ordering::Relaxed)
    }
}

/// Plugin release-tail length in samples. Same cache/threading story
/// as [`cb_latency_samples`].
unsafe extern "C" fn cb_tail_samples<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        if ctx.is_null() {
            return 0;
        }
        (*ctx.cast::<AuInstance<P>>())
            .tail_cache
            .load(Ordering::Relaxed)
    }
}

unsafe extern "C" fn cb_output_event_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let audio = inst.audio.enter();
        let n = audio
            .output_events
            .iter()
            .filter(|e| try_encode_au_midi(e).is_some())
            .count();
        len_u32(n)
    }
}

unsafe extern "C" fn cb_output_event_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut AuMidiEvent,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let audio = inst.audio.enter();
        if let Some(mut packet) = audio
            .output_events
            .iter()
            .filter_map(try_encode_au_midi)
            .nth(index as usize)
        {
            // Route to the plugin's chosen MIDI output cable; an
            // out-of-range port lands on 0. The appex passes `port`
            // straight to `midiOutputEventBlock`.
            packet.port = route_midi_port(packet.port, P::info().midi_output_ports);
            *out = packet;
        }
    }
}

/// The UMP protocol a `MIDIEventList` stream is declared in. Crosses
/// the appex FFI as a `u32` (`1` / `2`); anything else defaults to
/// 2.0, the protocol `CoreMIDI` hosts translate natively.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UmpProtocol {
    Midi1,
    Midi2,
}

impl UmpProtocol {
    fn from_wire(protocol: u32) -> Self {
        if protocol == 1 {
            Self::Midi1
        } else {
            Self::Midi2
        }
    }
}

/// Encode a channel-voice event into UMP for AU v3's `MIDIEventList`
/// output, in the stream's declared protocol. The UMP spec forbids
/// mixing MT 0x2 (1.0 CV) and MT 0x4 (2.0 CV) packets in one protocol
/// stream, so events convert across dialects instead: a 1.0 body
/// widens for a 2.0 stream, a 2.0 body narrows (lossy) for a 1.0
/// stream, and a 2.0-only body with no 1.0 form drops there. Returns
/// `None` for bodies that aren't channel voice (`SysEx` flattens to
/// its own packet chain in [`au_ump_packet_at`]; transport /
/// automation don't ride UMP).
fn try_encode_au_ump(event: &Event, protocol: UmpProtocol) -> Option<AuUmpEvent> {
    let (word_count, words) = match protocol {
        UmpProtocol::Midi2 => (
            2u8,
            encode_ump_channel_voice_2(&event.body).or_else(|| {
                upconvert_to_midi2(&event.body)
                    .as_ref()
                    .and_then(encode_ump_channel_voice_2)
            })?,
        ),
        UmpProtocol::Midi1 => (
            1u8,
            encode_ump_channel_voice_1(&event.body).or_else(|| {
                downconvert_to_midi1(&event.body)
                    .as_ref()
                    .and_then(encode_ump_channel_voice_1)
            })?,
        ),
    };
    Some(AuUmpEvent {
        sample_offset: event.sample_offset,
        cable: event.port,
        word_count,
        reserved: [0; 2],
        words,
    })
}

/// UMP packets `event` contributes to the output stream: channel voice
/// is one packet (or zero when it has no form in the stream's
/// protocol), `SysEx` is its whole `SysEx`-7 chain (one 64-bit packet
/// per 6 payload bytes, valid in either protocol).
fn au_ump_packet_count(list: &EventList, event: &Event, protocol: UmpProtocol) -> usize {
    if matches!(event.body, EventBody::SysEx { .. }) {
        sysex7_packet_count(list.sysex_bytes(&event.body).len())
    } else {
        usize::from(try_encode_au_ump(event, protocol).is_some())
    }
}

/// Resume point for the sequential UMP output drain. `output_ump_at`
/// addresses the flattened packet stream by index; without a cursor
/// every call rescans from the head, turning a SysEx-heavy drain
/// quadratic (one large `SysEx` is thousands of packets in a single
/// render).
struct UmpDrainCursor {
    /// Packets contributed by events before [`Self::event_pos`].
    packets_before: usize,
    /// Event position the next lookup resumes from.
    event_pos: usize,
    /// Protocol the positions were computed for.
    protocol: UmpProtocol,
}

impl UmpDrainCursor {
    /// Head of the stream; also the per-block reset value.
    const HEAD: Self = Self {
        packets_before: 0,
        event_pos: 0,
        protocol: UmpProtocol::Midi2,
    };
}

/// The `index`-th packet of the flattened UMP output stream. Resumes
/// from `cursor` when the query continues forward in the same
/// protocol (the appex drains indices in ascending order); a
/// backwards or cross-protocol query restarts at the head.
fn au_ump_packet_at(
    list: &EventList,
    index: usize,
    protocol: UmpProtocol,
    cursor: &mut UmpDrainCursor,
) -> Option<AuUmpEvent> {
    if protocol != cursor.protocol || index < cursor.packets_before {
        *cursor = UmpDrainCursor {
            protocol,
            ..UmpDrainCursor::HEAD
        };
    }
    let mut remaining = index - cursor.packets_before;
    for (pos, event) in list.iter().enumerate().skip(cursor.event_pos) {
        let n = au_ump_packet_count(list, event, protocol);
        if remaining < n {
            cursor.event_pos = pos;
            cursor.packets_before = index - remaining;
            if matches!(event.body, EventBody::SysEx { .. }) {
                // SysEx carries no group; packets go out on group 0. The
                // cable still routes by the event's port.
                let words = encode_sysex7_packet(0, list.sysex_bytes(&event.body), remaining)?;
                return Some(AuUmpEvent {
                    sample_offset: event.sample_offset,
                    cable: event.port,
                    word_count: 2,
                    reserved: [0; 2],
                    words: [words[0], words[1], 0, 0],
                });
            }
            return try_encode_au_ump(event, protocol);
        }
        remaining -= n;
    }
    None
}

unsafe extern "C" fn cb_output_ump_count<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    protocol: u32,
) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let audio = inst.audio.enter();
        let list = &audio.output_events;
        let protocol = UmpProtocol::from_wire(protocol);
        len_u32(
            list.iter()
                .map(|e| au_ump_packet_count(list, e, protocol))
                .sum(),
        )
    }
}

unsafe extern "C" fn cb_output_ump_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    protocol: u32,
    index: u32,
    out: *mut AuUmpEvent,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut audio = inst.audio.enter();
        let scr = &mut *audio;
        if let Some(mut packet) = au_ump_packet_at(
            &scr.output_events,
            index as usize,
            UmpProtocol::from_wire(protocol),
            &mut scr.ump_drain_cursor,
        ) {
            // Route the cable like the byte path (out-of-range lands
            // on 0); the appex passes it to `midiOutputEventListBlock`.
            packet.cable = route_midi_port(packet.cable, P::info().midi_output_ports);
            *out = packet;
        }
    }
}

/// `SysEx` input for AU v2. The shim's `au_v2_sysex` strips the
/// `0xF0`/`0xF7` framing and calls this once per complete message. AU v2
/// does **not** serialize `MusicDeviceSysEx` against `AudioUnitRender`,
/// so the host may call this on any thread while `cb_process` is running
/// on the audio thread. It must therefore never touch the audio-thread-
/// owned scratch: it copies the bytes into a recycled buffer and queues
/// it lock-free ([`SysExQueue`]); `cb_process` drains the queue into the
/// event pool at the block top. AU v3 takes the in-line `events2`
/// `SysEx`-7/8 path instead and never calls this.
unsafe extern "C" fn cb_au_push_sysex_input<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_offset: u32,
    bytes: *const u8,
    len: u32,
) {
    // Firewalled like the other extern boundaries: a panic here (unlike
    // before) must not unwind across the C ABI and abort the host.
    run_extern_callback_with::<P, ()>("au", "sysex_input", (), || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        if bytes.is_null() || len == 0 {
            return;
        }
        let slice = std::slice::from_raw_parts(bytes, len as usize);
        queue_sysex_input(&inst.sysex_free, &inst.sysex_ready, sample_offset, slice);
    });
}

unsafe extern "C" fn cb_au_push_sysex_input_native<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_offset: u32,
    bytes: *const u8,
    len: u32,
) -> u32 {
    run_extern_callback_with::<P, u32>("au", "sysex_input_native", 0, || unsafe {
        if ctx.is_null() || (bytes.is_null() && len != 0) {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let payload = if len == 0 {
            &[][..]
        } else {
            slice::from_raw_parts(bytes, len as usize)
        };
        u32::from(try_queue_sysex_input(
            &inst.sysex_free,
            &inst.sysex_ready,
            sample_offset,
            payload,
        ))
    })
}

unsafe extern "C" fn cb_output_sysex_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let audio = inst.audio.enter();
        len_u32(
            audio
                .output_events
                .iter()
                .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
                .count(),
        )
    }
}

unsafe extern "C" fn cb_output_sysex_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out_delta_frames: *mut u32,
    out_bytes: *mut *const u8,
    out_len: *mut u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let audio = inst.audio.enter();
        if let Some(event) = audio
            .output_events
            .iter()
            .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
            .nth(index as usize)
        {
            let bytes = audio.output_events.sysex_bytes(&event.body);
            *out_delta_frames = event.sample_offset;
            *out_bytes = bytes.as_ptr();
            *out_len = len_u32(bytes.len());
        }
    }
}

enum NativeEncodeResult {
    Emitted(AuNativeEvent),
    Unsupported,
    Invalid,
}

fn typed_midi1_bytes(body: &EventBody) -> Option<([u8; 3], u32)> {
    let (bytes, len) = match *body {
        EventBody::NoteOn {
            group: 0,
            channel,
            note,
            velocity,
        } if channel < 16 && note < 128 && velocity < 128 => ([0x90 | channel, note, velocity], 3),
        EventBody::NoteOff {
            group: 0,
            channel,
            note,
            velocity,
        } if channel < 16 && note < 128 && velocity < 128 => ([0x80 | channel, note, velocity], 3),
        EventBody::ControlChange {
            group: 0,
            channel,
            cc,
            value,
        } if channel < 16 && cc < 128 && value < 128 => ([0xB0 | channel, cc, value], 3),
        EventBody::Aftertouch {
            group: 0,
            channel,
            note,
            pressure,
        } if channel < 16 && note < 128 && pressure < 128 => ([0xA0 | channel, note, pressure], 3),
        EventBody::ChannelPressure {
            group: 0,
            channel,
            pressure,
        } if channel < 16 && pressure < 128 => ([0xD0 | channel, pressure, 0], 2),
        EventBody::ProgramChange {
            group: 0,
            channel,
            program,
        } if channel < 16 && program < 128 => ([0xC0 | channel, program, 0], 2),
        EventBody::PitchBend {
            group: 0,
            channel,
            value,
        } if channel < 16 && value <= 0x3FFF => {
            let (lsb, msb) = pitch_bend_to_bytes(value);
            ([0xE0 | channel, lsb, msb], 3)
        }
        _ => return None,
    };
    Some((bytes, len))
}

fn midi1_message_len(status: u8) -> Option<usize> {
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

fn raw_midi1_valid(message: RawMidi1) -> bool {
    let bytes = message.bytes();
    midi1_message_len(bytes[0]) == Some(bytes.len())
        && bytes.iter().skip(1).all(|byte| byte & 0x80 == 0)
}

fn output_port<P: PluginExport>(port: u16) -> Option<u16> {
    (port < u16::from(P::info().midi_output_ports)).then_some(port)
}

fn default_protocol<P: PluginExport>() -> u8 {
    if P::info().midi_output_dialect == MidiDialect::Midi2 {
        2
    } else {
        1
    }
}

fn raw_ump_protocol<P: PluginExport>(
    exact: moose_core::ExactEventRef<'_>,
) -> Result<u8, NativeEncodeResult> {
    let ExactEventBody::Ump { packet, .. } = exact.body() else {
        return Err(NativeEncodeResult::Invalid);
    };
    let message_type = ((packet.words()[0] >> 28) & 0x0f) as u8;
    if packet.word_count() != usize::from(ump_word_count_for_type(packet.words()[0])) {
        return Err(NativeEncodeResult::Invalid);
    }
    let protocol = match exact.metadata() {
        ExactEventMetadata::Au(metadata) => metadata.protocol(),
        ExactEventMetadata::None => match message_type {
            0x2 => 1,
            0x4 => 2,
            _ => default_protocol::<P>(),
        },
        _ => return Err(NativeEncodeResult::Unsupported),
    };
    if !ump_protocol_accepts(protocol, message_type) {
        return Err(NativeEncodeResult::Invalid);
    }
    Ok(protocol)
}

fn require_ump_carrier(carrier_mask: u32, host_protocol: u8) -> Result<(), NativeEncodeResult> {
    if carrier_mask & AU_NATIVE_CARRIER_UMP == 0 {
        Err(NativeEncodeResult::Unsupported)
    } else if matches!(host_protocol, 1 | 2) {
        Ok(())
    } else {
        Err(NativeEncodeResult::Invalid)
    }
}

#[allow(clippy::too_many_lines)]
fn encode_native_output<P: PluginExport>(
    event: LosslessEventRef<'_>,
    list: &EventList,
    carrier_mask: u32,
    num_frames: u32,
    host_protocol: u8,
) -> NativeEncodeResult {
    let sample_offset = match event {
        LosslessEventRef::Typed(event) => event.sample_offset,
        LosslessEventRef::Exact(exact) => exact.sample_offset(),
    };
    if sample_offset >= num_frames {
        return NativeEncodeResult::Invalid;
    }

    match event {
        LosslessEventRef::Typed(event) => {
            let Some(port) = output_port::<P>(u16::from(event.port)) else {
                return NativeEncodeResult::Invalid;
            };
            if let Some((midi, data_len)) = typed_midi1_bytes(&event.body) {
                if carrier_mask & AU_NATIVE_CARRIER_BYTES != 0 {
                    return NativeEncodeResult::Emitted(AuNativeEvent {
                        sample_offset,
                        port,
                        kind: AU_NATIVE_EVENT_MIDI1,
                        data_len,
                        midi,
                        ..AuNativeEvent::default()
                    });
                }
                if let Err(status) = require_ump_carrier(carrier_mask, host_protocol) {
                    return status;
                }
                let Some(words) = encode_ump_channel_voice_1(&event.body) else {
                    return NativeEncodeResult::Invalid;
                };
                return NativeEncodeResult::Emitted(AuNativeEvent {
                    sample_offset,
                    port,
                    kind: AU_NATIVE_EVENT_UMP,
                    protocol: 1,
                    data_len: 1,
                    words,
                    ..AuNativeEvent::default()
                });
            }
            if event_to_midi1(&event.body).is_some() {
                return NativeEncodeResult::Invalid;
            }
            if let EventBody::SysEx { .. } = event.body {
                let Some(bytes) = list.sysex_bytes_checked(&event.body) else {
                    return NativeEncodeResult::Invalid;
                };
                if bytes.iter().any(|byte| byte & 0x80 != 0) {
                    return NativeEncodeResult::Invalid;
                }
                if carrier_mask & AU_NATIVE_CARRIER_BYTES == 0 {
                    return NativeEncodeResult::Unsupported;
                }
                return NativeEncodeResult::Emitted(AuNativeEvent {
                    sample_offset,
                    port,
                    kind: AU_NATIVE_EVENT_SYSEX,
                    data_len: len_u32(bytes.len()),
                    sysex: bytes.as_ptr(),
                    ..AuNativeEvent::default()
                });
            }
            if let Some(words) = encode_ump_channel_voice_2(&event.body) {
                if decode_ump_channel_voice_2(words).as_ref() != Some(&event.body) {
                    return NativeEncodeResult::Invalid;
                }
                if let Err(status) = require_ump_carrier(carrier_mask, host_protocol) {
                    return status;
                }
                return NativeEncodeResult::Emitted(AuNativeEvent {
                    sample_offset,
                    port,
                    kind: AU_NATIVE_EVENT_UMP,
                    protocol: 2,
                    data_len: 2,
                    words,
                    ..AuNativeEvent::default()
                });
            }
            NativeEncodeResult::Unsupported
        }
        LosslessEventRef::Exact(exact) => {
            if exact.qualifiers() != ExactEventQualifiers::default() {
                return NativeEncodeResult::Unsupported;
            }
            match *exact.body() {
                ExactEventBody::Midi1 { port, message } => {
                    if !exact.metadata().is_none() {
                        return NativeEncodeResult::Unsupported;
                    }
                    let Some(port) = output_port::<P>(port) else {
                        return NativeEncodeResult::Invalid;
                    };
                    if !raw_midi1_valid(message) {
                        return NativeEncodeResult::Invalid;
                    }
                    if carrier_mask & AU_NATIVE_CARRIER_BYTES == 0 {
                        return NativeEncodeResult::Unsupported;
                    }
                    NativeEncodeResult::Emitted(AuNativeEvent {
                        sample_offset,
                        port,
                        kind: AU_NATIVE_EVENT_MIDI1,
                        data_len: u32::try_from(message.len()).unwrap_or(0),
                        midi: *message.storage(),
                        ..AuNativeEvent::default()
                    })
                }
                ExactEventBody::SysEx { port } => {
                    if !exact.metadata().is_none() {
                        return NativeEncodeResult::Unsupported;
                    }
                    let Some(port) = output_port::<P>(port) else {
                        return NativeEncodeResult::Invalid;
                    };
                    let Some(bytes) = exact.sysex_bytes_checked() else {
                        return NativeEncodeResult::Invalid;
                    };
                    if bytes.iter().any(|byte| byte & 0x80 != 0) {
                        return NativeEncodeResult::Invalid;
                    }
                    if carrier_mask & AU_NATIVE_CARRIER_BYTES == 0 {
                        return NativeEncodeResult::Unsupported;
                    }
                    NativeEncodeResult::Emitted(AuNativeEvent {
                        sample_offset,
                        port,
                        kind: AU_NATIVE_EVENT_SYSEX,
                        data_len: len_u32(bytes.len()),
                        sysex: bytes.as_ptr(),
                        ..AuNativeEvent::default()
                    })
                }
                ExactEventBody::Ump { port, packet } => {
                    let Some(port) = output_port::<P>(port) else {
                        return NativeEncodeResult::Invalid;
                    };
                    if let Err(status) = require_ump_carrier(carrier_mask, host_protocol) {
                        return status;
                    }
                    let protocol = match raw_ump_protocol::<P>(exact) {
                        Ok(protocol) => protocol,
                        Err(status) => return status,
                    };
                    NativeEncodeResult::Emitted(AuNativeEvent {
                        sample_offset,
                        port,
                        kind: AU_NATIVE_EVENT_UMP,
                        protocol,
                        data_len: u32::try_from(packet.word_count()).unwrap_or(0),
                        words: *packet.storage(),
                        ..AuNativeEvent::default()
                    })
                }
                _ => NativeEncodeResult::Unsupported,
            }
        }
    }
}

fn encode_native_output_checked<P: PluginExport>(
    event: LosslessEventRef<'_>,
    list: &EventList,
    carrier_mask: u32,
    num_frames: u32,
    host_protocol: u8,
    max_absolute_offset: u32,
    ump_time_valid: bool,
) -> NativeEncodeResult {
    let sample_offset = match event {
        LosslessEventRef::Typed(event) => event.sample_offset,
        LosslessEventRef::Exact(exact) => exact.sample_offset(),
    };
    if sample_offset > max_absolute_offset {
        return NativeEncodeResult::Invalid;
    }
    match encode_native_output::<P>(event, list, carrier_mask, num_frames, host_protocol) {
        NativeEncodeResult::Emitted(event)
            if event.kind == AU_NATIVE_EVENT_UMP && !ump_time_valid =>
        {
            NativeEncodeResult::Invalid
        }
        status => status,
    }
}

#[allow(clippy::too_many_arguments)]
fn begin_output_events<P: PluginExport>(
    inst: &AuInstance<P>,
    audio: &mut AuAudio<P>,
    carrier_mask: u32,
    num_frames: u32,
    ump_protocol: u32,
    max_absolute_offset: u32,
    ump_time_valid: u32,
    param_feedback_available: u32,
) {
    audio.native_output_cursor = LosslessEventCursor::default();
    audio.native_output_carriers = carrier_mask & (AU_NATIVE_CARRIER_BYTES | AU_NATIVE_CARRIER_UMP);
    audio.native_output_num_frames = num_frames;
    audio.native_output_ump_protocol = u8::try_from(ump_protocol).unwrap_or(0);
    audio.native_output_max_absolute_offset = max_absolute_offset;
    audio.native_output_ump_time_valid = ump_time_valid != 0;
    audio.native_output_param_feedback_available = param_feedback_available != 0;
    if audio.output_events.overflow().is_some() {
        audio.native_output_status = AU_OUTPUT_QUEUE_FULL;
        return;
    }

    #[cfg(target_os = "macos")]
    let mut param_count = 0usize;
    let status = audio.output_events.lossless_iter().find_map(|event| {
        if let LosslessEventRef::Typed(Event {
            sample_offset,
            body: EventBody::ParamChange { id, value },
            ..
        }) = event
        {
            if *sample_offset >= num_frames
                || !value.is_finite()
                || !inst.param_infos.iter().any(|info| info.id == *id)
            {
                return Some(AU_OUTPUT_INVALID);
            }
            #[cfg(target_os = "macos")]
            {
                if !f32::from_f64(*value).is_finite() {
                    return Some(AU_OUTPUT_INVALID);
                }
                if !audio.native_output_param_feedback_available || inst.param_notify.is_none() {
                    return Some(AU_OUTPUT_UNSUPPORTED);
                }
                param_count += 1;
                return None;
            }
            #[cfg(not(target_os = "macos"))]
            return Some(AU_OUTPUT_UNSUPPORTED);
        }
        match encode_native_output_checked::<P>(
            event,
            &audio.output_events,
            audio.native_output_carriers,
            num_frames,
            audio.native_output_ump_protocol,
            max_absolute_offset,
            audio.native_output_ump_time_valid,
        ) {
            NativeEncodeResult::Emitted(_) => None,
            NativeEncodeResult::Unsupported => Some(AU_OUTPUT_UNSUPPORTED),
            NativeEncodeResult::Invalid => Some(AU_OUTPUT_INVALID),
        }
    });
    audio.native_output_status = status.unwrap_or(AU_OUTPUT_END);

    #[cfg(target_os = "macos")]
    if audio.native_output_status == AU_OUTPUT_END
        && param_count > 0
        && let Some(notifier) = &inst.param_notify
        && notifier
            .queue
            .capacity()
            .saturating_sub(notifier.queue.len())
            < param_count
    {
        audio.native_output_status = AU_OUTPUT_QUEUE_FULL;
    }
}

unsafe extern "C" fn cb_begin_output_events<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    carrier_mask: u32,
    num_frames: u32,
    ump_protocol: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut audio = inst.audio.enter();
        begin_output_events(
            inst,
            &mut audio,
            carrier_mask,
            num_frames,
            ump_protocol,
            u32::MAX,
            1,
            0,
        );
    }
}

unsafe extern "C" fn cb_begin_output_events_v9<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    carrier_mask: u32,
    num_frames: u32,
    ump_protocol: u32,
    max_absolute_offset: u32,
    ump_time_valid: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut audio = inst.audio.enter();
        begin_output_events(
            inst,
            &mut audio,
            carrier_mask,
            num_frames,
            ump_protocol,
            max_absolute_offset,
            ump_time_valid,
            0,
        );
    }
}

unsafe extern "C" fn cb_begin_output_events_v10<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    carrier_mask: u32,
    num_frames: u32,
    ump_protocol: u32,
    max_absolute_offset: u32,
    ump_time_valid: u32,
    param_feedback_available: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut audio = inst.audio.enter();
        begin_output_events(
            inst,
            &mut audio,
            carrier_mask,
            num_frames,
            ump_protocol,
            max_absolute_offset,
            ump_time_valid,
            param_feedback_available,
        );
    }
}

unsafe extern "C" fn cb_next_output_event<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out: *mut AuNativeEvent,
) -> u32 {
    unsafe {
        if out.is_null() {
            return AU_OUTPUT_INVALID;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut audio = inst.audio.enter();
        let scr = &mut *audio;
        if scr.native_output_status != AU_OUTPUT_END {
            return std::mem::replace(&mut scr.native_output_status, AU_OUTPUT_END);
        }
        let event = loop {
            let Some(event) = scr
                .output_events
                .lossless_next(&mut scr.native_output_cursor)
            else {
                return AU_OUTPUT_END;
            };
            if !matches!(
                event,
                LosslessEventRef::Typed(Event {
                    body: EventBody::ParamChange { .. },
                    ..
                })
            ) {
                break event;
            }
        };
        match encode_native_output_checked::<P>(
            event,
            &scr.output_events,
            scr.native_output_carriers,
            scr.native_output_num_frames,
            scr.native_output_ump_protocol,
            scr.native_output_max_absolute_offset,
            scr.native_output_ump_time_valid,
        ) {
            NativeEncodeResult::Emitted(event) => {
                out.write(event);
                AU_OUTPUT_EMITTED
            }
            NativeEncodeResult::Unsupported => AU_OUTPUT_UNSUPPORTED,
            NativeEncodeResult::Invalid => AU_OUTPUT_INVALID,
        }
    }
}

unsafe extern "C" fn cb_commit_output_params<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let audio = inst.audio.enter();
        if audio.output_events.overflow().is_some() {
            return AU_OUTPUT_QUEUE_FULL;
        }
        if !audio.native_output_param_feedback_available
            && audio.output_events.lossless_iter().any(|event| {
                matches!(
                    event,
                    LosslessEventRef::Typed(Event {
                        body: EventBody::ParamChange { .. },
                        ..
                    })
                )
            })
        {
            return AU_OUTPUT_UNSUPPORTED;
        }

        #[cfg(target_os = "macos")]
        {
            let mut emitted = false;
            for event in audio.output_events.lossless_iter() {
                let LosslessEventRef::Typed(Event {
                    body: EventBody::ParamChange { id, value },
                    ..
                }) = event
                else {
                    continue;
                };
                let Some(notifier) = &inst.param_notify else {
                    return AU_OUTPUT_UNSUPPORTED;
                };
                if notifier.queue.push((*id, f32::from_f64(*value))).is_err() {
                    return AU_OUTPUT_QUEUE_FULL;
                }
                emitted = true;
            }
            if emitted {
                AU_OUTPUT_EMITTED
            } else {
                AU_OUTPUT_END
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            if audio.output_events.lossless_iter().any(|event| {
                matches!(
                    event,
                    LosslessEventRef::Typed(Event {
                        body: EventBody::ParamChange { .. },
                        ..
                    })
                )
            }) {
                AU_OUTPUT_UNSUPPORTED
            } else {
                AU_OUTPUT_END
            }
        }
    }
}

unsafe extern "C" fn cb_finish_output_events<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    status: u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut audio = inst.audio.enter();
        let status = audio.output_events.overflow().map_or_else(
            || match status {
                AU_OUTPUT_END | AU_OUTPUT_EMITTED => OutputEventStatus::Success,
                AU_OUTPUT_UNSUPPORTED => OutputEventStatus::Unsupported,
                AU_OUTPUT_QUEUE_FULL => OutputEventStatus::HostQueueFull,
                _ => OutputEventStatus::Invalid,
            },
            OutputEventStatus::BufferFull,
        );
        audio.output_events.set_output_status(status);
    }
}

// ---------------------------------------------------------------------------
// GUI callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_gui_has_editor<P: PluginExport>(ctx: *mut std::ffi::c_void) -> i32 {
    // The editor builder is author code; firewall its lazy construction so
    // a panic there can't unwind across the C ABI (0 = "no editor").
    run_extern_callback_with::<P, i32>("au", "gui_has_editor", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut gui = inst.gui.enter();
        if gui.editor.is_none() {
            // Built from the lock-free param store the wrapper already
            // holds outside the plugin, so opening the GUI never
            // stalls the audio thread.
            gui.editor = (inst.editor_builder)(inst.params_arc.clone());
        }
        i32::from(gui.editor.is_some())
    })
}

/// Used by the AU v3 Swift shim in `viewDidLayoutSubviews` to
/// decide whether to forward host bounds changes to the editor,
/// and by the AU v2 `uiViewForAudioUnit:withSize:` path to pick
/// between the host's `preferredSize` and the editor's natural
/// size. Returns 1 / 0 mapping to "yes / no resizable".
unsafe extern "C" fn cb_gui_can_resize<P: PluginExport>(ctx: *mut std::ffi::c_void) -> i32 {
    // `Editor::can_resize` is author code; firewall it (0 = "not resizable").
    run_extern_callback_with::<P, i32>("au", "gui_can_resize", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        i32::from(
            inst.gui
                .enter()
                .editor
                .as_ref()
                .is_some_and(|e| e.can_resize()),
        )
    })
}

/// Host-driven `set_size`. The AU v2 Cocoa view's
/// `setFrameSize:` / superview-frame observer calls this when the
/// host resizes its outer container; the AU v3 Swift shim calls
/// it from `viewDidLayoutSubviews`. Clamps to the editor's
/// `min_size` / `max_size` / `aspect_ratio` so a host dragging
/// below the editor's floor doesn't clip widgets (mirrors the
/// CLAP and VST3 wrappers).
unsafe extern "C" fn cb_gui_set_size<P: PluginExport>(ctx: *mut std::ffi::c_void, w: u32, h: u32) {
    // `Editor::set_size` / `can_resize` are author code; firewall them.
    run_extern_callback_with::<P, ()>("au", "gui_set_size", (), || unsafe {
        if ctx.is_null() || w == 0 || h == 0 {
            return;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        // `try_enter`, not `enter`: if `editor.set_size` (driven by a plugin
        // `request_resize`) ever makes the host re-drive layout synchronously
        // back into this callback on the same thread, `enter` would
        // double-enter the still-held cell. Skipping a re-entrant commit is
        // safe - the host re-asserts the size on its next layout pass.
        let Some(mut gui) = inst.gui.try_enter() else {
            return;
        };
        if let Some(editor) = gui.editor.as_mut()
            && editor.can_resize()
        {
            let (cw, ch) = fit_logical_size(w, h, editor.as_ref());
            editor.set_size(cw, ch);
        }
    });
}

unsafe extern "C" fn cb_gui_get_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    // The editor builder + `Editor::size` are author code; firewall them so
    // a panic can't unwind across the C ABI.
    run_extern_callback_with::<P, ()>("au", "gui_get_size", (), || unsafe {
        if ctx.is_null() {
            return;
        }
        // Lazily install the editor here too. Some AU validators
        // (`auval`, Logic Pro's plugin validator) call `..._get_size`
        // before `..._has_editor`, which is the canonical install
        // site. Without the lazy install here those validators see
        // `inst.editor == None` and silently receive a 0x0 view,
        // which shows up as "plugin reports invalid size" in their
        // reports.
        let inst = &*ctx.cast::<AuInstance<P>>();
        // `try_enter`: same synchronous-relayout re-entry guard as
        // `cb_gui_set_size`. If busy, report the size the plugin last
        // requested (still pending), leaving the host's value when there is
        // none - never re-enter the cell.
        let Some(mut gui) = inst.gui.try_enter() else {
            let packed = inst.pending_resize.load(Ordering::Relaxed);
            if packed != 0 {
                #[allow(clippy::cast_possible_truncation)]
                {
                    *w = (packed >> 32) as u32;
                    *h = packed as u32;
                }
            }
            return;
        };
        if gui.editor.is_none() {
            // Built from the lock-free param store the wrapper already
            // holds outside the plugin, so opening the GUI never
            // stalls the audio thread.
            gui.editor = (inst.editor_builder)(inst.params_arc.clone());
        }
        // Apply a resize the plugin requested re-entrantly through
        // `request_resize` (deferred there because the `gui` cell was held)
        // before reporting the size, so the host sees the value the plugin
        // asked for. A re-request from this `set_size` re-stashes and lands on
        // the next query.
        let packed = inst.pending_resize.swap(0, Ordering::Relaxed);
        if packed != 0
            && let Some(editor) = gui.editor.as_mut()
        {
            #[allow(clippy::cast_possible_truncation)]
            let (rw, rh) = ((packed >> 32) as u32, packed as u32);
            editor.set_size(rw, rh);
        }
        if let Some(ref editor) = gui.editor {
            // AU is macOS-only; hosts embed our NSView inside a Cocoa
            // container at logical-point coordinates and AppKit handles
            // the Retina backing transparently. Report the editor size
            // as-is - no scaling.
            let (ew, eh) = editor.size();
            *w = ew;
            *h = eh;
        }
    });
}

unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) {
    // `editor.open` runs author GUI-construction code that can panic;
    // firewall it so the panic can't unwind across the C ABI.
    run_extern_callback_with::<P, ()>("au", "gui_open", (), || {
        // AU is macOS+iOS-only at runtime. Linux/Windows builds compile
        // the wrapper crate for completeness (it's part of the workspace
        // build matrix) but the body references AppKit / UIKit /
        // AUEventListener APIs that don't exist off-Apple. Stubbing the
        // body keeps the FFI table population in `register_au_inner`
        // type-checking on every platform.
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        {
            let _ = ctx;
            let _ = parent;
            let _ = std::marker::PhantomData::<P>;
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        unsafe {
            let inst = &*ctx.cast::<AuInstance<P>>();
            let mut gui = inst.gui.enter();
            if let Some(ref mut editor) = gui.editor {
                let params = Arc::clone(&inst.params_arc);
                let meter_store = Arc::clone(&inst.meter_store);
                let snapshot = Arc::clone(&inst.snapshot);
                let ctx_raw = SendPtr::new(ctx);
                let params_for_set = params.clone();
                let params_for_get = params.clone();
                let params_for_plain = params.clone();
                let params_for_fmt = params.clone();
                let params_for_ctx = params.clone();
                let task_spawner_for_ctx = inst.task_spawner.clone();
                let pending_state_for_set = inst.pending_state.clone();
                let transport_slot = inst.transport_slot.clone();
                let ctx_for_begin = ctx_raw;
                let ctx_for_end = ctx_raw;
                // iOS AU v3 hosts the editor inside an .appex; v2's
                // `AUEventListener` doesn't exist there. Parameter
                // changes from the editor flow to the host directly
                // through the AUParameterTree's setter (handled by the
                // Swift shim). The begin/set/end closures are no-ops on
                // iOS so the plugin's editor code stays platform-agnostic.
                let context = PluginContext::from_closures(
                    ClosureBridge {
                        #[cfg(target_os = "macos")]
                        begin_edit: Box::new(move |id| {
                            // Broadcasts kAudioUnitEvent_BeginParameterChangeGesture
                            // via AUEventListenerNotify so hosts (Logic, Live,
                            // Reaper) group subsequent set_param calls into one
                            // undo step and one automation gesture.
                            moose_au_v2_host_begin_param_gesture(
                                ctx_for_begin.as_ptr().cast_mut(),
                                id,
                            );
                        }),
                        #[cfg(target_os = "ios")]
                        begin_edit: Box::new(move |_id| {
                            let _ = ctx_for_begin;
                        }),
                        #[cfg(target_os = "macos")]
                        set_param: Box::new(move |id, value| {
                            // One combined trait dispatch (set_normalized
                            // + get_plain) instead of two - the
                            // `#[derive(Params)]` impl can compute both in
                            // a single match-arm walk.
                            let plain = f32::from_f64(
                                params_for_set.set_normalized_returning_plain(id, value),
                            );
                            moose_au_v2_host_set_param(ctx_raw.as_ptr().cast_mut(), id, plain);
                        }),
                        #[cfg(target_os = "ios")]
                        set_param: Box::new(move |id, value| {
                            // No host-notify on iOS; just write the
                            // normalised value through. The Swift shim
                            // polls the parameter tree.
                            let _ = ctx_raw;
                            let _ = params_for_set.set_normalized_returning_plain(id, value);
                        }),
                        #[cfg(target_os = "macos")]
                        end_edit: Box::new(move |id| {
                            // Closes the gesture started by begin_edit so the
                            // host commits the undo group / stops automation
                            // recording.
                            moose_au_v2_host_end_param_gesture(ctx_for_end.as_ptr().cast_mut(), id);
                        }),
                        #[cfg(target_os = "ios")]
                        end_edit: Box::new(move |_id| {
                            let _ = ctx_for_end;
                        }),
                        request_resize: Box::new(move |w, h| {
                            // AU v2 has no host-driven resize API: the
                            // host observes the plug-in's NSView frame
                            // via AppKit and updates its container in
                            // response. So `ctx.request_resize` here
                            // routes back into the editor's own
                            // `set_size`, which resizes the baseview
                            // NSView; AppKit propagates the frame
                            // change to the host as a notification.
                            //
                            // SAFETY: `ctx_raw` points at the live
                            // `AuInstance<P>`. The closure runs on the
                            // GUI thread, same as `cb_gui_open` which
                            // installed it.
                            if w == 0 || h == 0 {
                                return false;
                            }
                            let inst = &*ctx_raw.as_ptr().cast::<AuInstance<P>>();
                            // An author can call this from inside an `Editor`
                            // method (`set_size`, `state_changed`) that a GUI
                            // callback invoked while holding the `gui` cell -
                            // re-entering `enter()` there is same-thread
                            // aliasing UB. `try_enter` applies the resize when
                            // the cell is free (the common editor-loop path)
                            // and otherwise stashes it for `cb_gui_get_size` to
                            // apply on the next size query.
                            if let Some(mut gui) = inst.gui.try_enter() {
                                gui.editor.as_mut().is_some_and(|e| e.set_size(w, h))
                            } else {
                                inst.pending_resize
                                    .store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
                                true
                            }
                        }),
                        get_param: Box::new(move |id| {
                            params_for_get.get_normalized(id).unwrap_or(0.0)
                        }),
                        get_param_plain: Box::new(move |id| {
                            params_for_plain.get_plain(id).unwrap_or(0.0)
                        }),
                        format_param: Box::new(move |id| {
                            let plain = params_for_fmt.get_plain(id).unwrap_or(0.0);
                            params_for_fmt
                                .format_value(id, plain)
                                .unwrap_or_else(|| format!("{plain:.1}"))
                        }),
                        get_meter: Box::new(move |id| meter_store.read(id)),
                        get_state: Box::new(move || {
                            // Editor state read: lock-free, reads the snapshot
                            // the audio thread publishes each block. Never
                            // touches the plugin, so an editor read can't
                            // stall audio.
                            save_extra(&snapshot)
                        }),
                        set_state: Box::new(move |bytes| {
                            // The editor sends RAW custom-state bytes -
                            // exactly what `save_state()` emits and
                            // `get_state` above returns - NOT a full
                            // `serialize_state` envelope. Route them to the
                            // plugin's `load_state` on the audio thread via
                            // the same handoff queue the host load path uses
                            // (the queue is what avoids aliasing
                            // `process()`'s `&mut plugin`). No params ride
                            // along: the editor mutates params through
                            // `set_param`.
                            let _ = pending_state_for_set.force_push(state::DeserializedState {
                                params: Vec::new(),
                                extra: Some(bytes),
                                persist: Vec::new(),
                            });
                        }),
                        transport: Box::new(move || transport_slot.read()),
                    },
                    params_for_ctx,
                )
                .with_tasks(task_spawner_for_ctx);
                #[cfg(target_os = "macos")]
                let handle = RawWindowHandle::AppKit(parent);
                #[cfg(target_os = "ios")]
                let handle = RawWindowHandle::UiKit(parent);
                // Scale policy (same as moose-clap / moose-vst3): the host
                // scale wins when one is given, else the OS backing scale,
                // never both multiplied. AU has no host content-scale API,
                // so nothing is pinned via `set_scale_factor`: sizes stay in
                // logical AppKit points and the editor follows the parent
                // view's backing scale.
                editor.open(handle, context);
            }
        }
    });
}

unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut gui = inst.gui.enter();
        if let Some(editor) = gui.editor.as_mut() {
            // Same boundary-protection as `cb_destroy`: any panic
            // during `editor.close()` (wgpu surface drop, baseview
            // window close, NSView removal) would otherwise cross
            // the FFI line and become an unhandled ObjC exception
            // in the host.
            let editor_ptr: *mut dyn Editor = editor.as_mut();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (*editor_ptr).close();
            }));
        }
        // Keep the editor alive - just closed, not dropped.
        //
        // Dropping the editor here would synchronously deallocate its
        // baseview NSWindow + content NSView. Logic / Pro Tools tend
        // to call `gui_close` from inside their own NSTimer fire or
        // a `[CALayer display]` callback, both of which run inside an
        // implicit autorelease pool that's about to pop. If
        // `[NSTimer invalidate]` (which baseview's drop chain calls
        // via `WindowHandle::drop`) re-enters that pool's pop
        // sequence, the host crashes inside `objc_release` on a
        // freed `NSAutoreleasePool*`. The editor's `close()` has
        // already released the NSView contents and Metal resources;
        // the lightweight Rust struct that survives is reopened
        // in-place by the next `gui_open` call.
    }
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn free(ptr: *mut std::ffi::c_void);
}

// AU v2 host-side automation notifiers: gated to macOS because v2 is
// macOS-only. iOS uses AU v3 exclusively, where host notification
// goes through the parameter tree directly.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn moose_au_v2_host_set_param(ctx: *mut std::ffi::c_void, param_id: u32, value: f32);
    fn moose_au_v2_host_begin_param_gesture(ctx: *mut std::ffi::c_void, param_id: u32);
    fn moose_au_v2_host_end_param_gesture(ctx: *mut std::ffi::c_void, param_id: u32);
    fn moose_au_v2_host_latency_changed(ctx: *mut std::ffi::c_void);
}

// ---------------------------------------------------------------------------
// Registration: called once from the export_au! macro
// ---------------------------------------------------------------------------

/// Register the plugin with the AU system. Must be called once at load time
/// (typically from a constructor function generated by `export_au!`).
/// Host-facing AU display name. Reads `moose.toml`'s `au_name`
/// (baked into `PluginInfo` by `moose::plugin_info!`), falling back
/// to `PluginInfo::name`. The v3 host gets its display name out of
/// the appex's `Info.plist` (`AUNAME`, populated by
/// `cargo moose install --au3` from `au3_name`), not from this
/// function - `g_descriptor->name` only feeds the v2 bridge's
/// internal scanning responses, so the same value works for both
/// build flavours.
fn resolved_plugin_name(info: &PluginInfo) -> &'static str {
    resolve_name_override(info.au_name, info.name)
}

pub fn register_au<P: PluginExport>() {
    // Called from the export macro's `extern "C" fn init()` static
    // initializer. Catch any panic so it doesn't cross the FFI
    // boundary and abort the host process.
    run_register::<P>("AU", || {
        let Some((num_inputs, num_outputs)) = default_io_channels::<P>() else {
            log_missing_bus_layout::<P>("AU");
            return;
        };
        let layouts = P::bus_layouts();
        if !bus_layouts_fit_routing(&layouts) {
            eprintln!(
                "[moose AU] {} declares an audio-bus topology beyond BusRouting's limit of 32 \
                 buses per direction and 65,535 channels per bus - plugin will not register.",
                std::any::type_name::<P>(),
            );
            return;
        }
        if au_max_aux_input_buses(&layouts) > 1 {
            eprintln!(
                "[moose AU] {}: declares more than one auxiliary input bus; AU exposes only the \
                 first auxiliary and preserves later declared bus indices as unavailable",
                std::any::type_name::<P>(),
            );
        }
        if layouts.iter().any(|layout| !au_layout_supported(layout)) {
            eprintln!(
                "[moose AU] {} declares a layout beyond AU's exact process topology: at most one \
                 main output bus, 32 structurally declared channels per direction, and 32 \
                 channels of v2 main/output plus first-sidechain staging - plugin will not \
                 register.",
                std::any::type_name::<P>(),
            );
            return;
        }
        register_au_inner::<P>(num_inputs, num_outputs);
    });
}

/// MIDI status high-nibble for an `AudioUnitParameterMIDIMapping`'s
/// `mStatus`. The AU host ORs in the channel (or ignores it under the
/// any-channel flag).
fn midi_status_byte(source: MidiSource) -> u8 {
    match source {
        MidiSource::Cc(_) => 0xB0,
        MidiSource::PitchBend => 0xE0,
        MidiSource::ChannelPressure => 0xD0,
        MidiSource::ProgramChange => 0xC0,
    }
}

/// Per-layout `(main-in, main-out)` channel widths AU may negotiate, plus
/// the single sidechain width and how many layouts were dropped.
///
/// AU wires ONE sidechain width: element 1's stream format is fixed at
/// descriptor time and isn't renegotiated when the host picks a different
/// main layout. Only layouts with the default layout's bus structure,
/// enabled auxiliary set, and auxiliary widths are advertised. The main
/// input/output widths may vary. AU routes the first sidechain; later bus
/// indices stay visible to the plugin as zero-width unavailable routes.
fn au_negotiable_layouts(layouts: &[BusLayout]) -> (Vec<i16>, Vec<i16>, u32, usize) {
    fn main_in(l: &BusLayout) -> u32 {
        l.inputs.first().map_or(0, |b| b.channels.channel_count())
    }
    fn sidechain(l: &BusLayout) -> u32 {
        l.inputs
            .get(1)
            .map_or(0, |bus| bus.channels.channel_count())
    }
    // AU channel counts are small (mono..7.1.4); saturating cast is safe.
    let ch = |c: u32| i16::try_from(c).unwrap_or(0);
    let sc0 = layouts.first().map_or(0, sidechain);
    let kept: Vec<&BusLayout> = layouts
        .iter()
        .filter(|layout| {
            layouts
                .first()
                .is_none_or(|default| au_layout_matches_topology(default, layout))
        })
        .collect();
    let dropped = layouts.len() - kept.len();
    let ins = kept.iter().map(|l| ch(main_in(l))).collect();
    let outs = kept.iter().map(|l| ch(l.total_output_channels())).collect();
    (ins, outs, sc0, dropped)
}

fn au_layout_matches_topology(default: &BusLayout, layout: &BusLayout) -> bool {
    layout.inputs.len() == default.inputs.len()
        && layout.outputs.len() == default.outputs.len()
        && layout
            .inputs
            .iter()
            .zip(&default.inputs)
            .all(|(bus, default_bus)| {
                bus.kind == default_bus.kind
                    && bus.enabled == default_bus.enabled
                    && (bus.kind == BusKind::Main
                        || bus.channels.channel_count() == default_bus.channels.channel_count())
            })
        && layout
            .outputs
            .iter()
            .zip(&default.outputs)
            .all(|(bus, default_bus)| {
                bus.kind == default_bus.kind && bus.enabled == default_bus.enabled
            })
}

fn au_process_capacity(layouts: &[BusLayout]) -> (u32, u32) {
    let Some(default) = layouts.first() else {
        return (0, 0);
    };
    let sidechain = default
        .inputs
        .get(1)
        .map_or(0, |bus| bus.channels.channel_count());
    let (main_input, output) = layouts
        .iter()
        .filter(|layout| au_layout_matches_topology(default, layout))
        .fold((0_u32, 0_u32), |(max_in, max_out), layout| {
            (
                max_in.max(
                    layout
                        .inputs
                        .first()
                        .map_or(0, |bus| bus.channels.channel_count()),
                ),
                max_out.max(
                    layout
                        .outputs
                        .first()
                        .map_or(0, |bus| bus.channels.channel_count()),
                ),
            )
        });
    (main_input.saturating_add(sidechain), output)
}

const AU_MAX_FLAT_CHANNELS: u32 = 32;

fn au_layout_supported(layout: &BusLayout) -> bool {
    let structural_width = |buses: &[BusConfig]| {
        buses.iter().fold(0_u32, |channels, bus| {
            channels.saturating_add(bus.channels.channel_count())
        })
    };
    let main_input = layout
        .inputs
        .first()
        .map_or(0, |bus| bus.channels.channel_count());
    let first_sidechain = layout
        .inputs
        .get(1)
        .map_or(0, |bus| bus.channels.channel_count());
    let output = layout
        .outputs
        .first()
        .map_or(0, |bus| bus.channels.channel_count());

    layout.outputs.len() <= 1
        && layout
            .inputs
            .first()
            .is_none_or(|bus| bus.kind == BusKind::Main && bus.enabled)
        && layout
            .inputs
            .iter()
            .skip(1)
            .all(|bus| bus.kind == BusKind::Sidechain)
        && layout
            .outputs
            .first()
            .is_none_or(|bus| bus.kind == BusKind::Main && bus.enabled)
        && structural_width(&layout.inputs) <= AU_MAX_FLAT_CHANNELS
        && structural_width(&layout.outputs) <= AU_MAX_FLAT_CHANNELS
        && main_input.max(output).saturating_add(first_sidechain) <= AU_MAX_FLAT_CHANNELS
}

/// Largest number of auxiliary (non-main) input buses across all declared
/// layouts. AU exposes only the first sidechain input element; additional
/// declared indices remain present in `BusRouting` as unavailable buses.
fn au_max_aux_input_buses(layouts: &[BusLayout]) -> usize {
    layouts
        .iter()
        .map(|l| l.inputs.len().saturating_sub(1))
        .max()
        .unwrap_or(0)
}

/// Pack a `"major.minor.patch"` version string into the AU component
/// version `u32`: `(major << 16) | (minor << 8) | patch`. Hosts (Logic,
/// `GarageBand`) key their AU validation cache and component registry on
/// this value, so a shipped update must change it or the host keeps stale
/// cached validation and metadata. Falls back to `0x0001_0000` (1.0.0)
/// when a component isn't a plain number (empty string, pre-release
/// suffix on that component).
fn au_version_u32(version: &str) -> u32 {
    let mut parts = version.split('.').map(|p| p.trim().parse::<u32>().ok());
    let major = parts.next().flatten().unwrap_or(1);
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    ((major & 0xFFFF) << 16) | ((minor & 0xFF) << 8) | (patch & 0xFF)
}

fn register_au_inner<P: PluginExport>(num_inputs: u32, num_outputs: u32) {
    let info = P::info();

    // Static metadata path: derive emits a `LazyLock`-cached
    // `Vec<ParamInfo>` so registration doesn't construct a plugin
    // instance just to read parameter shape. Hand-written
    // `PluginExport` impls without a `Params::param_infos_static`
    // override fall back to the historical
    // `Self::create().params().param_infos()` walk inside the trait
    // default - see `PluginExport::param_infos_static`.
    let param_infos = P::param_infos_static();
    let mut param_descs: Vec<AuParamDescriptor> = Vec::with_capacity(param_infos.len());

    for pi in &param_infos {
        let cs = ParamCStrings::from_info(pi);
        param_descs.push(AuParamDescriptor {
            id: pi.id,
            name: cs.name.into_raw(),
            min: pi.range.min(),
            max: pi.range.max(),
            default_value: pi.default_plain,
            step_count: pi.range.step_count().map_or(0, std::num::NonZero::get),
            unit: cs.unit.into_raw(),
            group: cs.group.into_raw(),
            midi_status: pi.midi_map.map_or(0, midi_status_byte),
            midi_data1: match pi.midi_map {
                Some(MidiSource::Cc(n)) => n,
                _ => 0,
            },
            midi_channel: pi.midi_channel.map_or(-1, i16::from),
        });
    }

    let name = CString::new(resolved_plugin_name(&info)).unwrap_or_default();
    let vendor = CString::new(info.vendor).unwrap_or_default();

    let bypass_param_id = param_infos
        .iter()
        .find(|pi| pi.flags.contains(ParamFlags::IS_BYPASS))
        .map_or(u32::MAX, |pi| pi.id);

    // MIDI output is decided once on `PluginInfo` (note-effect default,
    // overridable via `midi_output` in moose.toml) so an instrument or
    // effect can opt into a host "MIDI Out" port instead of only note
    // effects advertising one.
    let has_midi_output = i32::from(info.emits_midi);
    // AU v3 exposes both declared directions as cable-indexed native lanes.
    // AU v2 remains one cable by format design while preserving native UMP.

    // Supported (in, out) channel configs from `bus_layouts()`, exposed to
    // the host through AU v2 `SupportedNumChannels` / AU v3
    // `channelCapabilities`. Only when there's more than one layout; a
    // single-layout plugin keeps `num_layouts == 0`, which the shims read
    // as "the one `(num_inputs, num_outputs)` config" (also preserving the
    // audio-less `(2, 2)` synthesis in `default_io_channels`). The leaked
    // arrays live for the process, like the descriptor itself.
    let layouts = P::bus_layouts();
    // The channel capability describes the *main* bus; a sidechain is a
    // separate input element, not summed into the main width. The main
    // input bus is the first input bus of a layout.
    let main_in_of = |l: &BusLayout| l.inputs.first().map_or(0, |b| b.channels.channel_count());
    // Only layouts sharing the first layout's sidechain width are
    // negotiable on AU (element 1's width is fixed for the instance).
    let (ins, outs, sidechain_in, dropped) = au_negotiable_layouts(&layouts);
    if dropped > 0 {
        eprintln!(
            "[moose AU] {}: {dropped} layout(s) dropped - their sidechain width differs from the \
             default layout's ({sidechain_in}); AU wires a single fixed sidechain width",
            std::any::type_name::<P>(),
        );
    }
    let (layout_in_channels, layout_out_channels, num_layouts) = if ins.len() > 1 {
        let n = len_u32(ins.len());
        (
            Box::leak(ins.into_boxed_slice()).as_ptr(),
            Box::leak(outs.into_boxed_slice()).as_ptr(),
            n,
        )
    } else {
        (std::ptr::null(), std::ptr::null(), 0)
    };

    // Main input bus width (element 0); `num_inputs` is the main width of
    // the default layout. A plain effect (one input bus) leaves
    // `sidechain_in` at 0.
    let num_inputs = layouts.first().map_or(num_inputs, main_in_of);

    let descriptor = Box::leak(Box::new(AuPluginDescriptor {
        component_type: info.au_type,
        component_subtype: info.fourcc,
        component_manufacturer: info.au_manufacturer,
        name: name.into_raw(),
        vendor: vendor.into_raw(),
        version: au_version_u32(info.version),
        num_inputs,
        num_outputs,
        bypass_param_id,
        has_midi_output,
        accepts_midi_in: i32::from(info.accepts_midi_in),
        midi_input_ports: u32::from(info.midi_input_ports),
        midi_output_ports: u32::from(info.midi_output_ports),
        midi2_input: i32::from(info.midi_input_dialect == MidiDialect::Midi2),
        midi2_output: i32::from(info.midi_output_dialect == MidiDialect::Midi2),
        layout_in_channels,
        layout_out_channels,
        num_layouts,
        sidechain_in_channels: sidechain_in,
    }));

    let callbacks = Box::leak(Box::new(AuCallbacks {
        abi_version: ffi::MOOSE_AU_ABI_VERSION,
        create: cb_create::<P>,
        destroy: cb_destroy::<P>,
        reset: cb_reset::<P>,
        process: cb_process::<P>,
        param_count: cb_param_count::<P>,
        param_get_value: cb_param_get_value::<P>,
        param_set_value: cb_param_set_value::<P>,
        param_format_value: cb_param_format_value::<P>,
        state_save: cb_state_save::<P>,
        state_load: cb_state_load::<P>,
        state_free: cb_state_free,
        output_event_count: cb_output_event_count::<P>,
        output_event_at: cb_output_event_at::<P>,
        output_sysex_count: cb_output_sysex_count::<P>,
        output_sysex_at: cb_output_sysex_at::<P>,
        output_ump_count: cb_output_ump_count::<P>,
        output_ump_at: cb_output_ump_at::<P>,
        gui_has_editor: cb_gui_has_editor::<P>,
        gui_get_size: cb_gui_get_size::<P>,
        gui_open: cb_gui_open::<P>,
        gui_close: cb_gui_close::<P>,
        gui_can_resize: cb_gui_can_resize::<P>,
        gui_set_size: cb_gui_set_size::<P>,
        factory_preset_count: cb_factory_preset_count::<P>,
        factory_preset_name: cb_factory_preset_name::<P>,
        factory_preset_load: cb_factory_preset_load::<P>,
        push_sysex_input: cb_au_push_sysex_input::<P>,
        legacy_state_key_count: cb_legacy_state_key_count::<P>,
        legacy_state_key_at: cb_legacy_state_key_at::<P>,
        state_load_foreign: cb_state_load_foreign::<P>,
        latency_samples: cb_latency_samples::<P>,
        tail_samples: cb_tail_samples::<P>,
        set_render_mode: cb_set_render_mode::<P>,
        param_parse_value: cb_param_parse_value::<P>,
        process_native: cb_process_native::<P>,
        begin_output_events: cb_begin_output_events::<P>,
        next_output_event: cb_next_output_event::<P>,
        push_sysex_input_native: cb_au_push_sysex_input_native::<P>,
        finish_output_events: cb_finish_output_events::<P>,
        begin_output_events_v9: cb_begin_output_events_v9::<P>,
        commit_output_params: cb_commit_output_params::<P>,
        begin_output_events_v10: cb_begin_output_events_v10::<P>,
        process_native_v11: cb_process_native_v11::<P>,
    }));

    let param_descs = param_descs.leak();

    unsafe {
        ffi::moose_au_register_v2(
            std::ptr::from_ref::<AuPluginDescriptor>(descriptor),
            std::ptr::from_ref::<AuCallbacks>(callbacks),
            param_descs.as_ptr(),
            len_u32(param_descs.len()),
        );
    }
}

// ---------------------------------------------------------------------------
// export_au! macro
// ---------------------------------------------------------------------------

/// Export an Audio Unit v3 plugin entry point.
///
/// Usage:
/// ```ignore
/// export_au!(MyPlugin);
/// ```
///
/// Where `MyPlugin` implements `PluginExport`.
#[macro_export]
macro_rules! export_au {
    ($plugin_type:ty) => {
        // macOS: register both AU v2 (`.component`) and AU v3 (`.appex`)
        // entry points. AU v2's factory delegates to the C shim.
        #[cfg(target_os = "macos")]
        mod _au_entry {
            use super::*;

            /// Called by the constructor to init the plugin.
            #[unsafe(no_mangle)]
            pub extern "C" fn moose_au_init() {
                ::moose_au::register_au::<$plugin_type>();
            }

            // AU v2 factory: delegates to au_v2_shim.c. The whole
            // `_au_entry` module is gated on `target_os = "macos"`
            // because v2 only exists on macOS, matching `build.rs`'s
            // `is_macos` gate on compiling au_v2_shim.c.
            unsafe extern "C" {
                fn moose_au_v2_factory_bridge(
                    desc: *const ::std::ffi::c_void,
                ) -> *mut ::std::ffi::c_void;
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn MooseAUFactory(
                desc: *const ::std::ffi::c_void,
            ) -> *mut ::std::ffi::c_void {
                moose_au_v2_factory_bridge(desc)
            }
        }
        // iOS: AU v3 only. The Swift `AudioUnitFactory` /
        // `MooseAUAudioUnit` in the .appex bundle reads our exported
        // globals (g_callbacks / g_descriptor / ...) at runtime via
        // the dynamic symbol table; we just need `moose_au_init` to
        // run from the dylib constructor.
        #[cfg(target_os = "ios")]
        mod _au_entry {
            use super::*;

            #[unsafe(no_mangle)]
            pub extern "C" fn moose_au_init() {
                ::moose_au::register_au::<$plugin_type>();
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use moose_core::SYSEX_POOL_PREALLOC;
    use moose_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList};
    use moose_shim_types::AU_SHIM_TYPES_H;

    use super::{
        SYSEX_INPUT_SLOTS, SysExInput, SysExQueue, UmpDrainCursor, UmpProtocol,
        au_max_aux_input_buses, au_negotiable_layouts, au_ump_packet_at, au_ump_packet_count,
        au_version_u32, drain_sysex_input, queue_sysex_input,
    };
    use moose_core::bus::{BusLayout, ChannelConfig};

    /// AU exposes one sidechain element, so registration counts aux buses
    /// to reject a plugin declaring more than one (rather than merging).
    #[test]
    fn au_counts_auxiliary_input_buses() {
        // No aux bus.
        assert_eq!(au_max_aux_input_buses(&[BusLayout::stereo()]), 0);
        // One aux bus - supported.
        assert_eq!(
            au_max_aux_input_buses(&[
                BusLayout::stereo().with_sidechain_input("SC", ChannelConfig::Stereo)
            ]),
            1
        );
        // Two aux buses - rejected at registration.
        assert_eq!(
            au_max_aux_input_buses(&[BusLayout::stereo()
                .with_sidechain_input("A", ChannelConfig::Stereo)
                .with_sidechain_input("B", ChannelConfig::Mono)]),
            2
        );
        // The max is taken across layouts.
        assert_eq!(
            au_max_aux_input_buses(&[
                BusLayout::mono(),
                BusLayout::stereo()
                    .with_sidechain_input("A", ChannelConfig::Stereo)
                    .with_sidechain_input("B", ChannelConfig::Mono),
            ]),
            2
        );
    }

    /// AU wires one fixed sidechain width, so only layouts sharing the
    /// first layout's sidechain width may be negotiated. A layout whose
    /// sidechain differs is dropped rather than advertised (which would let
    /// the host pick its main width but keep the wrong sidechain width).
    #[test]
    fn au_drops_layouts_with_a_differing_sidechain_width() {
        let layouts = [
            BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo),
            BusLayout::mono().with_sidechain_input("Sidechain", ChannelConfig::Mono),
        ];
        let (ins, outs, sidechain, dropped) = au_negotiable_layouts(&layouts);
        // Only the first layout survives (stereo main, stereo sidechain);
        // the mono/mono-sidechain layout is dropped.
        assert_eq!(sidechain, 2);
        assert_eq!(dropped, 1);
        assert_eq!(ins, vec![2]);
        assert_eq!(outs, vec![2]);
    }

    /// Layouts that share the sidechain width (a mono sidechain across a
    /// mono and a stereo main) are all kept.
    #[test]
    fn au_keeps_layouts_with_a_consistent_sidechain_width() {
        let layouts = [
            BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Mono),
            BusLayout::mono().with_sidechain_input("Sidechain", ChannelConfig::Mono),
        ];
        let (ins, outs, sidechain, dropped) = au_negotiable_layouts(&layouts);
        assert_eq!(sidechain, 1);
        assert_eq!(dropped, 0);
        assert_eq!(ins, vec![2, 1]);
        assert_eq!(outs, vec![2, 1]);
    }

    /// No sidechain: every layout is kept, sidechain width 0.
    #[test]
    fn au_no_sidechain_keeps_all_layouts() {
        let layouts = [BusLayout::stereo(), BusLayout::mono()];
        let (ins, _outs, sidechain, dropped) = au_negotiable_layouts(&layouts);
        assert_eq!(sidechain, 0);
        assert_eq!(dropped, 0);
        assert_eq!(ins, vec![2, 1]);
    }

    #[test]
    fn ump_output_flattens_sysex_into_packet_chain() {
        // A note, a 7-byte SysEx (Start + End), and a trailing CC:
        // 1 + 2 + 1 packets, indexed in event order.
        let mut list = EventList::with_capacity(8);
        list.push(Event::on_port(
            10,
            1,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 100,
            },
        ));
        list.push_sysex_on_port(20, 1, &[1, 2, 3, 4, 5, 6, 7])
            .unwrap();
        list.push(Event::new(
            30,
            EventBody::ControlChange {
                group: 0,
                channel: 0,
                cc: 1,
                value: 64,
            },
        ));

        let proto = UmpProtocol::Midi1;
        let total: usize = list
            .iter()
            .map(|e| au_ump_packet_count(&list, e, proto))
            .sum();
        assert_eq!(total, 4);

        // One cursor across the drain, like the appex's ascending walk.
        let mut cur = UmpDrainCursor::HEAD;

        // Packet 0: the note (MT 0x2, one word, in a 1.0 stream).
        let note = au_ump_packet_at(&list, 0, proto, &mut cur).unwrap();
        assert_eq!(note.word_count, 1);
        assert_eq!(note.sample_offset, 10);
        assert_eq!(note.cable, 1);

        // Packets 1-2: the SysEx chain - MT 0x3, Start then End, both
        // stamped with the event's offset and port.
        let start = au_ump_packet_at(&list, 1, proto, &mut cur).unwrap();
        let end = au_ump_packet_at(&list, 2, proto, &mut cur).unwrap();
        for p in [&start, &end] {
            assert_eq!((p.words[0] >> 28) & 0xF, 0x3);
            assert_eq!(p.word_count, 2);
            assert_eq!(p.sample_offset, 20);
            assert_eq!(p.cable, 1);
        }
        assert_eq!((start.words[0] >> 20) & 0xF, 0x1, "Start status");
        assert_eq!((end.words[0] >> 20) & 0xF, 0x3, "End status");

        // Packet 3: the CC; then the stream ends.
        let cc = au_ump_packet_at(&list, 3, proto, &mut cur).unwrap();
        assert_eq!(cc.sample_offset, 30);
        assert!(au_ump_packet_at(&list, 4, proto, &mut cur).is_none());

        // A backwards query restarts at the head and still resolves;
        // a stale forward cursor must never skip real packets.
        let back = au_ump_packet_at(&list, 1, proto, &mut cur).unwrap();
        assert_eq!(back.sample_offset, 20);
        assert_eq!((back.words[0] >> 20) & 0xF, 0x1, "Start status");
    }

    #[test]
    fn ump_output_streams_are_protocol_pure() {
        // One 1.0 event and one 2.0 event in the same output list. The
        // UMP spec forbids mixing MT 0x2 and MT 0x4 channel voice in a
        // protocol stream, so each protocol converts the foreign
        // dialect instead of passing it through.
        let mut list = EventList::with_capacity(4);
        list.push(Event::new(
            0,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 100,
            },
        ));
        list.push(Event::new(
            1,
            EventBody::NoteOn2 {
                group: 0,
                channel: 1,
                note: 61,
                velocity: 0x8000,
                attribute_type: 0,
                attribute: 0,
            },
        ));

        // One cursor across both protocols: the switch must restart
        // the walk, not resume 1.0 positions into the 2.0 stream.
        let mut cur = UmpDrainCursor::HEAD;
        for (proto, mt, words) in [
            (UmpProtocol::Midi1, 0x2u32, 1u8),
            (UmpProtocol::Midi2, 0x4, 2),
        ] {
            for i in 0..2 {
                let p = au_ump_packet_at(&list, i, proto, &mut cur).unwrap();
                assert_eq!((p.words[0] >> 28) & 0xF, mt);
                assert_eq!(p.word_count, words);
            }
            assert!(au_ump_packet_at(&list, 2, proto, &mut cur).is_none());
        }

        // A 2.0-only body with no 1.0 form drops from a 1.0 stream but
        // rides the 2.0 stream.
        let mut pnm = EventList::with_capacity(2);
        pnm.push(Event::new(
            0,
            EventBody::PerNoteManagement {
                group: 0,
                channel: 0,
                note: 60,
                flags: 0,
            },
        ));
        let mut cur = UmpDrainCursor::HEAD;
        assert!(au_ump_packet_at(&pnm, 0, UmpProtocol::Midi1, &mut cur).is_none());
        assert!(au_ump_packet_at(&pnm, 0, UmpProtocol::Midi2, &mut cur).is_some());
    }

    #[test]
    fn sysex_pool_prealloc_matches_header() {
        // The Swift AU v3 template (`AudioUnitFactory.swift`)
        // reads `MOOSE_SYSEX_POOL_PREALLOC` from `au_shim_types.h`
        // to size its per-render `sysexOutScratch`. Confirm the C
        // macro still expands to the same value as the Rust const
        // - otherwise the scratch is either undersized (event
        // drops) or wasteful (memory bloat per AU instance).
        let needle = format!("#define MOOSE_SYSEX_POOL_PREALLOC ({SYSEX_POOL_PREALLOC})");
        let needle_paren = format!(
            "#define MOOSE_SYSEX_POOL_PREALLOC ({} * 1024)",
            SYSEX_POOL_PREALLOC / 1024,
        );
        assert!(
            AU_SHIM_TYPES_H.contains(&needle) || AU_SHIM_TYPES_H.contains(&needle_paren),
            "au_shim_types.h::MOOSE_SYSEX_POOL_PREALLOC must equal \
             moose_core::SYSEX_POOL_PREALLOC ({} bytes / {} KiB). \
             Looked for `{}` or `{}` in the header.",
            SYSEX_POOL_PREALLOC,
            SYSEX_POOL_PREALLOC / 1024,
            needle,
            needle_paren,
        );
    }

    #[test]
    fn abi_version_matches_header() {
        // The Rust `MOOSE_AU_ABI_VERSION` (stamped into
        // `AuCallbacks::abi_version` at registration) and the header
        // `#define` (compiled into the C shims + the Swift appex) must
        // agree, or the version handshake reports a value the appex
        // reads against a different scale.
        let parsed = AU_SHIM_TYPES_H
            .lines()
            .find_map(|l| l.trim().strip_prefix("#define MOOSE_AU_ABI_VERSION "))
            .map(|v| v.trim().trim_end_matches('u'))
            .and_then(|v| {
                v.strip_prefix("0x").map_or_else(
                    || v.parse::<u32>().ok(),
                    |h| u32::from_str_radix(h, 16).ok(),
                )
            })
            .expect("au_shim_types.h must #define MOOSE_AU_ABI_VERSION as an integer");
        assert_eq!(
            parsed,
            crate::ffi::MOOSE_AU_ABI_VERSION,
            "au_shim_types.h::MOOSE_AU_ABI_VERSION ({parsed}) differs from Rust \
             crate::ffi::MOOSE_AU_ABI_VERSION ({}); bump both together when appending a callback",
            crate::ffi::MOOSE_AU_ABI_VERSION,
        );
    }

    /// The AU v2 param-notify pump drains what the audio thread queues
    /// and joins cleanly on drop (no teardown hang). Uses a dummy,
    /// never-registered `ctx`: the C-side map lookup compares it by
    /// pointer identity and returns NULL, so the host-set FFI is a safe
    /// no-op - we exercise the queue/thread lifecycle, not `CoreAudio`.
    #[cfg(target_os = "macos")]
    #[test]
    fn param_notifier_drains_and_joins() {
        use std::time::{Duration, Instant};

        use moose_core::editor::SendPtr;

        use super::ParamNotifier;

        let dummy = core::ptr::dangling::<std::ffi::c_void>();
        // SAFETY: `dummy` is only ever compared (never dereferenced) by
        // the C map lookup, and the notifier is dropped (joined) below
        // while still in scope.
        let notifier =
            unsafe { ParamNotifier::spawn(SendPtr::new(dummy)) }.expect("spawn notifier");

        // Payload value is irrelevant (the host-set FFI no-ops here).
        for i in 0..100u32 {
            assert!(notifier.queue.push((i, 1.0)).is_ok());
        }
        notifier.thread.unpark();

        let start = Instant::now();
        while !notifier.queue.is_empty() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "notifier did not drain the queue"
            );
            std::thread::yield_now();
        }

        // Must stop + join without hanging.
        drop(notifier);
    }

    /// Build a prefilled free-list mirroring the constructor.
    fn sysex_queues() -> (SysExQueue, SysExQueue) {
        let free = SysExQueue::new(SYSEX_INPUT_SLOTS);
        for _ in 0..SYSEX_INPUT_SLOTS {
            let _ = free.push(SysExInput::default());
        }
        (free, SysExQueue::new(SYSEX_INPUT_SLOTS))
    }

    /// The host-thread queue and the audio-thread drain round-trip every
    /// message, in order, and recycle every buffer (no leak, no growth).
    /// This is the handoff that replaced the direct audio-scratch mutation
    /// that raced `cb_process`.
    #[test]
    fn sysex_input_handoff_delivers_and_recycles() {
        let (free, ready) = sysex_queues();
        let msgs: [(u32, &[u8]); 3] = [
            (0, &[0x7e, 0x00, 0x06, 0x01]), // universal device inquiry
            (10, &[0x43, 0x12, 0x40]),
            (0, &[0x11]),
        ];
        for (off, bytes) in msgs {
            queue_sysex_input(&free, &ready, off, bytes);
        }
        assert_eq!(ready.len(), 3);
        assert_eq!(free.len(), SYSEX_INPUT_SLOTS - 3);

        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        drain_sysex_input(&ready, &free, &mut events);

        let sysex: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
            .collect();
        assert_eq!(sysex.len(), 3, "every queued message delivered");
        for (i, (off, bytes)) in msgs.iter().enumerate() {
            assert_eq!(sysex[i].sample_offset, *off);
            assert_eq!(events.sysex_bytes(&sysex[i].body), *bytes);
        }

        // Buffers all recycled: total stays constant, none leaked or freed.
        assert_eq!(ready.len(), 0);
        assert_eq!(
            free.len(),
            SYSEX_INPUT_SLOTS,
            "all buffers returned to free"
        );
    }

    /// When the audio thread hasn't drained, the queue saturates at
    /// `SYSEX_INPUT_SLOTS` and further messages drop rather than allocate
    /// an unbounded backlog.
    #[allow(clippy::cast_possible_truncation)]
    #[test]
    fn sysex_input_drops_when_saturated() {
        let (free, ready) = sysex_queues();
        for i in 0..SYSEX_INPUT_SLOTS as u32 + 5 {
            queue_sysex_input(&free, &ready, i, &[i as u8]);
        }
        assert_eq!(ready.len(), SYSEX_INPUT_SLOTS, "capped at the buffer count");
        assert_eq!(free.len(), 0, "no free buffer left, extras dropped");
    }

    /// The plugin's declared version packs into the AU component version
    /// `(major << 16) | (minor << 8) | patch`, so a shipped update changes
    /// the value hosts key their AU caches on.
    #[test]
    fn au_version_packs_semver() {
        assert_eq!(au_version_u32("1.0.0"), 0x0001_0000);
        assert_eq!(au_version_u32("1.2.3"), 0x0001_0203);
        assert_eq!(au_version_u32("2.10.5"), 0x0002_0A05);
        assert_eq!(au_version_u32("0.38.0"), 0x0000_2600);
        // Unparseable components fall back per position (major -> 1).
        assert_eq!(au_version_u32(""), 0x0001_0000);
        assert_eq!(au_version_u32("1.2.3-beta"), 0x0001_0200);
    }
}
