//! C callbacks the AU shim calls: lifecycle, parameters, state and process.

use crate::au_process_capacity;
use crate::catalog::{self, AuExposedParams};
use crate::gui::free;
use crate::gui::malloc;
#[cfg(target_os = "macos")]
use crate::gui::moose_au_view_detach;
use crate::host_drain::{self, HostSink, RenderSignals};
use crate::instance::ActiveAuRamp;
use crate::instance::AuAudio;
use crate::instance::AuGui;
use crate::instance::AuInstance;
use crate::instance::FeedbackEpoch;
use crate::instance::LIFECYCLE_ACTIVE;
use crate::instance::LIFECYCLE_INACTIVE;
use crate::instance::ParamNotifyQueue;
use crate::instance::ParamTrackEntry;
use crate::instance::drain_sysex_input;
use crate::instance::new_param_edit_seqs;
use crate::output_events::NthCursor;
use crate::output_events::UmpDrainCursor;
use crate::output_events::typed_midi1_bytes;
use crate::state::StateLoadQueue;

use crate::ffi::{
    AU_NATIVE_EVENT_MIDI1, AU_NATIVE_EVENT_SYSEX, AU_NATIVE_EVENT_UMP, AU_OUTPUT_END,
    AU_PARAM_EVENT_EXACT_OFFSET, AU_PARAM_EVENT_HAS_START_VALUE, AU_PROCESS_INVALID, AU_PROCESS_OK,
    AU_PROCESS_QUEUE_FULL, AuMidi2Event, AuMidiEvent, AuNativeEvent, AuParamEvent, AuParamEventV2,
    AuParamInfoOut, AuTransportSnapshot, MooseAuHostSink,
};
use moose_core::SYSEX_POOL_PREALLOC;
use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::cast::{len_u32, sample_pos_i64};
use moose_core::config::{AudioConfig, ProcessMode};
use moose_core::events::{
    AuEventMetadata, EVENT_LIST_PREALLOC, Event, EventBody, EventList, ExactEvent, ExactEventBody,
    ExactEventMetadata, LosslessEventCursor, PushError, RawMidi1, RawUmp, TransportInfo,
};
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostQueue, HostSignal};
use moose_core::info::{Automation, MidiDialect};
use moose_core::main_thread::{HostNotifier, TrackInfo};
use moose_core::midi::decode_short_message;
use moose_core::plugin::{PreparationConfig, PreparationFrameBound};
use moose_core::rt::{RtSection, audit};
use moose_core::state;
use moose_core::thread::MainThreadCell;
use moose_core::ump::{
    SysExAssembler, SysExFeed, decode_ump_channel_voice_2, encode_ump_channel_voice_1,
    encode_ump_channel_voice_2,
};
use moose_core::wrapper::{
    PluginCell, apply_or_queue_state, apply_queued_state, copy_c_str, enter_plugin,
    run_audio_block_latched, run_extern_callback_with,
};
use moose_params::{CatalogSignal, Params};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::slice;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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

