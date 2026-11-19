//! Lifecycle, host-notice and bus-layout callbacks.

use crate::ffi;
use crate::input_events::VST3_EVENT_END;
use crate::instance::{
    K_IO_CHANGED, OutputNoteIds, PendingOutputMutation, Vst3Gui, Vst3Instance, Vst3Scratch,
};
use crate::midi_proxy::{
    MIDI_PROXY_PER_CHANNEL, allocate_midi_proxy_ids, midi_proxy_default, midi_proxy_len,
};
use crate::output_events::MidiMap;
use crate::registration::max_layout_channels;
use crate::state::StateLoadQueue;

use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::bus::{BusConfig, BusKind, BusLayout};
use moose_core::config::{AudioConfig, ProcessMode};
use moose_core::editor::SendPtr;
use moose_core::events::{EVENT_LIST_PREALLOC, EventList, LosslessEventCursor};
use moose_core::export::PluginExport;
use moose_core::main_thread::{
    HostColor, HostNotice, HostNotifier, LearnSource, Midi2Controller, Midi2ControllerKind,
    TrackInfo,
};
use moose_core::plugin::{PreparationConfig, PreparationFrameBound};
use moose_core::rt::RtSection;
use moose_core::state;
use moose_core::thread::{MainThread, MainThreadCell};
use moose_core::wrapper::{
    PluginCell, apply_queued_state, enter_plugin, find_bus_layout, run_extern_callback_with,
    shared_plugin,
};
use moose_params::{MidiSource, ParamFunction, ParamRange, Params, PersistChangeSignal};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// C callback implementations
//
// SAFETY for all unsafe extern "C" fn below:
// - `ctx` is a *mut c_void created by Box::into_raw in cb_create().
//   Valid until cb_destroy() (called exactly once by the C++ shim).
// - The C++ shim (MooseComponent) owns the Rust context and
//   guarantees exclusive access: process() on the audio thread,
//   all other callbacks on the main thread, never concurrent.
// - Audio buffer pointers come from the VST3 host via ProcessData
//   and are valid for the declared channel count × numSamples.
// - Parameter IDs and values come from IParamValueQueue and are
//   guaranteed valid by the VST3 host.
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn cb_create<P: PluginExport>() -> *mut std::ffi::c_void {
    // Author `create` / `init` run here; a panic must not cross the FFI
    // boundary. A null return tells the host construction failed.
    run_extern_callback_with::<P, *mut std::ffi::c_void>(
        "vst3",
        "create",
        std::ptr::null_mut(),
        || {
            let mut plugin = P::create();
            plugin.init();
            let info = P::info();
            let param_infos = plugin.params().param_catalog();
            let mut param_ranges: Vec<(u32, ParamRange)> =
                param_infos.iter().map(|i| (i.id, i.range)).collect();
            // Sort by id so `binary_search_by_key` works in the hot lookups.
            param_ranges.sort_by_key(|(id, _)| *id);
            // Precompute the MIDI-controller bindings, sorted by id, so the
            // audio thread bridges mapped controllers without a linear scan.
            let mut midi_maps: Vec<(u32, MidiMap)> = param_infos
                .iter()
                .filter_map(|i| MidiMap::from_param(i).map(|m| (i.id, m)))
                .collect();
            midi_maps.sort_by_key(|(id, _)| *id);
            let params_arc = plugin.params_arc();
            let mut hooks = plugin.main_thread();
            // VST3 has no note-name / voice-info changed call; a port
            // change becomes `kIoChanged`. The wake learns the instance
            // once it is boxed; `cb_destroy` closes it before the free.
            let notice_ctx = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
            let host_notifier = HostNotifier::new({
                let notice_ctx = Arc::clone(&notice_ctx);
                // SAFETY: the pointer is the live instance until
                // `cb_destroy` closes this wake.
                move || unsafe { host_notice_woke::<P>(notice_ctx.load(Ordering::Acquire)) }
            });
            hooks.connect(host_notifier.clone());
            let main_thread = MainThreadCell::new(hooks);
            let meter_store = plugin.meter_store();
            let snapshot = plugin.snapshot_slot();
            let task_spawner = plugin.task_spawner();
            let editor_builder = plugin.editor_builder();
            let latency_cache = AtomicU32::new(plugin.latency());
            let tail_cache = AtomicU32::new(plugin.tail());
            let midi_proxy_ids = allocate_midi_proxy_ids(&param_infos, midi_proxy_len::<P>());
            // One point per parameter per block fits next to the note
            // events; a mapped controller pushes two (event + change).
            // Points past this are committed as the queue's final value.
            let event_capacity = EVENT_LIST_PREALLOC + 2 * param_infos.len();
            let midi_proxy_values = (0..midi_proxy_ids.len())
                .map(|i| {
                    // Bounded by the proxy count.
                    #[allow(clippy::cast_possible_truncation)]
                    let controller = (i as u32) % MIDI_PROXY_PER_CHANNEL;
                    AtomicU64::new(midi_proxy_default(controller).to_bits())
                })
                .collect();
            let instance = Box::new(Vst3Instance::<P> {
                persist_signal: std::sync::Mutex::new(None),
                catalog_signal: std::sync::Mutex::new(None),
                main_thread,
                plugin: shared_plugin(plugin),
                params_arc,
                meter_store,
                snapshot,
                task_spawner,
                editor_builder,
                param_infos,
                automation: info.automation,
                plugin_id_hash: state::shared_plugin_state_hash(&info),
                active: AtomicBool::new(false),
                processing: AtomicBool::new(false),
                param_ranges,
                midi_maps,
                transport_slot: TransportSlot::new(),
                pending_state: Arc::new(StateLoadQueue::new(1)),
                panic_latch: moose_core::wrapper::PanicLatch::new(),
                latency_cache,
                tail_cache,
                midi_proxy_ids,
                midi_proxy_values,
                host_scale: AtomicU64::new(0),
                window_scale: AtomicU64::new(0),
                pending_resize: AtomicU64::new(0),
                audio: PluginCell::new(Vst3Scratch {
                    event_list: EventList::with_capacity(event_capacity),
                    input_num_frames: 0,
                    output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
                    sub_event_scratch: EventList::with_capacity(event_capacity),
                    sample_rate: 44100.0,
                    // 8192 covers the largest block sizes mainstream DAWs /
                    // validators use (Reaper / pluginval <= 4096); a non-zero
                    // default keeps the process-before-activate path from
                    // tripping the contract assert.
                    max_block_size: 8192,
                    prepared: false,
                    scratch: RawBufferScratch::default(),
                    output_cursor: LosslessEventCursor::default(),
                    output_preflight_status: VST3_EVENT_END,
                    output_note_ids: OutputNoteIds::new(),
                    pending_output_mutation: PendingOutputMutation::None,
                }),
                gui: PluginCell::new(Vst3Gui { editor: None }),
                host_notifier,
            });
            let raw = Box::into_raw(instance);
            notice_ctx.store(raw.cast(), Ordering::Release);
            raw.cast::<std::ffi::c_void>()
        },
    )
}

