//! Per-instance wrapper state and the GUI parameter change queue.

use crate::catalog_sync;
use crate::events::SoundingNotes;
use crate::gui::Extensions;
use crate::host_handle::{Any, HostHandle};
#[cfg(all(target_os = "linux", feature = "clap-draft"))]
use crate::wayland_nested;

use clap_sys::ext::latency::clap_host_latency;
use clap_sys::ext::params::clap_host_params;
use clap_sys::ext::state::clap_host_state;
use clap_sys::ext::tail::clap_host_tail;
use clap_sys::ext::track_info::clap_host_track_info;
use clap_sys::process::clap_process;
use moose_core::Sample;
use moose_core::TransportSlot;
use moose_core::bus_routing::BusRouting;
use moose_core::delay::ReportedDelay;
use moose_core::editor::{Editor, EditorBuilder};
use moose_core::events::EventList;
use moose_core::export::PluginExport;
use moose_core::host_queue::HostQueue;
use moose_core::info::PluginInfo;
use moose_core::main_thread::{HostNotifier, MainThreadHooks, NoteName};
use moose_core::meters::MeterStore;
use moose_core::plugin::PluginRuntime;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::tasks::AnyTaskSpawner;
use moose_core::thread::{MainThread, MainThreadCell};
use moose_core::wrapper::PluginCell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// GUI → host parameter change queue
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub(crate) enum GuiParamChange {
    GestureBegin(u32),
    Value(u32, f64), // (param_id, plain_value)
    GestureEnd(u32),
}

/// Wait-free bounded queue for GUI-initiated parameter changes.
///
/// GUI thread pushes, audio thread drains. Every push and pop is O(1)
/// and allocation-free, so the audio thread never blocks on the GUI
/// thread.
///
/// Capacity sized for the worst case "user wiggles every param at
/// once during MIDI-learn": ~64 widgets × (begin + value + end) per
/// gesture, with several blocks of headroom before the audio thread
/// next drains. Overflow drops the change - blocking or panicking
/// from the audio path is worse, and the host's next automation tick
/// recovers the lost values via the param tree. Per-instance memory
/// is `CAPACITY * sizeof(GuiParamChange)` (≈ 32 KB), one-time at
/// instance creation.
pub(crate) const GUI_QUEUE_CAPACITY: usize = 1024;
pub(crate) type GuiChangeQueue = crossbeam_queue::ArrayQueue<GuiParamChange>;

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
pub(crate) type StateLoadQueue = state::StateLoadQueue;

// ---------------------------------------------------------------------------
// Internal wrapper struct held as plugin_data
// ---------------------------------------------------------------------------