#[allow(clippy::too_many_lines)]
pub(crate) unsafe extern "C" fn cb_create_with_sink<P: PluginExport>(
    sink: *const MooseAuHostSink,
) -> *mut std::ffi::c_void {
    // `P::create` + author `init` + `main_thread().connect(...)` run here;
    // firewall them so a panic can't unwind across the C ABI (null = failed).
    run_extern_callback_with::<P, *mut std::ffi::c_void>(
        "au",
        "create",
        std::ptr::null_mut(),
        || {
            let host_sink = unsafe { HostSink::from_raw(sink) };
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
            let params_arc = plugin.params_arc();
            let wants_persist = params_arc.wants_persist_notifications();

            let render_signals = Arc::new(RenderSignals::default());
            let (host_dispatch, host_queue) =
                if let Some((dispatch, wake)) = unsafe { host_drain::Dispatcher::new() } {
                    let signals = Arc::clone(&render_signals);
                    let queue = HostQueue::with_deferred_wake(wake, move || signals.defer());
                    (Some(dispatch), Arc::new(queue))
                } else {
                    // Without a main-queue wake, runtime delay and persist
                    // changes could never reach the host. Fail instantiation
                    // through the existing null-context error contract.
                    return std::ptr::null_mut();
                };

            let persist_signal = wants_persist.then(|| {
                let queue = Arc::clone(&host_queue);
                let signal = Arc::new(moose_params::PersistChangeSignal::new(move || {
                    queue.signal(HostSignal::PERSIST);
                }));
                params_arc.set_persist_change_signal(Arc::downgrade(&signal));
                signal
            });

            let catalog_signal = {
                let queue = Arc::clone(&host_queue);
                let signal = Arc::new(CatalogSignal::new(move || {
                    queue.signal(HostSignal::PRESENTATION);
                }));
                params_arc.set_catalog_signal(Arc::downgrade(&signal));
                let _ = params_arc.take_catalog_changes();
                Some(signal)
            };

            let mut hooks = plugin.main_thread();
            hooks.connect(HostNotifier::from_queue(Arc::clone(&host_queue)));
            let main_thread = MainThreadCell::new(hooks);

            // Build the live exposed catalog AFTER `init` and `main_thread`
            // so instance-level defaults / dynamic slots are reflected.
            let Some(snapshot) = AuExposedParams::try_capture(params_arc.as_ref(), info.automation)
            else {
                host_queue.close();
                return std::ptr::null_mut();
            };
            let param_infos = snapshot.render_catalog();
            let exposed = Mutex::new(Arc::new(snapshot));

            let delay = moose_core::delay::ReportedDelay::new(plugin.latency(), plugin.tail());
            let (sysex_ready_q, sysex_free_q) = crate::instance::new_sysex_queues();
            let instance = Box::new(AuInstance::<P> {
                persist_signal,
                catalog_signal,
                host_queue,
                render_signals,
                pending_context_name: Mutex::new(None),
                host_dispatch,
                main_thread,
                params_arc,
                exposed,
                meter_store: plugin.meter_store(),
                snapshot: plugin.snapshot_slot(),
                task_spawner: plugin.task_spawner(),
                editor_builder: plugin.editor_builder(),
                plugin: PluginCell::new(plugin),
                delay,
                render_mode: AtomicU8::new(ProcessMode::Realtime.as_u8()),
                prepared_flag: std::sync::atomic::AtomicBool::new(false),
                prepared_rate_bits: AtomicU64::new(0),
                prepared_max_frames: std::sync::atomic::AtomicUsize::new(0),
                prepared_mode: AtomicU8::new(ProcessMode::Realtime.as_u8()),
                lifecycle_state: AtomicU8::new(LIFECYCLE_INACTIVE),
                #[cfg(any(not(target_vendor = "apple"), test))]
                owner_thread: std::thread::current().id(),
                lifecycle_lock: Mutex::new(()),
                render_catalog: crate::catalog::RenderCatalog::default(),
                global_edit_seq: AtomicU64::new(0),
                param_edit_seqs: new_param_edit_seqs(&param_infos),
                editor_gestures: Mutex::default(),
                param_out_queue: ParamNotifyQueue::new(EVENT_LIST_PREALLOC),
                automation: info.automation,
                input_bus_count,
                output_bus_count,
                sidechain_channels,
                plugin_id_hash: state::shared_plugin_state_hash(&info),
                audio: PluginCell::new(AuAudio {
                    active_ramps: Vec::with_capacity(param_infos.len().max(64)),
                    active_ramps_scratch: Vec::with_capacity(param_infos.len().max(64)),
                    param_tracker: Vec::with_capacity(512),
                    feedback_epoch: FeedbackEpoch::default(),
                    catalog: param_infos,
                    // Preallocate enough capacity for a full AUv2 shim block
                    // (256 MIDI + 512 parameter events) plus ramp subdivisions.
                    event_list: EventList::with_capacity(EVENT_LIST_PREALLOC.max(1024)),
                    output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
                    ump_drain_cursor: UmpDrainCursor::HEAD,
                    midi_drain_cursor: NthCursor::HEAD,
                    sysex_drain_cursor: NthCursor::HEAD,
                    native_output_cursor: LosslessEventCursor::default(),
                    native_output_carriers: 0,
                    native_output_num_frames: 0,
                    native_output_ump_protocol: 0,
                    native_output_max_absolute_offset: u32::MAX,
                    native_output_ump_time_valid: true,
                    native_output_status: AU_OUTPUT_END,
                    sub_event_scratch: EventList::with_capacity(EVENT_LIST_PREALLOC.max(1024)),
                    sysex_assembler: SysExAssembler::with_capacity(SYSEX_POOL_PREALLOC),
                    sample_rate: 44100.0,
                    max_block_size: 8192,
                    prepared: false,
                    prepared_mode: 0,
                    scratch: RawBufferScratch::default(),
                }),
                gui: PluginCell::new(AuGui {
                    editor: None,
                    view: 0,
                }),
                pending_resize: AtomicU64::new(0),
                transport_slot: TransportSlot::new(),
                pending_state: Arc::new(StateLoadQueue::new(1)),
                panic_latch: moose_core::wrapper::PanicLatch::new(),
                sysex_ready: sysex_ready_q,
                sysex_free: sysex_free_q,
                legacy_key_cstrings: info
                    .legacy_au_keys
                    .iter()
                    .filter_map(|k| CString::new(*k).ok())
                    .collect(),
                host_sink,
            });
            let raw = Box::into_raw(instance);
            unsafe {
                if let Some(dispatch) = &(*raw).host_dispatch {
                    dispatch.activate::<P>(
                        Arc::clone(&(*raw).host_queue),
                        raw.cast::<std::ffi::c_void>(),
                    );
                    if !(*raw).host_queue.pending().is_empty() {
                        dispatch.trigger_now();
                    }
                }
            }
            raw.cast::<std::ffi::c_void>()
        },
    )
}

pub(crate) unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        if !ctx.is_null() {
            let raw = ctx.cast::<AuInstance<P>>();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Some(signal) = &(*raw).persist_signal {
                    signal.close();
                }
                if let Some(signal) = &(*raw).catalog_signal {
                    signal.close();
                }
                // Stop any new `with_open` / wake calls immediately before
                // deciding whether to finalize now or after the active drain.
                (*raw).host_queue.begin_close();
            }));
            if crate::gui::defer_host_destroy(ctx) {
                return;
            }
            finalize_destroy::<P>(raw);
        }
    }
}

/// Complete instance teardown after any enclosing `HostQueue::with_open` drain
/// on the current thread has exited.
pub(crate) unsafe fn finalize_destroy<P: PluginExport>(raw: *mut AuInstance<P>) {
    unsafe {
        #[cfg(target_os = "macos")]
        {
            let view = std::mem::take(&mut (*raw).gui.enter().view);
            if view != 0 {
                moose_au_view_detach(view as *mut std::ffi::c_void);
            }
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (*raw).host_queue.close();
            // Dropping `AuInstance` tears down dispatch sources, editor,
            // plugin, and finally `host_sink` (invoking `release_user_data`
            // last, after every in-flight `with_open` has returned).
            drop(Box::from_raw(raw));
        }));
    }
}