/// A [`HostNotice`] arrived: flag `kIoChanged` for a port change.
///
/// # Safety
/// `ctx` is null or a live `Vst3Instance<P>`.
pub(crate) unsafe fn host_notice_woke<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    // SAFETY: caller contract.
    let Some(instance) = (unsafe { ctx.cast::<Vst3Instance<P>>().as_ref() }) else {
        return;
    };
    if instance
        .host_notifier
        .take()
        .contains(HostNotice::AUDIO_PORTS)
    {
        ffi::moose_vst3_mark_restart(ctx, K_IO_CHANGED);
    }
}

pub(crate) unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    // Dropping the instance cascades into the editor's `Drop` (wgpu surface
    // / NSView / baseview / runloop-timer teardown). A panic there would
    // unwind across this `extern "C"` boundary and abort the host. Hosts
    // destroy instances routinely mid-session - removing a plugin from a
    // track, closing a project - not just at quit, so swallowing the panic
    // keeps a live host alive; the instance is being torn down regardless.
    run_extern_callback_with::<P, ()>("vst3", "destroy", (), || unsafe {
        if !ctx.is_null() {
            let instance = Box::from_raw(ctx.cast::<Vst3Instance<P>>());
            instance.host_notifier.close();
            rebind_signal(&instance.persist_signal, None, std::ptr::null_mut());
            rebind_signal(&instance.catalog_signal, None, std::ptr::null_mut());
            drop(instance);
        }
    });
}