pub(crate) struct ClapPluginData<P: PluginExport> {
    pub(crate) persist_signal: Option<Arc<moose_params::PersistChangeSignal>>,
    pub(crate) host_state: *const clap_host_state,
    /// The user's plugin instance, owned directly by this wrapper instance
    /// inside [`PluginCell`]: the audio thread owns it per block, host
    /// lifecycle callbacks own it while processing is stopped, and no other
    /// thread holds a cloned `Arc` to it. `state_save` and the editor's
    /// `get_state` read the lock-free snapshot instead.
    pub(crate) plugin: PluginCell<P>,
    /// Stable handle to the params Arc, set once at instance creation.
    /// Host-thread callbacks (`params_get_value`, `params_value_to_text`,
    /// `params_text_to_value`) read params through this handle so a
    /// param query never touches the plugin. Params are
    /// atomic-backed and `Sync`.
    pub(crate) params_arc: Arc<P::Params>,
    /// Shared meter storage, set once at instance creation. The
    /// editor's `get_meter` closure reads these atomic slots instead
    /// of the plugin instance.
    pub(crate) meter_store: Arc<MeterStore>,
    /// Lock-free custom-state slot the audio thread publishes into, read
    /// by `save_state` so a snapshot-capable plugin's save never touches
    /// the plugin. Cached here on the instance, like `params_arc`.
    pub(crate) snapshot: Arc<SnapshotSlot>,
    /// The plugin's background-task spawner (`None` unless it wired
    /// `tasks:`), cached at creation so the editor schedules without
    /// touching the plugin.
    pub(crate) task_spawner: Option<AnyTaskSpawner>,
    /// Lock-free editor factory, cached at creation. Building the editor
    /// through this never touches the plugin (`--shell` builds rebuild
    /// from the reloaded dylib, so GUI edits hot-reload).
    pub(crate) editor_builder: EditorBuilder<P::Params>,
    /// The latency and tail reported to the host: published at `activate`
    /// only, because CLAP forbids a latency change while active.
    /// `latency.get` / `tail.get` read it without entering the processor
    /// cell; a block that sees either move raises `HostSignal::LATENCY`,
    /// and the main thread asks the host to restart.
    pub(crate) delay: ReportedDelay,
    /// Index into `P::bus_layouts()` of the port config the host last
    /// selected through `clap.audio-ports-config` (0 = the default first
    /// layout). Read by the audio-ports extension so a config switch is
    /// reflected in the ports the host sees. Set on the main thread while
    /// the plugin is deactivated; the audio scratch is pre-sized to the
    /// widest layout, so a wider selection never allocates in `process`.
    pub(crate) selected_config: AtomicU32,
    /// Which enabled audio ports the host switched off through
    /// `clap.audio-ports-activation` (bit = port index; inputs, outputs).
    /// Applied to the bus routing at `activate`; until the host uses the
    /// extension every bus reports `BusActivation::Unknown`.
    pub(crate) ports_inactive: [AtomicU32; 2],
    /// The host has used `clap.audio-ports-activation`.
    pub(crate) ports_activation_used: AtomicBool,
    /// Audio + lifecycle-owned per-block scratch. Behind a `PluginCell` so
    /// every callback reaches it through a shared `&ClapPluginData` - never a
    /// whole-struct `&mut`, which would alias a concurrent host-thread `&`
    /// (param reads, GUI) and is UB under the aliasing model. The CLAP host
    /// contract serializes its owners: `process` and `reset` on the audio
    /// thread (`reset` is `[audio-thread & active]`, never concurrent with
    /// `process`), `activate` / `deactivate` on the main thread while
    /// stopped - so the cell is never held twice at once.
    pub(crate) audio: PluginCell<ClapAudio<P>>,
    /// The parameter list the host sees (main-thread readers). Replaced
    /// only while inactive, together with `ClapAudio::catalog`; see
    /// `catalog_sync`.
    pub(crate) exposed: Mutex<catalog_sync::ExposedParams>,
    pub(crate) parameter_restart_pending: AtomicBool,
    /// The parameter set's change signal, bound at init and closed at
    /// destroy. Its wake schedules `on_main_thread`.
    pub(crate) catalog_signal: Option<Arc<moose_params::CatalogSignal>>,
    /// A structure change the host hasn't seen yet, with the ids it
    /// removed or retyped. Published at the next inactive sync.
    pub(crate) pending_structure: MainThreadCell<Option<Vec<u32>>>,
    /// The plugin's main-thread part (`PluginExport::main_thread`).
    pub(crate) hooks: MainThreadCell<Box<dyn MainThreadHooks>>,
    /// Every "tell the host later": the editor's dirty / restart /
    /// context-menu requests, latency changes from the audio thread,
    /// rescans, persist and catalog wakes, plugin notices. Its wake is
    /// `request_callback`; `on_main_thread` drains it. Closed at destroy.
    pub(crate) host_queue: Arc<HostQueue>,
    /// The plugin's notices, on `host_queue`.
    pub(crate) host_notifier: HostNotifier,
    /// The note names taken by the last `note_name.count`.
    pub(crate) note_names: MainThreadCell<Vec<NoteName>>,
    /// Whether the hooks reported voice info at init (`voice_info`).
    pub(crate) voice_info_supported: AtomicBool,
    /// Cached plugin info. Read by the chunker each block for
    /// `automation` and otherwise unused; the
    /// rest of the wrapper consumes `PluginInfo` from the C ABI
    /// surface we build at registration.
    pub(crate) info: PluginInfo,
    /// Pre-hashed plugin ID for state serialization.
    pub(crate) plugin_id_hash: u64,
    /// Main/UI-thread-owned editor state, behind a `PluginCell` for the same
    /// reason as `audio`. Its owners are the GUI callbacks, all on the host
    /// main thread; `process` never touches it.
    pub(crate) gui: PluginCell<ClapGui>,
    /// Phase-gated host handle (`[thread-safe]` subset; narrow with
    /// `on_main_thread(&main)` for `[main-thread]` host extension calls).
    pub(crate) host: HostHandle<Any>,
    /// Host params extension (for `request_flush`).
    pub(crate) host_params: *const clap_host_params,
    /// Host latency extension (for `changed`). Null if the host doesn't
    /// expose `clap.latency`.
    pub(crate) host_latency: *const clap_host_latency,
    /// Host tail extension (for `[audio-thread]` `changed`). Null if the host
    /// doesn't expose `clap.tail`.
    pub(crate) host_tail: *const clap_host_tail,
    /// Host track-info extension. Null if the host doesn't expose it.
    pub(crate) host_track_info: *const clap_host_track_info,
    /// A `HostNotice::AUDIO_PORTS` waits for the plugin to be inactive,
    /// where the host may rescan the port list.
    pub(crate) ports_rescan: AtomicBool,
    /// Queue of GUI-initiated parameter changes to emit as output events.
    pub(crate) gui_changes: Arc<GuiChangeQueue>,
    pub(crate) gui_overflow: Arc<AtomicBool>,
    /// Bounded SPSC handoff for state loads. Host (`state_load`) and
    /// editor (`set_state` callback) deserialize on their thread and
    /// push the result; the audio thread pops at the top of
    /// `clap_plugin_process` and calls [`state::apply_state`] under
    /// its exclusive `&mut plugin`. The queue is what makes the
    /// transfer race-free: no GUI-thread `&mut plugin` is ever
    /// constructed.
    pub(crate) pending_state: Arc<StateLoadQueue>,
    /// Set by a panic in `process`; cleared by the next activate.
    pub(crate) panic_latch: moose_core::wrapper::PanicLatch,
    /// `true` between `activate` and `deactivate`. `state_load` reads it
    /// to decide whether the audio thread will drain `pending_state`: if
    /// inactive, no `process` runs, so it applies the custom-state blob
    /// synchronously instead of leaving it stranded in the queue (which
    /// would let a following `get_state` re-serialize stale extra
    /// state). Main-thread-only (`activate` / `deactivate` / `state_load`
    /// are serialized by the host).
    pub(crate) active: AtomicBool,
    /// True from a successful `start_processing` until `stop_processing` returns.
    pub(crate) processing: AtomicBool,
    /// Current render mode as a [`ProcessMode`] discriminant. The host
    /// sets it through the `clap.render` extension on the main thread;
    /// the audio thread reads it each block. Most hosts deactivate
    /// before switching to offline, so `activate` re-preps with the new
    /// mode; a mode change while active still reaches `process` through
    /// the per-block `ProcessContext`, but reallocation waits for the
    /// next `activate`.
    pub(crate) render_mode: AtomicU8,
    pub(crate) floating_mode: AtomicBool,
    pub(crate) menu_popup_supported: AtomicBool,
    pub(crate) floating_visible: Arc<AtomicBool>,
    /// Last `Params::parameter_presentation_revision` the host saw.
    pub(crate) presentation_revision: AtomicU64,
    /// Main-thread discovery cache; audio processing never reads or locks it.
    pub(crate) remote_pages: Mutex<Vec<(String, Vec<u32>)>>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    pub(crate) transport_slot: Arc<TransportSlot>,
    /// Host-reported GUI scale (via `clap_plugin_gui::set_scale`).
    /// Sources of truth, by platform:
    /// - **macOS**: `gui_set_scale` refuses, so this stays unset
    ///   (`AppKit` handles backing scale through the parent `NSView`; we
    ///   report logical points and let the OS scale).
    /// - **Windows / Linux**: used at `gui_get_size` to convert
    ///   logical→physical. Unset, the editor's own window scale is used
    ///   instead (see `window_scale`): 1.0 in a non-DPI-aware host, the
    ///   monitor DPI or `Xft.dpi` otherwise. HiDPI-aware hosts call
    ///   `set_scale` before `gui_get_size`.
    ///
    /// f64 bits, `0` until the host calls `set_scale`; GUI-thread-only, but atomic so `gui_get_size`, `set_scale`,
    /// and the `gui_create` resize closure reach it through the shared
    /// `&ClapPluginData` without a `&mut *ctx` - and so it stays outside the
    /// `gui` cell, which the resize closure would otherwise re-enter.
    pub(crate) host_scale: AtomicU64,
    /// The open editor's own window scale (`Editor::window_scale`, f64 bits,
    /// `0` = none), refreshed by the size callbacks. Converts sizes while the
    /// host never called `set_scale`, so the reported size matches the child
    /// window the editor created at the monitor DPI / `Xft.dpi`.
    pub(crate) window_scale: AtomicU64,
    /// A resize the plugin requested through `PluginContext::request_resize`
    /// that came back into a GUI callback (`gui_set_size` / `gui_get_size`)
    /// synchronously - the host answering `request_host_resize` on the same
    /// thread - while the `gui` cell was already held. Stashed as physical
    /// `(w << 32) | h` (both non-zero, `0` = none) instead of re-entering the
    /// cell; `gui_get_size` applies it on the next size query. GUI thread only.
    pub(crate) pending_resize: AtomicU64,
    /// The immutable extension vtables this instance hands the host from
    /// `clap_plugin_get_extension`. Stored per-instance rather than in a
    /// generic function-local static: a static inside a generic fn is
    /// shared across every monomorphization, so a binary carrying two
    /// plugin types would reinterpret one type's vtables as the other's.
    /// The pointers returned point into this box and stay valid for the
    /// instance's lifetime, which is exactly how long the host uses them.
    pub(crate) extensions: Extensions<P>,
}