pub(crate) unsafe extern "C" fn cb_set_active<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    active: u32,
) {
    run_extern_callback_with::<P, ()>("au", "set_active", (), || unsafe {
        if ctx.is_null() {
            return;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let lifecycle = inst
            .lifecycle_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if active != 0 {
            if !inst.is_active()
                && let Some(dispatch) = &inst.host_dispatch
            {
                dispatch.start_render_polling(&inst.render_signals);
            }
            inst.lifecycle_state
                .store(LIFECYCLE_ACTIVE, Ordering::Release);
            return;
        }

        inst.lifecycle_state
            .store(LIFECYCLE_INACTIVE, Ordering::Release);
        {
            let mut audio = inst.audio.enter();
            inst.prepared_flag.store(false, Ordering::Release);
            audio.prepared = false;
            audio.active_ramps.clear();
            audio.active_ramps_scratch.clear();
            inst.render_catalog.reclaim();
            inst.render_catalog.install(&mut audio.catalog);
            inst.render_catalog.reclaim();
            let mut plugin = enter_plugin(&inst.plugin);
            if apply_queued_state(&mut *plugin, &inst.pending_state).is_some() {
                plugin.republish_snapshot();
            }
        }
        if let Some(dispatch) = &inst.host_dispatch {
            dispatch.stop_render_polling();
        }
        drop(lifecycle);

        if inst.is_ui_thread() {
            let queue = Arc::clone(&inst.host_queue);
            host_drain::run_host_drain::<P>(&queue, ctx);
        } else if let Some(dispatch) = &inst.host_dispatch {
            dispatch.trigger_now();
        }
    });
}

pub(crate) unsafe extern "C" fn cb_set_context_name<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    name_utf8: *const c_char,
) {
    run_extern_callback_with::<P, ()>("au", "set_context_name", (), || unsafe {
        if ctx.is_null() {
            return;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let name = if name_utf8.is_null() {
            None
        } else {
            CStr::from_ptr(name_utf8)
                .to_str()
                .ok()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let mut info = TrackInfo::new();
        info.name = name;
        *inst
            .pending_context_name
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(info);
        if let Some(dispatch) = &inst.host_dispatch {
            dispatch.trigger_now();
        }
    });
}

pub(crate) unsafe extern "C" fn cb_param_ids<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_ids: *mut u32,
    cap: u32,
) -> u32 {
    run_extern_callback_with::<P, u32>("au", "param_ids", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        catalog::control_snapshot(ctx, &inst.exposed).write_ids(out_ids, cap)
    })
}

pub(crate) unsafe extern "C" fn cb_param_info_by_id<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    out: *mut AuParamInfoOut,
) -> i32 {
    run_extern_callback_with::<P, i32>("au", "param_info_by_id", 0, || unsafe {
        if ctx.is_null() || out.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let mut local = AuParamInfoOut::default();
        let found = catalog::control_snapshot(ctx, &inst.exposed).fill_info(id, &mut local);
        if found {
            out.write(local);
        }
        i32::from(found)
    })
}

pub(crate) unsafe extern "C" fn cb_param_clump_name<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    clump_id: u32,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    run_extern_callback_with::<P, u32>("au", "param_clump_name", 0, || unsafe {
        if ctx.is_null() || out.is_null() || out_len == 0 {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        catalog::control_snapshot(ctx, &inst.exposed).write_clump_name(clump_id, out, out_len)
    })
}

pub(crate) unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    // Author `reset` runs here; firewall it so a panic can't unwind across
    // the C ABI.
    run_extern_callback_with::<P, ()>("au", "reset", (), || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        // Clamp host-supplied max_frames to a sane minimum.
        let max_frames = (max_frames as usize).max(1024);
        let mode_raw = inst.render_mode.load(Ordering::Relaxed);
        let mode = ProcessMode::from_u8(mode_raw);

        // The shim calls this from both `AudioUnitInitialize` (main thread)
        // and `AudioUnitReset`, which hosts may send from the render thread.
        // Detect a same-config reset atomically before entering any cell so a
        // render-thread `AudioUnitReset` never blocks even under host overlap.
        let likely_same_config = inst.prepared_flag.load(Ordering::Acquire)
            && inst.prepared_rate_bits.load(Ordering::Relaxed) == sample_rate.to_bits()
            && inst.prepared_max_frames.load(Ordering::Relaxed) == max_frames
            && inst.prepared_mode.load(Ordering::Relaxed) == mode_raw;
        let mut audio = if likely_same_config {
            let Some(guard) = inst.audio.try_enter() else {
                return;
            };
            guard
        } else {
            inst.audio.enter()
        };

        audio.active_ramps.clear();
        audio.active_ramps_scratch.clear();

        #[allow(clippy::float_cmp, reason = "same host value, not arithmetic")]
        let same_config = audio.prepared
            && audio.sample_rate == sample_rate
            && audio.max_block_size == max_frames
            && audio.prepared_mode == mode_raw;
        if same_config {
            let Some(mut plugin) = inst.plugin.try_enter() else {
                return;
            };
            {
                let _render_signals = inst.render_signals.enter();
                let _rt = RtSection::enter();
                plugin.reset(&AudioConfig::new(sample_rate, max_frames).with_process_mode(mode));
            }
            // A plain reset (maybe on the render thread) keeps the reported
            // latency and tail; a change shows up at the next block.
            return;
        }
        inst.prepared_flag.store(false, Ordering::Release);
        audio.prepared = false;
        inst.render_catalog.reclaim();
        inst.render_catalog.install(&mut audio.catalog);
        inst.render_catalog.reclaim();
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
        {
            let mut plugin = enter_plugin(&inst.plugin);
            let config = PreparationConfig::new(
                AudioConfig::new(sample_rate, max_frames).with_process_mode(mode),
                PreparationFrameBound::NegotiatedHost,
            );
            if plugin.prepare(&config).is_err() {
                // Stays unprepared: `cb_process` outputs silence.
                return;
            }
            inst.panic_latch.clear();
            // The activation: the only place the reported values change.
            inst.delay.publish(plugin.latency(), plugin.tail());
        }
        audio.prepared_mode = mode_raw;
        audio.prepared = true;
        inst.prepared_rate_bits
            .store(sample_rate.to_bits(), Ordering::Relaxed);
        inst.prepared_max_frames
            .store(max_frames, Ordering::Relaxed);
        inst.prepared_mode.store(mode_raw, Ordering::Relaxed);
        inst.prepared_flag.store(true, Ordering::Release);
    });
}