// All three callbacks run on the host thread. Only the installed wake itself
// may run on a background worker; close() gates it before the shim tears down.
pub(crate) unsafe extern "C" fn cb_wants_persist_notifications<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> bool {
    run_extern_callback_with::<P, bool>("vst3", "persist_wants", false, || unsafe {
        (&*ctx.cast::<Vst3Instance<P>>())
            .params_arc
            .wants_persist_notifications()
    })
}
pub(crate) unsafe extern "C" fn cb_set_persist_waker<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    wake: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    wake_context: *mut std::ffi::c_void,
) {
    run_extern_callback_with::<P, ()>("vst3", "persist_bind", (), || unsafe {
        let instance = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(signal) = rebind_signal(&instance.persist_signal, wake, wake_context) {
            instance.params_arc.set_persist_change_signal(signal);
        }
    });
}

/// Close the signal in `slot` (waiting for a running wake) and, with
/// `wake`, install one that calls `wake(wake_context)`; returns it for the
/// params setter.
///
/// # Safety
/// `wake_context` stays valid for `wake` until the next rebind of `slot`.
pub(crate) unsafe fn rebind_signal(
    slot: &std::sync::Mutex<Option<Arc<PersistChangeSignal>>>,
    wake: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    wake_context: *mut std::ffi::c_void,
) -> Option<std::sync::Weak<PersistChangeSignal>> {
    let mut binding = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(old) = binding.take() {
        old.close();
    }
    let wake = wake?;
    // SAFETY: the caller keeps `wake_context` alive until it rebinds, and
    // `close` in that rebind waits out a running wake.
    let context = unsafe { SendPtr::new(wake_context) };
    // SAFETY: as above; `wake` is the COM layer's wake for that context.
    let signal = Arc::new(PersistChangeSignal::new(move || unsafe {
        wake(context.as_ptr().cast_mut());
    }));
    let weak = Arc::downgrade(&signal);
    *binding = Some(signal);
    Some(weak)
}

pub(crate) unsafe extern "C" fn cb_set_catalog_waker<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    wake: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    wake_context: *mut std::ffi::c_void,
) {
    run_extern_callback_with::<P, ()>("vst3", "catalog_bind", (), || unsafe {
        let instance = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(signal) = rebind_signal(&instance.catalog_signal, wake, wake_context) {
            instance.params_arc.set_catalog_signal(signal);
        }
    });
}

pub(crate) unsafe extern "C" fn cb_take_catalog_changes<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u8 {
    run_extern_callback_with::<P, u8>("vst3", "catalog_changes", 0, || unsafe {
        (*ctx.cast::<Vst3Instance<P>>())
            .params_arc
            .take_catalog_changes()
            .flags
            .bits()
    })
}

pub(crate) unsafe extern "C" fn cb_param_affects_presentation<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> bool {
    run_extern_callback_with::<P, bool>(
        "vst3",
        "parameter_affects_presentation",
        false,
        || unsafe {
            (*ctx.cast::<Vst3Instance<P>>())
                .params_arc
                .parameter_affects_presentation(id)
        },
    )
}

/// The SDK `FunctionNameType` string for `function`.
pub(crate) fn function_name(function: ParamFunction) -> *const c_char {
    use vst3::Steinberg::Vst::FunctionNameType::{
        kCompGainReduction, kCompGainReductionMax, kCompGainReductionPeakHold,
        kCompResetGainReductionMax, kDryWetMix, kLowLatencyMode, kRandomize,
    };
    match function {
        ParamFunction::DryWetMix => kDryWetMix,
        ParamFunction::LowLatencyMode => kLowLatencyMode,
        ParamFunction::Randomize => kRandomize,
        ParamFunction::GainReduction => kCompGainReduction,
        ParamFunction::GainReductionMax => kCompGainReductionMax,
        ParamFunction::GainReductionPeakHold => kCompGainReductionPeakHold,
        ParamFunction::ResetGainReductionMax => kCompResetGainReductionMax,
        _ => std::ptr::null(),
    }
}

pub(crate) unsafe extern "C" fn cb_param_function_name<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> *const c_char {
    run_extern_callback_with::<P, *const c_char>(
        "vst3",
        "parameter_function",
        std::ptr::null(),
        || unsafe {
            (*ctx.cast::<Vst3Instance<P>>())
                .params_arc
                .parameter_function(id)
                .map_or(std::ptr::null(), function_name)
        },
    )
}

/// A VST3 `ColorSpec` (`0xAARRGGBB`).
pub(crate) fn argb_color(argb: u32) -> HostColor {
    let [a, r, g, b] = argb.to_be_bytes();
    HostColor { r, g, b, a }
}