/// Audio + lifecycle-owned per-block scratch (see [`ClapPluginData::audio`]).
pub(crate) struct ClapAudio<P: PluginExport> {
    /// Re-usable event list for converting CLAP events each process call.
    pub(crate) event_list: EventList,
    /// Note lifecycle tracker backing wildcard release/expression matching and
    /// successful outbound `NOTE_END` cleanup. Audio-thread only.
    pub(crate) sounding_notes: SoundingNotes,
    /// Re-usable output event list for the process context.
    pub(crate) output_events: EventList,
    /// Per-sub-block scratch the chunker writes rebased events into
    /// while walking the audio block. Pre-allocated to the same
    /// capacity as `event_list` so steady-state operation stays
    /// allocation-free.
    pub(crate) sub_event_scratch: EventList,
    /// Param changes a stopped-transport `params_flush` applied, kept
    /// latest-wins per id and replayed at offset 0 of the next process
    /// block, so the plugin sees the host's explicit events (including
    /// same-value ones) rather than only a changed snapshot. Capacity is
    /// the param count, so it never grows.
    pub(crate) deferred_params: Vec<(u32, f64)>,
    /// Current sample rate.
    pub(crate) sample_rate: f64,
    /// Current max block size.
    pub(crate) max_block_size: usize,
    /// Persistent input/output channel-slice scratch reused across
    /// process callbacks so the audio thread doesn't allocate per
    /// block. The `'static` annotation is fictional - the slices
    /// actually point into the host's per-block buffers; each
    /// `process` call rebuilds them and clears them on exit so no
    /// dangling pointer lives between blocks.
    pub(crate) input_slices: Vec<&'static [<P as PluginRuntime>::Sample]>,
    pub(crate) output_slices: Vec<&'static mut [<P as PluginRuntime>::Sample]>,
    /// Per-channel input conversion scratch. A slot stays empty when
    /// the host wire for its port matches `P::Sample` (the slice in
    /// `input_slices` points straight into host memory); on a
    /// precision mismatch the channel is converted into the matching
    /// slot here and the slice points there.
    pub(crate) input_widen: Vec<Vec<<P as PluginRuntime>::Sample>>,
    /// Per-channel output conversion scratch. Same shape: only used
    /// on a precision mismatch, in which case the plugin writes here
    /// and the wrapper copies + converts back to the host's output
    /// pointers after `process()` returns.
    pub(crate) output_narrow: Vec<Vec<<P as PluginRuntime>::Sample>>,
    /// Cached pointers to host output channels, captured at slice
    /// build time so the post-`process` convert-back loop can copy
    /// without re-walking the CLAP bus structures. Each entry is
    /// tagged with the wire precision the host picked for its port.
    pub(crate) host_out_ptrs: Vec<HostOutPtr>,
    /// Selected layout's structural bus order and flattened ranges. Built at
    /// activate; CLAP exposes no truthful per-block connection state.
    pub(crate) bus_routing: BusRouting,
    /// The audio thread's parameter catalog. Replaced only while inactive
    /// (see `catalog_sync`), so `process` reads it without a lock.
    pub(crate) catalog: Arc<moose_params::ParamCatalog>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuiMode {
    Embedded,
    FloatingX11,
    FloatingWayland,
    /// Embedded through the host's nested compositor (`wayland_nested`).
    WaylandNested,
}

/// Main/UI-thread-owned editor state (see [`ClapPluginData::gui`]).
pub(crate) struct ClapGui {
    /// GUI editor (created by the plugin, if it implements `editor()`).
    pub(crate) editor: Option<Box<dyn Editor>>,
    /// Whether the GUI has been created via the gui extension.
    pub(crate) gui_created: bool,
    pub(crate) mode: GuiMode,
    pub(crate) visible: bool,
    pub(crate) title: String,
    #[cfg(all(target_os = "linux", feature = "clap-draft"))]
    pub(crate) nested: wayland_nested::Session,
}

impl<P: PluginExport> ClapPluginData<P> {
    /// The scale between the host's physical pixels and the editor's logical
    /// size: the host's `gui.set_scale` value, else the open editor's window
    /// scale, else `1.0`. Never both multiplied.
    pub(crate) fn host_scale(&self) -> f64 {
        self.reported_host_scale()
            .or_else(|| match self.window_scale.load(Ordering::Relaxed) {
                0 => None,
                bits => Some(f64::from_bits(bits)),
            })
            .unwrap_or(1.0)
    }