/// Host toggled AU offline rendering. `mode` is a [`ProcessMode`]
/// discriminant; stash it so `cb_reset` (buffer / quality prep) and
/// every `cb_process` block pick it up. Unknown values fold to
/// `Realtime` via [`ProcessMode::from_u8`].
pub(crate) unsafe extern "C" fn cb_set_render_mode<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    mode: u32,
) {
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
pub(crate) enum AuProcessInput {
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

pub(crate) fn push_exact_with_fallback(
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

pub(crate) fn ump_word_count_for_type(word0: u32) -> u8 {
    match ((word0 >> 28) & 0x0f) as u8 {
        0x0 | 0x1 | 0x2 | 0x6 | 0x7 => 1,
        0x3 | 0x4 | 0x8 | 0x9 | 0xA => 2,
        0xB | 0xC => 3,
        _ => 4,
    }
}

pub(crate) fn ump_protocol_accepts(protocol: u8, message_type: u8) -> bool {
    match message_type {
        0x2 => protocol == 1,
        0x4 => protocol == 2,
        _ => matches!(protocol, 1 | 2),
    }
}

pub(crate) fn declared_input_protocol<P: PluginExport>() -> u8 {
    if P::info().midi_input_dialect == MidiDialect::Midi2 {
        2
    } else {
        1
    }
}

pub(crate) fn set_midi1_group(body: &mut EventBody, group: u8) {
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

pub(crate) fn declared_input_port<P: PluginExport>(port: u16) -> Option<u8> {
    (port < u16::from(P::info().midi_input_ports))
        .then(|| u8::try_from(port).ok())
        .flatten()
}

#[allow(clippy::too_many_lines)]
pub(crate) fn push_native_input<P: PluginExport>(
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

#[derive(Clone, Copy)]
pub(crate) enum AuParamInput {
    V1 {
        events: *const AuParamEvent,
        len: u32,
        overflow: u32,
    },
    V2 {
        events: *const AuParamEventV2,
        len: u32,
        overflow: u32,
    },
}

#[allow(clippy::too_many_arguments)]
fn emit_ramp_segment(
    events: &mut EventList,
    automation: Automation,
    param_id: u32,
    clipped_start: u32,
    clipped_end: u32,
    seg_start_val: f64,
    seg_end_val: f64,
    remaining_inputs: u32,
    overridden_at_end: bool,
) {
    let seg_len = clipped_end - clipped_start;
    if seg_len == 0 {
        return;
    }
    match automation {
        Automation::PerSample => {
            events.push(Event {
                sample_offset: clipped_start,
                port: 0,
                body: EventBody::ParamRamp {
                    id: param_id,
                    start: seg_start_val,
                    end: seg_end_val,
                    len: seg_len,
                },
            });
        }
        Automation::BlockSplit { min } => {
            let base_stride = min.max(32);
            let free_slots = len_u32(events.capacity().saturating_sub(events.len()));
            let terminal_slots = u32::from(!overridden_at_end);
            let max_pts = free_slots
                .saturating_sub(
                    remaining_inputs
                        .saturating_sub(1)
                        .saturating_add(terminal_slots),
                )
                .clamp(u32::from(overridden_at_end), 16);
            let stride = base_stride.max(seg_len.div_ceil(max_pts.max(1)));
            let mut offset = clipped_start;
            while max_pts != 0 && offset < clipped_end {
                let frac = f64::from(offset - clipped_start) / f64::from(seg_len);
                events.push(Event::new(
                    offset,
                    EventBody::ParamChange {
                        id: param_id,
                        value: seg_start_val + (seg_end_val - seg_start_val) * frac,
                    },
                ));
                offset = offset.saturating_add(stride);
            }
            if !overridden_at_end {
                events.push(Event::new(
                    clipped_end,
                    EventBody::ParamChange {
                        id: param_id,
                        value: seg_end_val,
                    },
                ));
            }
        }
    }
}

/// Convert unified v13 step/ramp parameter events into `EventList` entries
/// according to `automation`, continuing retained multi-slice `AUv3` ramps when
/// hosts deliver an `AUParameterEvent` only in its starting slice.
/// Allocation-free (audio-thread safe; uses preallocated `active_ramps` and
/// `tracker` capacities).
#[allow(clippy::too_many_lines)]
pub(crate) fn push_param_events_v2(
    events: &mut EventList,
    params: &dyn Params,
    automation: Automation,
    input: &[AuParamEventV2],
    num_frames: u32,
    active_ramps: &mut Vec<ActiveAuRamp>,
    tracker: &mut Vec<ParamTrackEntry>,
) -> (u32, bool) {
    let nf = i64::from(num_frames);
    let mut presentation_affected = false;

    // Validate all input events upfront before mutating `active_ramps` or
    // pushing into `events` so an invalid slice fails atomically.
    for ev in input {
        let has_start = ev.flags & AU_PARAM_EVENT_HAS_START_VALUE != 0;
        if !ev.end_value.is_finite() || (has_start && !ev.start_value.is_finite()) {
            return (AU_PROCESS_INVALID, presentation_affected);
        }
        if ev.duration_frames == 0 && (ev.start_offset < 0 || i64::from(ev.start_offset) >= nf) {
            return (AU_PROCESS_INVALID, presentation_affected);
        }
    }

    tracker.clear();

    let value_at = |tracker: &[ParamTrackEntry], id: u32, query_offset: u32| -> f64 {
        for entry in tracker.iter().rev() {
            if entry.id == id {
                if query_offset >= entry.end_offset || entry.end_offset <= entry.start_offset {
                    return entry.end_val;
                }
                if query_offset <= entry.start_offset {
                    return entry.start_val;
                }
                let frac = f64::from(query_offset - entry.start_offset)
                    / f64::from(entry.end_offset - entry.start_offset);
                return entry.start_val + (entry.end_val - entry.start_val) * frac;
            }
        }
        params.get_plain(id).unwrap_or(0.0)
    };
    let record_track = |tracker: &mut Vec<ParamTrackEntry>, entry: ParamTrackEntry| {
        for slot in tracker.iter_mut() {
            if slot.id == entry.id {
                *slot = entry;
                return;
            }
        }
        if tracker.len() < tracker.capacity() {
            tracker.push(entry);
        }
    };
    let remove_active_ramp = |ramps: &mut Vec<ActiveAuRamp>, param_id: u32| {
        ramps.retain(|r| r.param_id != param_id);
    };
    let upsert_active_ramp = |ramps: &mut Vec<ActiveAuRamp>, ramp: ActiveAuRamp| -> bool {
        for slot in ramps.iter_mut() {
            if slot.param_id == ramp.param_id {
                *slot = ramp;
                return true;
            }
        }
        if ramps.len() < ramps.capacity() {
            ramps.push(ramp);
            true
        } else {
            false
        }
    };

    // Step A: continue any retained multi-slice AUv3 ramps into this slice.
    let mut keep = 0usize;
    let total_active = active_ramps.len();
    for i in 0..total_active {
        let mut ramp = active_ramps[i];
        if ramp.remaining_frames == 0 {
            continue;
        }
        // If the host explicitly delivered an event for this parameter at or
        // before frame 0 (either re-delivering the ramp or replacing it),
        // defer to `input` instead of double-emitting.
        if let Some(event) = input
            .iter()
            .find(|event| event.param_id == ramp.param_id && event.start_offset <= 0)
        {
            let repeats_ramp = event.duration_frames != 0
                && event.flags & AU_PARAM_EVENT_HAS_START_VALUE == 0
                && i64::from(event.start_offset) + i64::from(event.duration_frames)
                    == i64::from(ramp.remaining_frames)
                && f64::from(event.end_value).to_bits() == ramp.target_val.to_bits();
            if repeats_ramp || event.start_offset == 0 {
                record_track(
                    tracker,
                    ParamTrackEntry {
                        id: ramp.param_id,
                        start_offset: 0,
                        end_offset: 0,
                        start_val: ramp.current_val,
                        end_val: ramp.current_val,
                    },
                );
            }
            continue;
        }
        if params.parameter_affects_presentation(ramp.param_id) {
            presentation_affected = true;
        }
        let stop_at = input
            .iter()
            .filter(|ev| {
                ev.param_id == ramp.param_id
                    && ev.start_offset > 0
                    && i64::from(ev.start_offset) < nf
            })
            .map(|ev| {
                #[allow(clippy::cast_sign_loss)]
                {
                    ev.start_offset as u32
                }
            })
            .min()
            .unwrap_or(num_frames);
        let seg_end = ramp.remaining_frames.min(stop_at);
        if seg_end > 0 {
            let frac = (f64::from(seg_end) / f64::from(ramp.remaining_frames)).clamp(0.0, 1.0);
            let seg_start_val = ramp.current_val;
            let seg_end_val = seg_start_val + (ramp.target_val - seg_start_val) * frac;
            record_track(
                tracker,
                ParamTrackEntry {
                    id: ramp.param_id,
                    start_offset: 0,
                    end_offset: seg_end,
                    start_val: seg_start_val,
                    end_val: seg_end_val,
                },
            );
            // The emitter subtracts the segment being emitted from this count.
            let remaining_budget = len_u32(total_active - i + input.len());
            emit_ramp_segment(
                events,
                automation,
                ramp.param_id,
                0,
                seg_end,
                seg_start_val,
                seg_end_val,
                remaining_budget,
                stop_at != num_frames && seg_end == stop_at,
            );
            ramp.current_val = seg_end_val;
            ramp.remaining_frames -= seg_end;
        }
        if stop_at == num_frames && ramp.remaining_frames > 0 {
            active_ramps[keep] = ramp;
            keep += 1;
        }
    }
    active_ramps.truncate(keep);

    // Step B: process explicit parameter events delivered for this slice.
    for (idx, ev) in input.iter().enumerate() {
        let has_start = ev.flags & AU_PARAM_EVENT_HAS_START_VALUE != 0;
        if params.parameter_affects_presentation(ev.param_id) {
            presentation_affected = true;
        }

        if ev.duration_frames == 0 {
            #[allow(clippy::cast_sign_loss)]
            let offset = ev.start_offset as u32;
            let value = f64::from(ev.end_value);
            events.push(Event {
                sample_offset: offset,
                port: 0,
                body: EventBody::ParamChange {
                    id: ev.param_id,
                    value,
                },
            });
            remove_active_ramp(active_ramps, ev.param_id);
            record_track(
                tracker,
                ParamTrackEntry {
                    id: ev.param_id,
                    start_offset: offset,
                    end_offset: offset,
                    start_val: value,
                    end_val: value,
                },
            );
            continue;
        }

        let start = i64::from(ev.start_offset);
        let dur = i64::from(ev.duration_frames);
        let end = start + dur;

        if end <= 0 {
            // Ramp already completed before this block: snap to target at 0.
            let value = f64::from(ev.end_value);
            events.push(Event {
                sample_offset: 0,
                port: 0,
                body: EventBody::ParamChange {
                    id: ev.param_id,
                    value,
                },
            });
            remove_active_ramp(active_ramps, ev.param_id);
            record_track(
                tracker,
                ParamTrackEntry {
                    id: ev.param_id,
                    start_offset: 0,
                    end_offset: 0,
                    start_val: value,
                    end_val: value,
                },
            );
            continue;
        }
        if start >= nf {
            // Ramp begins in a future block; host delivers it there.
            continue;
        }

        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let clipped_start = start.max(0) as u32;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let clipped_end = end.min(nf) as u32;

        // End this ramp cleanly if a later event in the same slice replaces
        // the same parameter before `clipped_end`.
        let next_override = input[idx + 1..]
            .iter()
            .filter(|later| {
                later.param_id == ev.param_id
                    && i64::from(later.start_offset) >= start
                    && i64::from(later.start_offset) < nf
                    && i64::from(later.start_offset) <= i64::from(clipped_end)
            })
            .map(|later| {
                #[allow(clippy::cast_sign_loss)]
                {
                    later.start_offset.max(0) as u32
                }
            })
            .min();
        let effective_end = next_override.unwrap_or(clipped_end);
        let seg_len = effective_end.saturating_sub(clipped_start);
        if seg_len == 0 {
            remove_active_ramp(active_ramps, ev.param_id);
            continue;
        }

        let (seg_start_val, seg_end_val) = if has_start {
            let s = f64::from(ev.start_value);
            let e = f64::from(ev.end_value);
            #[allow(clippy::cast_precision_loss)]
            let d = dur as f64;
            #[allow(clippy::cast_precision_loss)]
            let t0 = (i64::from(clipped_start) - start) as f64 / d;
            #[allow(clippy::cast_precision_loss)]
            let t1 = (i64::from(effective_end) - start) as f64 / d;
            (s + (e - s) * t0, s + (e - s) * t1)
        } else {
            // AUv3 carries target + duration only: derive slope linearly from
            // the parameter's exact interpolated value at `clipped_start`.
            let cur = value_at(tracker, ev.param_id, clipped_start);
            #[allow(clippy::cast_precision_loss)]
            let remaining = (end - i64::from(clipped_start)) as f64;
            let frac = (f64::from(seg_len) / remaining).clamp(0.0, 1.0);
            (cur, cur + (f64::from(ev.end_value) - cur) * frac)
        };
        record_track(
            tracker,
            ParamTrackEntry {
                id: ev.param_id,
                start_offset: clipped_start,
                end_offset: effective_end,
                start_val: seg_start_val,
                end_val: seg_end_val,
            },
        );

        // Retain AUv3 ramps that extend past this slice (and are not
        // superseded within this slice) so the next slice finishes the tail
        // even if the host does not re-deliver the event. Never drop a
        // continuation silently: report `AU_PROCESS_QUEUE_FULL` on overflow.
        if !has_start && end > nf && next_override.is_none() {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let remaining_frames = (end - nf).min(i64::from(u32::MAX)) as u32;
            if !upsert_active_ramp(
                active_ramps,
                ActiveAuRamp {
                    param_id: ev.param_id,
                    current_val: seg_end_val,
                    target_val: f64::from(ev.end_value),
                    remaining_frames,
                },
            ) {
                return (AU_PROCESS_QUEUE_FULL, presentation_affected);
            }
        } else {
            remove_active_ramp(active_ramps, ev.param_id);
        }

        emit_ramp_segment(
            events,
            automation,
            ev.param_id,
            clipped_start,
            effective_end,
            seg_start_val,
            seg_end_val,
            len_u32(input.len() - idx),
            next_override.is_some(),
        );
    }

    (AU_PROCESS_OK, presentation_affected)
}

pub(crate) unsafe extern "C" fn cb_process<P: PluginExport>(
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
            AuParamInput::V1 {
                events: param_events,
                len: num_param_events,
                overflow: 0,
            },
            None,
            transport_ptr,
        );
    }
}

pub(crate) unsafe extern "C" fn cb_process_native<P: PluginExport>(
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
            AuParamInput::V1 {
                events: param_events,
                len: num_param_events,
                overflow: param_overflow,
            },
            None,
            transport_ptr,
        )
    }
}

pub(crate) unsafe extern "C" fn cb_process_native_v11<P: PluginExport>(
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
            AuParamInput::V1 {
                events: param_events,
                len: num_param_events,
                overflow: param_overflow,
            },
            Some((input_bus_active, output_bus_active)),
            transport_ptr,
        )
    }
}