pub(crate) unsafe extern "C" fn cb_track_info_changed<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    info: *const ffi::Vst3TrackInfo,
) {
    run_extern_callback_with::<P, ()>("vst3", "track_info", (), || unsafe {
        let (Some(inst), Some(info)) = (ctx.cast::<Vst3Instance<P>>().as_ref(), info.as_ref())
        else {
            return;
        };
        let mut track = TrackInfo::new();
        track.name = (!info.name.is_null())
            .then(|| CStr::from_ptr(info.name).to_string_lossy().into_owned());
        track.color = info.has_color.then(|| argb_color(info.color_argb));
        // SAFETY: the COM layer calls this only from
        // `IInfoListener::setChannelContextInfos`, an edit-controller call
        // VST3 makes on the UI thread.
        let main = MainThread::assume();
        if let Some(mut hooks) = inst.main_thread.try_borrow_mut(&main) {
            hooks.track_info_changed(&main, &track);
        }
    });
}

pub(crate) unsafe extern "C" fn cb_midi_learn<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    bus: u32,
    channel: u8,
    source: ffi::Vst3LearnSource,
) -> i32 {
    run_extern_callback_with::<P, i32>("vst3", "midi_learn", 0, || unsafe {
        let source = match source {
            ffi::Vst3LearnSource::Midi1(controller) => {
                vst3_midi_source(controller).map(LearnSource::Midi1)
            }
            ffi::Vst3LearnSource::Midi2 {
                registered,
                bank,
                index,
            } => Some(LearnSource::Midi2(Midi2Controller {
                kind: if registered {
                    Midi2ControllerKind::Registered
                } else {
                    Midi2ControllerKind::Assignable
                },
                bank,
                index,
            })),
        };
        let (Some(inst), Some(source)) = (ctx.cast::<Vst3Instance<P>>().as_ref(), source) else {
            return 0;
        };
        // SAFETY: the COM layer calls this only from
        // `IMidiLearn::onLiveMIDIControllerInput` / `IMidiLearn2`, which
        // VST3 runs on the UI thread.
        let main = MainThread::assume();
        let learned = inst
            .main_thread
            .try_borrow_mut(&main)
            .is_some_and(|mut hooks| hooks.midi_learn(&main, bus, channel, source));
        i32::from(learned)
    })
}

/// A VST3 `ControllerNumbers` value (`ivstmidicontrollers.h`): 0..=127 are
/// CCs, then kAfterTouch, kPitchBend, kCtrlProgramChange.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn vst3_midi_source(controller: i16) -> Option<MidiSource> {
    match controller {
        0..=127 => Some(MidiSource::Cc(controller as u8)),
        128 => Some(MidiSource::ChannelPressure),
        129 => Some(MidiSource::PitchBend),
        130 => Some(MidiSource::ProgramChange),
        _ => None,
    }
}
pub(crate) unsafe extern "C" fn cb_take_persist_change<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: *mut u32,
    value: *mut f64,
) -> bool {
    run_extern_callback_with::<P, bool>("vst3", "persist_drain", false, || unsafe {
        let params = &(&*ctx.cast::<Vst3Instance<P>>()).params_arc;
        let Some((changed_id, normalized)) = params.take_persist_change() else {
            return false;
        };
        if !normalized.is_finite()
            || !(0.0..=1.0).contains(&normalized)
            || params.get_normalized(changed_id).is_none()
        {
            return false;
        }
        *id = changed_id;
        *value = params.set_normalized_returning_normalized(changed_id, normalized);
        true
    })
}

/// Map a VST3 `ProcessModes` value (`kRealtime` 0, `kPrefetch` 1,
/// `kOffline` 2) to a moose [`ProcessMode`]. Unknown values fall back
/// to `Realtime`.
pub(crate) fn vst3_process_mode(mode: i32) -> ProcessMode {
    match mode {
        1 => ProcessMode::Buffered,
        2 => ProcessMode::Offline,
        _ => ProcessMode::Realtime,
    }
}

