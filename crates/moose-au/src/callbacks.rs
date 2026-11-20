//! C callbacks the AU shim calls: lifecycle, parameters, state and process.

use crate::au_process_capacity;
use crate::gui::free;
use crate::gui::malloc;
#[cfg(target_os = "macos")]
use crate::gui::moose_au_view_detach;
use crate::instance::AuAudio;
use crate::instance::AuGui;
use crate::instance::AuInstance;
#[cfg(target_os = "macos")]
use crate::instance::ParamNotifier;
use crate::instance::SYSEX_INPUT_SLOTS;
use crate::instance::SysExInput;
use crate::instance::SysExQueue;
use crate::instance::drain_sysex_input;
use crate::output_events::NthCursor;
use crate::output_events::UmpDrainCursor;
use crate::output_events::typed_midi1_bytes;
#[cfg(target_os = "macos")]
use crate::persist_notify;
use crate::state::StateLoadQueue;

use crate::ffi::{
    AU_NATIVE_EVENT_MIDI1, AU_NATIVE_EVENT_SYSEX, AU_NATIVE_EVENT_UMP, AU_OUTPUT_END,
    AU_PROCESS_INVALID, AU_PROCESS_OK, AU_PROCESS_QUEUE_FULL, AuMidi2Event, AuMidiEvent,
    AuNativeEvent, AuParamEvent, AuTransportSnapshot,
};
use moose_core::SYSEX_POOL_PREALLOC;
use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::bus_routing::{BusActivation, BusRouting};
use moose_core::cast::{len_u32, sample_pos_i64};
use moose_core::chunked_process::{ChunkedProcess, process_chunked_with_bus_routing_and_catalog};
use moose_core::config::{AudioConfig, ProcessMode};
#[cfg(any(target_os = "macos", target_os = "ios"))]
use moose_core::editor::SendPtr;
use moose_core::events::{
    AuEventMetadata, EVENT_LIST_PREALLOC, Event, EventBody, EventList, ExactEvent, ExactEventBody,
    ExactEventMetadata, LosslessEventCursor, PushError, RawMidi1, RawUmp, TransportInfo,
};
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostQueue, HostSignal};
use moose_core::info::MidiDialect;
use moose_core::midi::decode_short_message;
use moose_core::plugin::{PreparationConfig, PreparationFrameBound};
use moose_core::rt::{RtSection, audit};
use moose_core::state;
use moose_core::ump::{
    SysExAssembler, SysExFeed, decode_ump_channel_voice_2, encode_ump_channel_voice_1,
    encode_ump_channel_voice_2,
};
use moose_core::wrapper::{
    PluginCell, apply_or_queue_state, apply_queued_state, copy_c_str, custom_state_for_save,
    enter_plugin, run_audio_block_latched, run_extern_callback_with, shared_plugin,
};
use moose_params::Params;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

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