pub(crate) unsafe extern "C" fn cb_process_native_v13<P: PluginExport>(
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
    param_events: *const AuParamEventV2,
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
            AuParamInput::V2 {
                events: param_events,
                len: num_param_events,
                overflow: param_overflow,
            },
            Some((input_bus_active, output_bus_active)),
            transport_ptr,
        )
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) unsafe fn cb_process_impl<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_input_channels: u32,
    num_output_channels: u32,
    num_frames: u32,
    input: AuProcessInput,
    param_input: AuParamInput,
    bus_active: Option<(u32, u32)>,
    transport_ptr: *const AuTransportSnapshot,
) -> u32 {
    let nf = num_frames as usize;
    let mut process_status = AU_PROCESS_OK;
    // SAFETY: `ctx` is this wrapper's live instance.
    let latch = unsafe { &(*ctx.cast::<AuInstance<P>>()).panic_latch };
    let ok = run_audio_block_latched::<P, ()>(latch, "AU", || unsafe {
        // Shared `&AuInstance` (never a whole-struct `&mut`) - the audio
        // scratch is reached through its ownership cell, so a concurrent
        // host-thread `&*ctx` (param reads, GUI) can't alias us.
        let inst = &*ctx.cast::<AuInstance<P>>();
        let num_frames = nf;

        // Strictly wait-free audio entry: if a misbehaving host overlaps a
        // main-thread lifecycle call with render, zero outputs and return
        // `AU_PROCESS_INVALID` so the shim skips output-event draining (which
        // would otherwise re-emit the previous block's uncleared MIDI/SysEx).
        let Some(mut audio) = inst.audio.try_enter() else {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, num_frames);
                }
            }
            process_status = AU_PROCESS_INVALID;
            return;
        };

        let _render_signals = inst.render_signals.enter();

        // Host called render before AU initialized us - sample rate
        // and smoothers haven't been primed. Zero outputs and bail. Same
        // after a panic until the next prepare (plugin state unknown).
        if !audio.prepared || inst.panic_latch.is_failed() {
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
            audio.midi_drain_cursor = NthCursor::HEAD;
            audio.sysex_drain_cursor = NthCursor::HEAD;
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
        audio.midi_drain_cursor = NthCursor::HEAD;
        audio.sysex_drain_cursor = NthCursor::HEAD;

        // Strictly wait-free plugin entry: an uncontended `Acquire`, never
        // parking the audio thread. If a misbehaving host overlaps a
        // main-thread callback with render, zero outputs and bail cleanly.
        let Some(mut plugin) = inst.plugin.try_enter() else {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, num_frames);
                }
            }
            return;
        };

        // Apply any pending state-load before per-block work so the
        // plugin sees consistent params and extra state for the
        // entire block. See `pending_state` field comment for the
        // queue-overflow policy.
        apply_queued_state(&mut *plugin, &inst.pending_state);

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
        if inst.render_catalog.install(&mut scr.catalog) {
            inst.render_signals.mark();
        }

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

        // Ramp staging can read the current parameter for its starting value.
        // Capture the async edit cutoff before staging makes any such read.
        scr.feedback_epoch.edits = inst.current_edit_seq();
        let mut presentation_affected = false;
        let mut staged_v2_ramps = false;
        let mut exact_ramp_boundaries = false;
        let mut exact_step_boundaries = false;
        match param_input {
            AuParamInput::V1 {
                events: param_events,
                len: num_param_events,
                overflow: param_overflow,
            } => {
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
                        if inst.params_arc.parameter_affects_presentation(pe.param_id) {
                            presentation_affected = true;
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
            }
            AuParamInput::V2 {
                events: param_events,
                len: num_param_events,
                overflow: param_overflow,
            } => {
                if param_overflow != 0 {
                    process_status = AU_PROCESS_QUEUE_FULL;
                    return;
                }
                if param_events.is_null() && num_param_events != 0 {
                    process_status = AU_PROCESS_INVALID;
                    return;
                }
                if num_param_events > 0 || !scr.active_ramps.is_empty() {
                    let pe_slice = if num_param_events > 0 {
                        slice::from_raw_parts(param_events, num_param_events as usize)
                    } else {
                        &[]
                    };
                    exact_ramp_boundaries = !scr.active_ramps.is_empty()
                        || pe_slice.iter().any(|event| event.duration_frames != 0);
                    exact_step_boundaries = pe_slice.iter().any(|event| {
                        event.start_offset > 0 && event.flags & AU_PARAM_EVENT_EXACT_OFFSET != 0
                    });
                    // Stage ramp mutations into `active_ramps_scratch` and commit
                    // only after input staging succeeds so a rejected slice never
                    // loses in-flight multi-slice AUv3 ramps.
                    scr.active_ramps_scratch.clear();
                    scr.active_ramps_scratch
                        .extend_from_slice(&scr.active_ramps);
                    let (status, affected) = push_param_events_v2(
                        &mut scr.event_list,
                        inst.params_arc.as_ref(),
                        inst.automation,
                        pe_slice,
                        len_u32(num_frames),
                        &mut scr.active_ramps_scratch,
                        &mut scr.param_tracker,
                    );
                    presentation_affected |= affected;
                    if status != AU_PROCESS_OK {
                        process_status = status;
                        return;
                    }
                    staged_v2_ramps = true;
                }
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
            scr.midi_drain_cursor = NthCursor::HEAD;
            scr.sysex_drain_cursor = NthCursor::HEAD;
            return;
        }

        // Input slice is accepted: commit staged multi-slice ramp state and
        // advance per-parameter edit sequences for explicit host param events
        // so stale queued feedback from earlier blocks cannot undo them.
        if staged_v2_ramps {
            std::mem::swap(&mut scr.active_ramps, &mut scr.active_ramps_scratch);
        }
        match param_input {
            AuParamInput::V1 { events, len, .. } if !events.is_null() && len > 0 => {
                for pe in slice::from_raw_parts(events, len as usize) {
                    inst.accept_automation(pe.param_id);
                }
            }
            AuParamInput::V2 { events, len, .. } if !events.is_null() && len > 0 => {
                let nf = i64::from(len_u32(num_frames));
                for pe in slice::from_raw_parts(events, len as usize) {
                    if i64::from(pe.start_offset) < nf {
                        inst.accept_automation(pe.param_id);
                    }
                }
            }
            _ => {}
        }
        scr.feedback_epoch.automation = inst.current_edit_seq();
        let bus_routing = moose_core::c_abi_driver::build_single_sidechain_bus_routing(
            inst.input_bus_count,
            inst.output_bus_count,
            num_input_channels,
            num_output_channels,
            inst.sidechain_channels,
            bus_active,
        );

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

        moose_core::c_abi_driver::run_c_abi_audio_block(
            &mut moose_core::c_abi_driver::CAbiAudioBlock {
                plugin: &mut *plugin,
                params: inst.params_arc.as_ref(),
                scratch: &mut scr.scratch,
                inputs,
                outputs,
                num_input_channels,
                num_output_channels,
                num_frames: len_u32(num_frames),
                event_list: &scr.event_list,
                sub_event_scratch: &mut scr.sub_event_scratch,
                output_events: &mut scr.output_events,
                transport,
                sample_rate: scr.sample_rate,
                process_mode: moose_core::c_abi_driver::load_render_mode(&inst.render_mode),
                catalog: &scr.catalog,
                automation: if (exact_ramp_boundaries || exact_step_boundaries)
                    && matches!(inst.automation, Automation::BlockSplit { .. })
                {
                    // Preserve scheduled AUv2 steps and the boundaries emitted
                    // by the ramp generator, which spaces intermediate points itself.
                    Automation::BlockSplit { min: 1 }
                } else {
                    inst.automation
                },
                bus_routing,
            },
        );
        if exact_ramp_boundaries && matches!(inst.automation, Automation::BlockSplit { .. }) {
            for event in scr
                .event_list
                .iter()
                .filter(|event| event.sample_offset == len_u32(num_frames))
            {
                if let EventBody::ParamChange { id, value } = event.body {
                    inst.params_arc.set_plain(id, value);
                }
            }
        }
        if inst
            .delay
            .observe_and_signal_no_wake(plugin.latency(), plugin.tail(), &inst.host_queue)
        {
            inst.render_signals.mark();
        }
        if presentation_affected {
            inst.host_queue.signal_no_wake(HostSignal::PRESENTATION);
            inst.render_signals.mark();
        }
    })
    .is_some();
    if !ok {
        process_status = AU_PROCESS_INVALID;
    }
    if !ok || process_status != AU_PROCESS_OK {
        unsafe {
            moose_core::c_abi_driver::zero_raw_outputs(outputs, num_output_channels, nf);
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
        let ctx = cb_create_with_sink::<P>(std::ptr::null());
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

pub(crate) unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        len_u32(catalog::control_snapshot(ctx, &inst.exposed).len())
    }
}