    /// Cache `editor`'s window scale for [`Self::host_scale`].
    pub(crate) fn note_window_scale(&self, editor: &dyn Editor) {
        let bits = editor
            .window_scale()
            .filter(|s| s.is_finite() && *s > 0.0)
            .map_or(0, f64::to_bits);
        self.window_scale.store(bits, Ordering::Relaxed);
    }

    /// The last scale the host passed to `gui.set_scale`, if it ever did.
    /// Bits `0` (`+0.0`, which `gui_set_scale` rejects) mean "never".
    pub(crate) fn reported_host_scale(&self) -> Option<f64> {
        match self.host_scale.load(Ordering::Relaxed) {
            0 => None,
            bits => Some(f64::from_bits(bits)),
        }
    }

    pub(crate) fn set_host_scale(&self, scale: f64) {
        self.host_scale.store(scale.to_bits(), Ordering::Relaxed);
    }

    /// Run `f` on the plugin's main-thread hooks. `None` on a re-entrant
    /// host call that arrives while the hooks are already in use.
    pub(crate) fn with_hooks<R>(
        &self,
        main: &MainThread,
        f: impl FnOnce(&mut dyn MainThreadHooks) -> R,
    ) -> Option<R> {
        let mut hooks = self.hooks.try_borrow_mut(main)?;
        Some(f(hooks.as_mut()))
    }
}

/// Host output channel pointer, tagged with the port's wire
/// precision (the host picks 32- or 64-bit per port; 64-bit only on
/// ports that advertised `CLAP_AUDIO_PORT_SUPPORTS_64BITS`).
#[derive(Copy, Clone)]
pub(crate) enum HostOutPtr {
    Null,
    F32(*mut f32),
    F64(*mut f64),
}

/// Build the plugin-facing input slice for one host channel of wire
/// precision `H`: zero-copy when `H` matches the plugin's sample
/// type `S`, otherwise converted into `scratch` (f64 round-trip,
/// lossless in the widening direction).
///
/// # Safety
/// A non-null `host_ptr` must address `num_frames` readable samples
/// that stay valid for the block; the returned slice's `'static`
/// lifetime is fictional (same convention as `input_slices`).
pub(crate) unsafe fn input_channel_slice<S: Sample, H: Sample>(
    host_ptr: *const H,
    num_frames: usize,
    scratch: &mut Vec<S>,
    copy: bool,
) -> &'static [S] {
    if host_ptr.is_null() {
        // Unconnected channel (an unrouted sidechain port). The plugin
        // negotiated it, so hand back block-length silence, not an empty
        // slice it would read out of range.
        scratch.clear();
        scratch.resize(num_frames, S::default());
        // SAFETY: `scratch` outlives the returned slice (same fictional
        // 'static convention as the non-null path).
        return unsafe { std::slice::from_raw_parts(scratch.as_ptr(), num_frames) };
    }
    unsafe {
        if S::IS_F64 == H::IS_F64 && !copy {
            // SAFETY: sealed traits - equal IS_F64 means S == H.
            return std::slice::from_raw_parts(host_ptr.cast::<S>(), num_frames);
        }
        scratch.clear();
        scratch.reserve(num_frames);
        if S::IS_F64 == H::IS_F64 {
            scratch.extend_from_slice(std::slice::from_raw_parts(host_ptr.cast::<S>(), num_frames));
        } else {
            let host = std::slice::from_raw_parts(host_ptr, num_frames);
            for &h in host {
                scratch.push(S::from_f64(h.to_f64()));
            }
        }
        std::slice::from_raw_parts(scratch.as_ptr(), num_frames)
    }
}