pub(crate) unsafe extern "C" fn cb_create<P: PluginExport>() -> *mut std::ffi::c_void {
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
            let param_infos = plugin.params().param_catalog();
            let params_arc = plugin.params_arc();
            let wants_persist = params_arc.wants_persist_notifications();
            let latency_cache = AtomicU32::new(plugin.latency());
            let tail_cache = AtomicU32::new(plugin.tail());
            #[cfg(target_os = "macos")]
            let (persist_dispatch, host_queue) = if wants_persist
                && let Some((dispatch, wake)) = unsafe { persist_notify::Dispatcher::new() }
            {
                (Some(dispatch), Arc::new(HostQueue::new(wake)))
            } else {
                (None, Arc::new(HostQueue::new(|| {})))
            };
            #[cfg(not(target_os = "macos"))]
            let host_queue = Arc::new(HostQueue::new(|| {}));
            let persist_signal = wants_persist.then(|| {
                let queue = Arc::clone(&host_queue);
                let signal = Arc::new(moose_params::PersistChangeSignal::new(move || {
                    queue.signal(HostSignal::PERSIST);
                }));
                params_arc.set_persist_change_signal(Arc::downgrade(&signal));
                signal
            });
            let instance = Box::new(AuInstance::<P> {
                persist_signal,
                host_queue,
                #[cfg(target_os = "macos")]
                persist_dispatch,
                params_arc,
                meter_store: plugin.meter_store(),
                snapshot: plugin.snapshot_slot(),
                task_spawner: plugin.task_spawner(),
                editor_builder: plugin.editor_builder(),
                plugin: shared_plugin(plugin),
                latency_cache,
                tail_cache,
                render_mode: AtomicU8::new(ProcessMode::Realtime.as_u8()),
                #[cfg(target_os = "macos")]
                param_notify: None,
                param_infos,
                automation: info.automation,
                input_bus_count,
                output_bus_count,
                sidechain_channels,
                plugin_id_hash: state::shared_plugin_state_hash(&info),
                audio: PluginCell::new(AuAudio {
                    event_list: EventList::with_capacity(EVENT_LIST_PREALLOC),
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
                    native_output_param_feedback_available: false,
                    native_output_status: AU_OUTPUT_END,
                    sub_event_scratch: EventList::with_capacity(EVENT_LIST_PREALLOC),
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
                if let Some(dispatch) = &(*raw).persist_dispatch {
                    dispatch.activate::<P>(
                        Arc::clone(&(*raw).host_queue),
                        raw.cast::<std::ffi::c_void>(),
                    );
                }
                let ctx = SendPtr::new(raw.cast::<std::ffi::c_void>().cast_const());
                (*raw).param_notify = ParamNotifier::spawn(ctx);
            }
            raw.cast::<std::ffi::c_void>()
        },
    )
}

pub(crate) unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
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
            #[cfg(target_os = "macos")]
            {
                let view = std::mem::take(&mut (*raw).gui.enter().view);
                if view != 0 {
                    moose_au_view_detach(view as *mut std::ffi::c_void);
                }
            }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Some(signal) = &(*raw).persist_signal {
                    signal.close();
                }
                (*raw).host_queue.close();
                drop(Box::from_raw(raw));
            }));
        }
    }
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
        let mut audio = inst.audio.enter();
        // Clamp host-supplied max_frames to a sane minimum.
        let max_frames = (max_frames as usize).max(1024);
        let mode_raw = inst.render_mode.load(Ordering::Relaxed);
        let mode = ProcessMode::from_u8(mode_raw);
        // The shim calls this from both `AudioUnitInitialize` (main thread)
        // and `AudioUnitReset`, which hosts may send from the render thread.
        // Same rate, size and mode as the last prepare: a plain `reset`
        // (no allocation, checked under rt-paranoid). Otherwise a full
        // `prepare`, which may allocate.
        #[allow(clippy::float_cmp, reason = "same host value, not arithmetic")]
        let same_config = audio.prepared
            && audio.sample_rate == sample_rate
            && audio.max_block_size == max_frames
            && audio.prepared_mode == mode_raw;
        if same_config {
            let mut plugin = enter_plugin(&inst.plugin);
            {
                let _rt = RtSection::enter();
                plugin.reset(&AudioConfig::new(sample_rate, max_frames).with_process_mode(mode));
            }
            inst.latency_cache
                .store(plugin.latency(), Ordering::Relaxed);
            inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
            return;
        }
        audio.prepared = false;
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
            inst.latency_cache
                .store(plugin.latency(), Ordering::Relaxed);
            inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
        }
        audio.prepared_mode = mode_raw;
        audio.prepared = true;
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
            param_events,
            num_param_events,
            0,
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
            param_events,
            num_param_events,
            param_overflow,
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
            param_events,
            num_param_events,
            param_overflow,
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
    param_events: *const AuParamEvent,
    num_param_events: u32,
    param_overflow: u32,
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
        let mut audio = inst.audio.enter();
        let num_frames = nf;

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
            scr.midi_drain_cursor = NthCursor::HEAD;
            scr.sysex_drain_cursor = NthCursor::HEAD;
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
            latency_change_request: None,
            param_infos: &inst.param_infos,
            automation: inst.automation,
        };
        process_chunked_with_bus_routing_and_catalog(
            &mut *plugin,
            inst.params_arc.as_ref() as &dyn Params,
            &mut audio_buffer,
            chunk_args,
            bus_routing,
            &inst.param_infos,
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
    })
    .is_some();
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

pub(crate) unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        len_u32(inst.params_arc.count())
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
        inst.params_arc.set_plain(id, value);
    }
}