pub(crate) unsafe extern "C" fn cb_param_get_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        inst.params_arc.get_plain(id).unwrap_or(0.0)
    }
}

pub(crate) unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        // Process feedback is already committed. Replaying its host echo here
        // could overwrite a host edit after the drain checked its sequence.
        if !crate::gui::take_feedback_echo(ctx, id, value) {
            inst.edit_param(id, || inst.params_arc.set_plain(id, value));
        }
        if inst.params_arc.parameter_affects_presentation(id) {
            inst.host_queue.signal(HostSignal::PRESENTATION);
        }
    }
}

pub(crate) unsafe extern "C" fn cb_param_format_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    run_extern_callback_with::<P, u32>("au", "format_value", 0, || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        if let Some(snapshot) = catalog::scoped_snapshot(ctx, &inst.exposed)
            && let Some(label) = snapshot.choice_label(id, value)
        {
            if out.is_null() || out_len == 0 {
                return 0;
            }
            return len_u32(copy_c_str(out, out_len as usize, label));
        }
        moose_core::c_abi_driver::format_param_to_c_buf(
            inst.params_arc.as_ref(),
            id,
            value,
            out,
            out_len,
        )
    })
}

pub(crate) unsafe extern "C" fn cb_param_parse_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    text: *const c_char,
    out_plain: *mut f64,
) -> i32 {
    run_extern_callback_with::<P, i32>("au", "parse_value", 0, || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        moose_core::c_abi_driver::parse_param_from_c_str(
            inst.params_arc.as_ref(),
            id,
            text,
            out_plain,
        )
    })
}