pub(crate) unsafe extern "C" fn cb_prepare<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
    process_mode: i32,
) -> i32 {
    // Author `reset` can panic (allocation, DSP prep); firewall it so the
    // panic can't unwind across the C ABI and abort the host.
    run_extern_callback_with::<P, i32>("vst3", "prepare", 0, || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut audio = inst.audio.enter();
        audio.prepared = false;
        let max_frames = max_frames as usize;
        let config = PreparationConfig::new(
            AudioConfig::new(sample_rate, max_frames)
                .with_process_mode(vst3_process_mode(process_mode)),
            PreparationFrameBound::NegotiatedHost,
        );
        if config.validate().is_err() {
            return 0;
        }
        audio.sample_rate = sample_rate;
        audio.max_block_size = max_frames;
        // Grow per-block scratch to cover the *widest* declared layout and
        // this block size before the first process() call so the audio
        // thread stays alloc-free. VST3 negotiates layouts at runtime
        // (setBusArrangements), so a later, wider layout than the default
        // must not grow the per-channel Vecs on the audio thread - size to
        // the same structural width the shim gathers, including disabled
        // optional buses that retain null/silence positions in the flat
        // array. Active-only totals would leave `build` allocating here on
        // the audio thread when every declared optional bus is disabled.
        let (num_in, num_out) = max_layout_channels(&P::bus_layouts());
        audio
            .scratch
            .ensure_capacity(num_in as usize, num_out as usize, max_frames.max(1024));
        {
            let mut plugin = enter_plugin(&inst.plugin);
            if plugin.prepare(&config).is_err() {
                return 0;
            }
            inst.panic_latch.clear();
            inst.latency_cache
                .store(plugin.latency(), Ordering::Relaxed);
            inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
        }
        audio.output_note_ids.clear();
        audio.pending_output_mutation = PendingOutputMutation::None;
        audio.prepared = true;
        1
    })
}

pub(crate) unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
    process_mode: i32,
) {
    run_extern_callback_with::<P, ()>("vst3", "reset", (), || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let mut audio = inst.audio.enter();
        if !audio.prepared {
            return;
        }
        let mut plugin = enter_plugin(&inst.plugin);
        {
            // Hosts may call setProcessing(false) on the audio thread: the
            // author's `reset` must not allocate (rt-paranoid reports it).
            let _rt = RtSection::enter();
            plugin.reset(
                &AudioConfig::new(sample_rate, max_frames as usize)
                    .with_process_mode(vst3_process_mode(process_mode)),
            );
        }
        inst.latency_cache
            .store(plugin.latency(), Ordering::Relaxed);
        inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
        audio.output_note_ids.clear();
        audio.pending_output_mutation = PendingOutputMutation::None;
    });
}

/// `IAudioProcessor::setProcessing`. Tracks whether the audio thread owns
/// the plugin; stopping also resets it, as the shim did before.
pub(crate) unsafe extern "C" fn cb_set_processing<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    processing: i32,
    sample_rate: f64,
    max_frames: u32,
    process_mode: i32,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if processing != 0 {
            inst.processing.store(true, Ordering::Release);
        } else {
            cb_reset::<P>(ctx, sample_rate, max_frames, process_mode);
            inst.processing.store(false, Ordering::Release);
        }
    }
}

/// `IComponent::setActive`. Tracks activation so `cb_state_load` knows
/// whether the audio thread will drain the pending-state queue.
pub(crate) unsafe extern "C" fn cb_set_active<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    active: i32,
) {
    // The deactivation drain runs author `load_state` via `apply_state`;
    // firewall it so a panic can't unwind across the C ABI.
    run_extern_callback_with::<P, ()>("vst3", "set_active", (), || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        inst.active.store(active != 0, Ordering::Relaxed);
        inst.processing.store(false, Ordering::Release);
        if active == 0 {
            // Deactivating: the audio thread stops draining `pending_state`,
            // so a load queued (by the host or the editor) while active would
            // strand and a following `getState` would re-serialize stale extra
            // state - silently losing the load. Apply it now under the plugin
            // lock (uncontended: the host serializes lifecycle calls with
            // `process`) and refresh the snapshot.
            let mut plugin = enter_plugin(&inst.plugin);
            if apply_queued_state(&mut *plugin, &inst.pending_state).is_some() {
                plugin.republish_snapshot();
            }
        }
    });
}

/// The `bus_layouts()` index matching `(in_ch, out_ch)`, or `-1`. The
/// shim's `setBusArrangements` calls this to accept any declared layout.
/// Static per plugin type - no instance context.
pub(crate) unsafe extern "C" fn cb_match_bus_layout<P: PluginExport>(
    in_ch: u32,
    out_ch: u32,
) -> i32 {
    find_bus_layout::<P>(in_ch, out_ch).map_or(-1, |i| i32::try_from(i).unwrap_or(-1))
}