pub(crate) unsafe extern "C" fn cb_param_format_value<P: PluginExport>(
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

pub(crate) unsafe extern "C" fn cb_param_parse_value<P: PluginExport>(
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

pub(crate) unsafe extern "C" fn cb_state_save<P: PluginExport>(
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
        // Own exactly one contiguous host buffer. RAII frees failed or panicked
        // partial writes; successful transfer retains the original libc ABI.
        struct Buffer(*mut u8);
        impl Drop for Buffer {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe {
                        free(self.0.cast());
                    }
                }
            }
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        let (ids, values) = inst.params_arc.collect_values();
        // AU gives the wrapper no "render stopped" signal it can trust, so
        // the save always asks the render thread for a fresh snapshot
        // (bounded wait), falling back to the last one.
        //
        // Allocator pin: this wrapper allocates with libc `malloc` and
        // the AU shim frees with libc `free`. The Rust global allocator
        // must not appear on either side; mixing allocators is UB.
        let extra = custom_state_for_save(&inst.plugin, &inst.snapshot, &inst.pending_state, false);
        let Ok(persist) = inst.params_arc.prepare_persist_write() else {
            return;
        };
        let Some(len) = state::plugin_state_stream_size::<P>(
            &inst.params_arc,
            &ids,
            &values,
            &extra,
            persist.byte_len(),
            P::max_host_state_bytes(),
        ) else {
            return;
        };
        let Ok(host_len) = u32::try_from(len) else {
            return;
        };
        let mut buffer = Buffer(malloc(len).cast::<u8>());
        if buffer.0.is_null() {
            return;
        }
        let mut offset = 0;
        let ok = state::write_plugin_state_stream::<P>(
            inst.plugin_id_hash,
            &inst.params_arc,
            &ids,
            &values,
            &extra,
            persist.byte_len(),
            P::max_host_state_bytes(),
            |output| moose_params::write_prepared_persist(persist.as_ref(), output),
            |bytes| {
                if bytes.len() > len - offset {
                    return -1;
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.0.add(offset), bytes.len());
                offset += bytes.len();
                i64::try_from(bytes.len()).unwrap_or(-1)
            },
        );
        if !ok {
            return;
        }
        *out_data = buffer.0;
        *out_len = host_len;
        buffer.0 = std::ptr::null_mut();
    });
}

pub(crate) unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) -> i32 {
    run_extern_callback_with::<P, i32>("au", "load_state", 0, || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        // `slice::from_raw_parts(null, n)` for `n > 0` is UB. Treat
        // `(null, *)` and `(_, 0)` the same as "host gave us nothing".
        if data.is_null() || len == 0 {
            return 0;
        }
        let mut blob = slice::from_raw_parts(data, len as usize);
        let backup = state::ParamsBackup::capture(&*inst.params_arc);
        // Not this plugin's envelope? Offer the bytes to the plugin's
        // `migrate_state` hook (legacy sessions from a pre-moose
        // build that stored foreign bytes under moose's key, or a
        // renamed plugin's envelope).
        if let Some(deserialized) = state::read_and_apply_params::<P>(
            |dst| {
                std::io::Read::read(&mut blob, dst).map_or(-1, |n| i64::try_from(n).unwrap_or(-1))
            },
            inst.plugin_id_hash,
            state::PluginFormat::Au,
            Some("truce_state"),
            &*inst.params_arc,
            len as usize,
        ) {
            // Apply params synchronously on the host thread (atomic-safe)
            // so host queries that read parameter values right after
            // `setFullState:` see the restored values without first
            // running a render block.

            // Hand the deserialized state to the audio thread for
            // application. `force_push` overwrites any older pending
            // blob - see the `pending_state` field comment for why
            // newest-wins is the right policy.
            // Queue for the render thread and wait (bounded) so the editor
            // notify below sees the applied state.
            if !load_custom_state(inst, deserialized, &backup) {
                return 0;
            }
            if let Some(ref mut editor) = inst.gui.enter().editor {
                editor.state_changed();
            }
            1
        } else {
            0
        }
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
        let Some(migrated) = P::migrate_state(&state::ForeignState::Raw {
            format: state::PluginFormat::Au,
            source_key: Some(key),
            bytes,
        }) else {
            return 0;
        };
        let deserialized: state::DeserializedState = migrated.into();
        let backup = state::ParamsBackup::capture(&*inst.params_arc);
        state::apply_params(&*inst.params_arc, &deserialized);
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
    match apply_or_queue_state(
        &inst.plugin,
        &*inst.params_arc,
        &inst.snapshot,
        &inst.pending_state,
        deserialized,
        backup,
        false,
    ) {
        Ok(()) => true,
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