pub(crate) unsafe extern "C" fn cb_state_save<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
    out_len: *mut u32,
) {
    unsafe {
        *out_data = std::ptr::null_mut();
        *out_len = 0;
    }
    run_extern_callback_with::<P, ()>("au", "save_state", (), || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        if let Some((ptr, len)) = moose_core::c_abi_driver::save_state_to_host_buf(
            &inst.plugin,
            &inst.params_arc,
            &inst.snapshot,
            &inst.pending_state,
            inst.plugin_id_hash,
            false,
            malloc,
            free,
        ) {
            *out_data = ptr;
            *out_len = len;
        }
    });
}

pub(crate) unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) -> i32 {
    run_extern_callback_with::<P, i32>("au", "load_state", 0, || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        if data.is_null() || len == 0 {
            return 0;
        }
        let blob = slice::from_raw_parts(data, len as usize);
        let Some((deserialized, backup)) = moose_core::c_abi_driver::load_state_from_host_slice::<P>(
            blob,
            inst.plugin_id_hash,
            state::PluginFormat::Au,
            Some("truce_state"),
            &inst.params_arc,
        ) else {
            return 0;
        };
        if !load_custom_state(inst, deserialized, &backup) {
            return 0;
        }
        if let Some(ref mut editor) = inst.gui.enter().editor {
            editor.state_changed();
        }
        1
    })
}