// Detect overlap before creating any mutable output references.
pub(crate) unsafe fn input_overlaps_outputs(
    input: *const u8,
    len: usize,
    process: &clap_process,
) -> bool {
    if input.is_null() || len == 0 {
        return false;
    }
    let start = input as usize;
    let end = start.saturating_add(len);
    for bus in 0..process.audio_outputs_count {
        let output = unsafe { &*process.audio_outputs.add(bus as usize) };
        for channel in 0..output.channel_count {
            let (ptr, bytes) = if !output.data64.is_null() {
                (
                    unsafe { *output.data64.add(channel as usize) } as usize,
                    size_of::<f64>(),
                )
            } else if !output.data32.is_null() {
                (
                    unsafe { *output.data32.add(channel as usize) } as usize,
                    size_of::<f32>(),
                )
            } else {
                continue;
            };
            if ptr != 0
                && start < ptr.saturating_add((process.frames_count as usize).saturating_mul(bytes))
                && ptr < end
            {
                return true;
            }
        }
    }
    false
}

/// Output twin of [`input_channel_slice`]: zero-copy into host
/// memory when precisions match, otherwise the plugin renders into
/// `scratch` and the post-`process` loop converts back to the host
/// pointer.
///
/// # Safety
/// Same contract as [`input_channel_slice`], with `num_frames`
/// writable samples.
pub(crate) unsafe fn output_channel_slice<S: Sample, H: Sample>(
    host_ptr: *mut H,
    num_frames: usize,
    scratch: &mut Vec<S>,
) -> &'static mut [S] {
    if host_ptr.is_null() {
        // Unconnected output channel: block-length discard scratch to
        // write into rather than an empty slice. The caller records
        // `HostOutPtr::Null`, so the copy-back skips this slot.
        scratch.clear();
        scratch.resize(num_frames, S::default());
        // SAFETY: `scratch` outlives the returned slice.
        return unsafe { std::slice::from_raw_parts_mut(scratch.as_mut_ptr(), num_frames) };
    }
    unsafe {
        if S::IS_F64 == H::IS_F64 {
            // SAFETY: sealed traits - equal IS_F64 means S == H.
            return std::slice::from_raw_parts_mut(host_ptr.cast::<S>(), num_frames);
        }
        scratch.clear();
        scratch.resize(num_frames, S::default());
        std::slice::from_raw_parts_mut(scratch.as_mut_ptr(), num_frames)
    }
}