/// Per-bus channel width of a declared layout, for the shim's per-bus
/// `getBusInfo` / `getBusArrangement` and its process-time channel gather.
/// `0` for any out-of-range index.
pub(crate) unsafe extern "C" fn cb_layout_bus_channels<P: PluginExport>(
    layout_index: u32,
    is_output: i32,
    bus_index: u32,
) -> u32 {
    let (Ok(li), Ok(bi)) = (usize::try_from(layout_index), usize::try_from(bus_index)) else {
        return 0;
    };
    let layouts = P::bus_layouts();
    let Some(layout) = layouts.get(li) else {
        return 0;
    };
    let buses = if is_output != 0 {
        &layout.outputs
    } else {
        &layout.inputs
    };
    buses.get(bi).map_or(0, |b| b.channels.channel_count())
}

/// Match a host-proposed per-bus arrangement (arrays of per-bus channel
/// counts) to a declared `bus_layouts()` index, or `-1`. A layout matches
/// when its per-bus widths equal the host's, direction by direction - so
/// a sidechain bus is matched on its own width, not summed into the main.
pub(crate) unsafe extern "C" fn cb_match_bus_layout_perbus<P: PluginExport>(
    in_channels: *const u32,
    num_in: u32,
    out_channels: *const u32,
    num_out: u32,
) -> i32 {
    // SAFETY: the shim passes arrays of the lengths it declares, or null
    // with length 0 for a bus-less direction.
    let ins = unsafe { slice_or_empty(in_channels, num_in) };
    let outs = unsafe { slice_or_empty(out_channels, num_out) };
    let widths_match = |buses: &[BusConfig], want: &[u32]| {
        buses.len() == want.len()
            && buses
                .iter()
                .zip(want)
                .all(|(b, &w)| b.channels.channel_count() == w)
    };
    P::bus_layouts()
        .iter()
        .position(|l| widths_match(&l.inputs, ins) && widths_match(&l.outputs, outs))
        .map_or(-1, |i| i32::try_from(i).unwrap_or(-1))
}

/// `(num_input_buses, num_output_buses, input_kinds_ptr, output_kinds_ptr)`
/// for the descriptor, from the plugin's first declared layout. The
/// kind-byte arrays are leaked to `'static`.
pub(crate) fn descriptor_buses<P: PluginExport>() -> (u32, u32, *const u8, *const u8) {
    let first = P::bus_layouts().into_iter().next().unwrap_or_default();
    let ins = leak_bus_kinds(&first.inputs);
    let outs = leak_bus_kinds(&first.outputs);
    (
        u32::try_from(ins.len()).unwrap_or(0),
        u32::try_from(outs.len()).unwrap_or(0),
        ins.as_ptr(),
        outs.as_ptr(),
    )
}

/// Leak the per-bus kind bytes of a bus list (`0` = Main, `1` = Sidechain)
/// to `'static` for the descriptor's raw pointer.
pub(crate) fn leak_bus_kinds(buses: &[BusConfig]) -> &'static [u8] {
    Box::leak(
        buses
            .iter()
            .map(|b| u8::from(b.kind == BusKind::Sidechain))
            .collect::<Vec<u8>>()
            .into_boxed_slice(),
    )
}

/// True when every declared layout shares the first layout's bus topology:
/// the same input and output bus COUNT and the same per-bus KIND (main vs
/// sidechain) in order. VST3 fixes one bus topology per plugin - only
/// channel widths are negotiated - so [`descriptor_buses`] takes the bus
/// count + kinds from layout 0 and [`cb_match_bus_layout_perbus`] requires
/// the host arrangement to have exactly that many buses. A layout with a
/// different bus count or kind can therefore never be matched (it's dead),
/// or, if it leads the list, silently drops a bus the others declare.
pub(crate) fn vst3_topology_consistent(layouts: &[BusLayout]) -> bool {
    let kinds = |buses: &[BusConfig]| buses.iter().map(|b| b.kind).collect::<Vec<BusKind>>();
    let Some(first) = layouts.first() else {
        return true;
    };
    let (first_in, first_out) = (kinds(&first.inputs), kinds(&first.outputs));
    layouts
        .iter()
        .all(|l| kinds(&l.inputs) == first_in && kinds(&l.outputs) == first_out)
}

/// Build a slice from a `(ptr, len)` the C++ shim handed us, or an empty
/// slice when the pointer is null (a direction with no buses).
pub(crate) unsafe fn slice_or_empty<'a>(ptr: *const u32, len: u32) -> &'a [u32] {
    match usize::try_from(len) {
        Ok(n) if !ptr.is_null() && n > 0 => unsafe { std::slice::from_raw_parts(ptr, n) },
        _ => &[],
    }
}