pub(crate) unsafe extern "C" fn cb_legacy_state_key_count<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        len_u32(inst.legacy_key_cstrings.len())
    }
}

pub(crate) unsafe extern "C" fn cb_legacy_state_key_at<P: PluginExport>(
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
pub(crate) unsafe extern "C" fn cb_state_load_foreign<P: PluginExport>(
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
        let Some((deserialized, backup)) =
            moose_core::c_abi_driver::load_foreign_state_from_host_slice::<P>(
                state::PluginFormat::Au,
                Some(key),
                bytes,
                &inst.params_arc,
            )
        else {
            return 0;
        };
        if !load_custom_state(inst, deserialized, &backup) {
            return 0;
        }
        if let Some(ref mut editor) = inst.gui.enter().editor {
            editor.state_changed();
        }
        1
    })
}

/// Custom part of an AU state load, through the render-thread queue (AU has
/// no trusted stopped signal). `false`: the plugin's `load_state` rejected
/// it and params and `#[persist]` were put back; report the error.
pub(crate) fn load_custom_state<P: PluginExport>(
    inst: &AuInstance<P>,
    deserialized: state::DeserializedState,
    backup: &state::ParamsBackup,
) -> bool {
    // Fast-path only when the adapter explicitly confirmed this instance is
    // inactive (v13 `create_with_sink` / `cb_set_active(0)`). Legacy
    // `cb_create` instances stay `LIFECYCLE_UNKNOWN` and always queue.
    if inst.is_known_inactive() {
        let _lifecycle = inst
            .lifecycle_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if inst.is_known_inactive() {
            return match apply_or_queue_state(
                &inst.plugin,
                &*inst.params_arc,
                &inst.snapshot,
                &inst.pending_state,
                deserialized,
                backup,
                true,
            ) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("[moose AU] state load rejected by load_state: {e}");
                    false
                }
            };
        }
    }

    match apply_or_queue_state(
        &inst.plugin,
        &*inst.params_arc,
        &inst.snapshot,
        &inst.pending_state,
        deserialized,
        backup,
        false,
    ) {
        Ok(()) => {
            // If `cb_set_active(0)` raced while we were queuing or waiting,
            // drain any stranded `pending_state` now under `lifecycle_lock`.
            if inst.is_known_inactive() {
                let _lifecycle = inst
                    .lifecycle_lock
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if inst.is_known_inactive() {
                    let mut plugin = enter_plugin(&inst.plugin);
                    match apply_queued_state(&mut *plugin, &inst.pending_state) {
                        Some(Ok(())) => plugin.republish_snapshot(),
                        Some(Err(e)) => {
                            eprintln!("[moose AU] state load rejected by load_state: {e}");
                            return false;
                        }
                        None => {}
                    }
                }
            }
            true
        }
        Err(e) => {
            eprintln!("[moose AU] state load rejected by load_state: {e}");
            false
        }
    }
}

pub(crate) unsafe extern "C" fn cb_state_free(data: *mut u8, _len: u32) {
    unsafe {
        if !data.is_null() {
            free(data.cast::<std::ffi::c_void>());
        }
    }
}
