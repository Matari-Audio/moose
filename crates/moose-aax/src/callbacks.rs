//! C callback implementations backing [`crate::ffi::MooseAaxCallbacks`].
//!
//! Process execution, state streaming, parameter text formatting/parsing, and
//! single-sidechain bus routing delegate to [`moose_core::c_abi_driver`] so
//! AAX shares the exact same core driver behavior as AU.

use std::ffi::c_void;
use std::num::NonZero;
use std::os::raw::c_char;
use std::slice;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moose_core::TransportSlot;
use moose_core::bus::{BusKind, BusLayout};
use moose_core::c_abi_driver::{
    CAbiAudioBlock, GestureQueue, build_single_sidechain_bus_routing, format_param_to_c_buf,
    load_foreign_state_from_host_slice, load_render_mode, load_state_from_host_slice,
    parse_param_from_c_str, run_c_abi_audio_block, save_state_to_host_buf, zero_raw_outputs,
};
use moose_core::cast::sample_pos_i64;
use moose_core::config::{AudioConfig, ProcessMode};
use moose_core::delay::ReportedDelay;
use moose_core::editor::RawWindowHandle;
use moose_core::events::{
    Event, EventBody, ExactEvent, ExactEventBody, LosslessEventCursor, LosslessEventRef, RawMidi1,
    TransportInfo,
};
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostQueue, HostSignal};
use moose_core::main_thread::HostNotifier;
use moose_core::midi::{decode_short_message, event_to_midi1};
use moose_core::plugin::{PreparationConfig, PreparationFrameBound};
use moose_core::rt::RtSection;
use moose_core::state::{self, PluginFormat, StateLoadQueue};
use moose_core::thread::MainThreadCell;
use moose_core::wrapper::{
    PanicLatch, PluginCell, apply_or_queue_state, apply_queued_state, copy_c_str, enter_plugin,
    run_audio_block_latched, run_extern_callback_with,
};
use moose_params::{CatalogSignal, ParamFlags, ParamRange, Params, PersistChangeSignal};

use crate::ffi::{
    MOOSE_AAX_EVENT_EMITTED, MOOSE_AAX_EVENT_END, MOOSE_AAX_EVENT_INVALID,
    MOOSE_AAX_EVENT_QUEUE_FULL, MOOSE_AAX_EVENT_UNSUPPORTED, MOOSE_AAX_NATIVE_EVENT_CAP,
    MOOSE_AAX_NATIVE_EVENT_MIDI1, MOOSE_AAX_NATIVE_EVENT_SYSEX, MOOSE_AAX_PARAM_FLAG_AUTOMATABLE,
    MOOSE_AAX_PARAM_FLAG_BYPASS, MOOSE_AAX_PARAM_FLAG_HIDDEN, MOOSE_AAX_PARAM_FLAG_READ_ONLY,
    MOOSE_AAX_RANGE_CUSTOM, MOOSE_AAX_RANGE_DISCRETE, MOOSE_AAX_RANGE_LINEAR, MOOSE_AAX_RANGE_LOG,
    MOOSE_AAX_VIEW_HWND, MOOSE_AAX_VIEW_NSVIEW, MooseAaxHostSink, MooseAaxNativeEvent,
    MooseAaxParamInfo, MooseAaxTransportSnapshot,
};
use crate::host_sink::{AaxHostSink, editor_context, run_host_drain};
use crate::instance::{AaxAudio, AaxGui, AaxInstance, OutputTransaction};

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn primary_bus_layout<P: PluginExport>() -> BusLayout {
    P::bus_layouts()
        .into_iter()
        .next()
        .unwrap_or_else(BusLayout::stereo)
}

#[must_use]
pub(crate) fn aax_range_type(range: &ParamRange) -> u8 {
    match range {
        ParamRange::Linear { .. } | ParamRange::Stepped { .. } => MOOSE_AAX_RANGE_LINEAR,
        ParamRange::Logarithmic { .. } => MOOSE_AAX_RANGE_LOG,
        ParamRange::Discrete { .. } | ParamRange::Enum { .. } => MOOSE_AAX_RANGE_DISCRETE,
        _ => MOOSE_AAX_RANGE_CUSTOM,
    }
}

fn fill_param_info(info: &moose_params::ParamInfo, out: &mut MooseAaxParamInfo) {
    *out = MooseAaxParamInfo::default();
    out.id = info.id;
    unsafe {
        let _ = copy_c_str(out.name.as_mut_ptr(), out.name.len(), info.name);
        let _ = copy_c_str(out.unit.as_mut_ptr(), out.unit.len(), info.unit.as_str());
    }
    out.min_value = info.range.min();
    out.max_value = info.range.max();
    out.default_value = info.default_plain;
    out.step_count = info.range.step_count().map_or(0, NonZero::get);
    out.range_type = aax_range_type(&info.range);
    let mut flags = 0u32;
    if info.flags.contains(ParamFlags::AUTOMATABLE) {
        flags |= MOOSE_AAX_PARAM_FLAG_AUTOMATABLE;
    }
    if info.flags.contains(ParamFlags::READONLY) {
        flags |= MOOSE_AAX_PARAM_FLAG_READ_ONLY;
    }
    if info.flags.contains(ParamFlags::HIDDEN) {
        flags |= MOOSE_AAX_PARAM_FLAG_HIDDEN;
    }
    if info.flags.contains(ParamFlags::IS_BYPASS) {
        flags |= MOOSE_AAX_PARAM_FLAG_BYPASS;
    }
    out.flags = flags;
}

fn build_transport(snap: &MooseAaxTransportSnapshot) -> Option<TransportInfo> {
    if snap.valid == 0 {
        return None;
    }
    let mut info = TransportInfo {
        playing: snap.playing != 0,
        ..TransportInfo::default()
    };
    if snap.tempo.is_finite() && snap.tempo > 0.0 {
        info.tempo = snap.tempo;
    }
    if snap.time_sig_num > 0 && snap.time_sig_den > 0 {
        info.time_sig_num = u8::try_from(snap.time_sig_num).unwrap_or(4);
        info.time_sig_den = u8::try_from(snap.time_sig_den).unwrap_or(4);
    }
    if snap.position_samples.is_finite() {
        info.position_samples = sample_pos_i64(snap.position_samples);
    }
    if snap.position_beats.is_finite() {
        info.position_beats = snap.position_beats;
    }
    if snap.bar_start_beats.is_finite() {
        info.bar_start_beats = snap.bar_start_beats;
    }
    if snap.loop_active != 0 && snap.loop_start_beats.is_finite() && snap.loop_end_beats.is_finite()
    {
        info.loop_active = true;
        info.loop_start_beats = snap.loop_start_beats;
        info.loop_end_beats = snap.loop_end_beats;
    }
    Some(info)
}

fn format_fourcc_key(chunk_id: u32) -> Option<String> {
    let bytes = chunk_id.to_be_bytes();
    std::str::from_utf8(&bytes).ok().map(ToOwned::to_owned)
}

// ---------------------------------------------------------------------------
// Lifecycle callbacks
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn cb_create_with_sink<P: PluginExport>(
    raw_sink: *const MooseAaxHostSink,
) -> *mut c_void {
    run_extern_callback_with::<P, *mut c_void>(
        "aax",
        "create_with_sink",
        std::ptr::null_mut(),
        || {
            let host_sink = unsafe { AaxHostSink::from_raw(raw_sink) };
            let mut plugin = P::create();
            plugin.init();
            let info = P::info();

            let host_queue = Arc::new(HostQueue::new(|| {}));

            let params_arc = plugin.params_arc();
            let meter_store = plugin.meter_store();
            let snapshot = plugin.snapshot_slot();
            let task_spawner = plugin.task_spawner();
            let editor_builder = plugin.editor_builder();

            let persist_signal = params_arc.wants_persist_notifications().then(|| {
                let queue = Arc::clone(&host_queue);
                let signal = Arc::new(PersistChangeSignal::new(move || {
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

            let initial_catalog = params_arc.param_catalog();
            let layout = primary_bus_layout::<P>();
            let input_bus_count = layout.inputs.len();
            let output_bus_count = layout.outputs.len();
            let sidechain_channels = layout
                .inputs
                .iter()
                .skip(1)
                .find(|bus| bus.kind == BusKind::Sidechain)
                .map_or(0, |bus| bus.channels.channel_count());

            let initial_latency = plugin.latency();
            let initial_tail = plugin.tail();
            let delay = ReportedDelay::new(initial_latency, initial_tail);
            host_queue.signal(HostSignal::LATENCY);

            let audio = AaxAudio::new(Arc::clone(&initial_catalog), info.emits_midi, layout);

            let inst = Box::new(AaxInstance::<P> {
                persist_signal,
                catalog_signal,
                host_queue,
                _main_thread: main_thread,
                plugin: PluginCell::new(plugin),
                params_arc,
                exposed: Mutex::new(initial_catalog),
                meter_store,
                snapshot,
                task_spawner,
                editor_builder,
                delay,
                render_mode: AtomicU8::new(ProcessMode::Realtime.as_u8()),
                ui_thread: Mutex::new(std::thread::current().id()),
                editor_gestures: Mutex::new(GestureQueue::default()),
                automation: info.automation,
                input_bus_count,
                output_bus_count,
                sidechain_channels,
                plugin_id_hash: state::shared_plugin_state_hash(&info),
                audio: PluginCell::new(audio),
                gui: PluginCell::new(AaxGui::default()),
                pending_resize: AtomicU64::new(0),
                transport_slot: TransportSlot::new(),
                pending_state: Arc::new(StateLoadQueue::new(1)),
                panic_latch: PanicLatch::new(),
                host_sink,
            });
            Box::into_raw(inst).cast()
        },
    )
}

pub(crate) unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    run_extern_callback_with::<P, ()>("aax", "destroy", (), || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        if let Some(signal) = &inst.persist_signal {
            signal.close();
        }
        if let Some(signal) = &inst.catalog_signal {
            signal.close();
        }
        inst.host_queue.close();
        if let Some(mut gui) = inst.gui.try_enter() {
            if let Some(mut editor) = gui.editor.take() {
                editor.close();
            }
            gui.open = false;
        }
        drop(unsafe { Box::from_raw(ctx.cast::<AaxInstance<P>>()) });
    });
}

pub(crate) unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    if ctx.is_null() {
        return;
    }
    run_extern_callback_with::<P, ()>("aax", "reset", (), || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        inst.panic_latch.clear();
        let mode = load_render_mode(&inst.render_mode);
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            44_100.0
        };
        let mf = max_frames.max(1);
        let mut plugin = enter_plugin(&inst.plugin);
        let mut audio = enter_plugin(&inst.audio);
        audio.sample_rate = sr;
        audio.max_frames = mf;
        audio.catalog = inst.catalog_snapshot();
        let layout = audio.bus_layout.clone();
        let max_in = layout.total_input_channels().max(16) as usize;
        let max_out = layout.total_output_channels().max(8) as usize;
        audio.scratch.ensure_capacity(max_in, max_out, mf as usize);

        audio.prepared = false;
        let prep = PreparationConfig::new(
            AudioConfig::new(sr, mf as usize).with_process_mode(mode),
            PreparationFrameBound::NegotiatedHost,
        );
        if plugin.prepare(&prep).is_err() {
            return;
        }
        plugin.reset(&prep.audio);
        audio.prepared = true;

        let lat = plugin.latency();
        let tail = plugin.tail();
        inst.delay.publish(lat, tail);
        inst.host_queue.signal(HostSignal::LATENCY);
    });
}

pub(crate) unsafe extern "C" fn cb_set_render_mode<P: PluginExport>(ctx: *mut c_void, mode: u32) {
    if ctx.is_null() {
        return;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let clamped = match mode {
        1 => ProcessMode::Buffered.as_u8(),
        2 => ProcessMode::Offline.as_u8(),
        _ => ProcessMode::Realtime.as_u8(),
    };
    inst.render_mode.store(clamped, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Strict audio + MIDI process callback
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) unsafe extern "C" fn cb_process_native<P: PluginExport>(
    ctx: *mut c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_input_channels: u32,
    num_output_channels: u32,
    input_bus_active: u32,
    output_bus_active: u32,
    num_frames: u32,
    events: *const MooseAaxNativeEvent,
    num_events: u32,
    input_status: u32,
    transport: *const MooseAaxTransportSnapshot,
) -> u32 {
    if ctx.is_null() {
        return MOOSE_AAX_EVENT_INVALID;
    }
    let nf = num_frames as usize;
    if nf == 0 {
        return MOOSE_AAX_EVENT_END;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    if inst.panic_latch.is_failed() {
        unsafe { zero_raw_outputs(outputs, num_output_channels, nf) };
        return MOOSE_AAX_EVENT_INVALID;
    }

    let result = run_audio_block_latched::<P, u32>(&inst.panic_latch, "aax", || {
        let Some(mut plugin) = inst.plugin.try_enter() else {
            unsafe { zero_raw_outputs(outputs, num_output_channels, nf) };
            return MOOSE_AAX_EVENT_INVALID;
        };
        let Some(mut audio) = inst.audio.try_enter() else {
            unsafe { zero_raw_outputs(outputs, num_output_channels, nf) };
            return MOOSE_AAX_EVENT_INVALID;
        };

        // Run queued state loads before entering `RtSection`: `load_state`
        // may allocate, matching CLAP, VST3, and AU.
        apply_queued_state(&mut *plugin, &inst.pending_state);

        let _rt = RtSection::enter();

        if !audio.prepared || num_frames > audio.max_frames {
            unsafe { zero_raw_outputs(outputs, num_output_channels, nf) };
            audio.output_events.clear();
            audio.staging_status = MOOSE_AAX_EVENT_INVALID;
            return MOOSE_AAX_EVENT_INVALID;
        }

        audio.event_list.clear();
        audio.output_events.clear();
        audio.output_tx = None;

        let mut stage_status = input_status;
        if stage_status == MOOSE_AAX_EVENT_END && num_events > 0 {
            if events.is_null() || num_events > MOOSE_AAX_NATIVE_EVENT_CAP {
                stage_status = if events.is_null() {
                    MOOSE_AAX_EVENT_INVALID
                } else {
                    MOOSE_AAX_EVENT_QUEUE_FULL
                };
            } else {
                let ev_slice = unsafe { slice::from_raw_parts(events, num_events as usize) };
                for ev in ev_slice {
                    if ev.sample_offset >= num_frames {
                        stage_status = MOOSE_AAX_EVENT_INVALID;
                        break;
                    }
                    match ev.kind {
                        MOOSE_AAX_NATIVE_EVENT_MIDI1 => {
                            let len = ev.data_len as usize;
                            if len == 0 || len > 3 {
                                stage_status = MOOSE_AAX_EVENT_INVALID;
                                break;
                            }
                            let bytes = &ev.midi[..len];
                            let d1 = if len > 1 { bytes[1] } else { 0 };
                            let d2 = if len > 2 { bytes[2] } else { 0 };
                            let Some(body) = decode_short_message(bytes[0], d1, d2) else {
                                stage_status = MOOSE_AAX_EVENT_INVALID;
                                break;
                            };
                            #[allow(clippy::cast_possible_truncation)]
                            let Some(raw) = RawMidi1::new([bytes[0], d1, d2], len as u8) else {
                                stage_status = MOOSE_AAX_EVENT_INVALID;
                                break;
                            };
                            let exact = ExactEvent::new(
                                ev.sample_offset,
                                ExactEventBody::Midi1 {
                                    port: 0,
                                    message: raw,
                                },
                            );
                            if audio
                                .event_list
                                .try_push_with_exact(
                                    Event {
                                        sample_offset: ev.sample_offset,
                                        port: 0,
                                        body,
                                    },
                                    exact,
                                )
                                .is_err()
                            {
                                stage_status = MOOSE_AAX_EVENT_QUEUE_FULL;
                                break;
                            }
                        }
                        MOOSE_AAX_NATIVE_EVENT_SYSEX => {
                            let len = ev.data_len as usize;
                            let payload = if len == 0 {
                                &[][..]
                            } else if ev.sysex.is_null() {
                                stage_status = MOOSE_AAX_EVENT_INVALID;
                                break;
                            } else {
                                unsafe { slice::from_raw_parts(ev.sysex, len) }
                            };
                            let exact = ExactEvent::new(
                                ev.sample_offset,
                                ExactEventBody::SysEx { port: 0 },
                            );
                            if audio
                                .event_list
                                .try_push_sysex_with_exact_on_port(
                                    ev.sample_offset,
                                    0,
                                    payload,
                                    exact,
                                )
                                .is_err()
                            {
                                stage_status = MOOSE_AAX_EVENT_QUEUE_FULL;
                                break;
                            }
                        }
                        _ => {
                            stage_status = MOOSE_AAX_EVENT_INVALID;
                            break;
                        }
                    }
                }
            }
        }
        if stage_status != MOOSE_AAX_EVENT_END {
            audio.event_list.clear();
        }

        let transport_info = if transport.is_null() {
            TransportInfo::default()
        } else {
            build_transport(unsafe { &*transport }).unwrap_or_default()
        };
        inst.transport_slot.write(&transport_info);

        let bus_routing = build_single_sidechain_bus_routing(
            inst.input_bus_count,
            inst.output_bus_count,
            num_input_channels,
            num_output_channels,
            inst.sidechain_channels,
            Some((input_bus_active, output_bus_active)),
        );

        let AaxAudio {
            scratch,
            event_list,
            sub_event_scratch,
            output_events,
            sample_rate,
            catalog,
            ..
        } = &mut *audio;

        let mut block = CAbiAudioBlock {
            plugin: &mut *plugin,
            params: inst.params_arc.as_ref(),
            scratch,
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            num_frames,
            event_list,
            sub_event_scratch,
            output_events,
            transport: transport_info,
            sample_rate: *sample_rate,
            process_mode: load_render_mode(&inst.render_mode),
            catalog: catalog.as_ref(),
            automation: inst.automation,
            bus_routing,
        };
        unsafe { run_c_abi_audio_block(&mut block) };
        inst.delay
            .observe_and_signal_no_wake(plugin.latency(), plugin.tail(), &inst.host_queue);

        if audio.output_events.overflow().is_some() {
            audio.staging_status = MOOSE_AAX_EVENT_QUEUE_FULL;
        } else {
            audio.staging_status = stage_status;
        }
        stage_status
    });

    if inst.panic_latch.is_failed() {
        unsafe { zero_raw_outputs(outputs, num_output_channels, nf) };
        return MOOSE_AAX_EVENT_INVALID;
    }
    result.unwrap_or(MOOSE_AAX_EVENT_INVALID)
}

// ---------------------------------------------------------------------------
// Transactional output MIDI / SysEx drain
// ---------------------------------------------------------------------------

fn preflight_aax_output(audio: &AaxAudio<impl PluginExport>, num_frames: u32) -> u32 {
    if audio.staging_status != MOOSE_AAX_EVENT_END {
        return audio.staging_status;
    }
    if audio.output_events.overflow().is_some() {
        return MOOSE_AAX_EVENT_QUEUE_FULL;
    }
    if audio.output_events.is_empty() {
        return MOOSE_AAX_EVENT_END;
    }
    if !audio.emits_midi {
        return MOOSE_AAX_EVENT_UNSUPPORTED;
    }
    let mut event_count = 0u32;
    let mut sysex_bytes = 0usize;
    for item in audio.output_events.lossless_iter() {
        let sample_offset = match item {
            LosslessEventRef::Typed(ev) => ev.sample_offset,
            LosslessEventRef::Exact(exact) => exact.sample_offset(),
        };
        if sample_offset >= num_frames {
            return MOOSE_AAX_EVENT_INVALID;
        }
        event_count = event_count.saturating_add(1);
        if event_count > MOOSE_AAX_NATIVE_EVENT_CAP {
            return MOOSE_AAX_EVENT_QUEUE_FULL;
        }
        match item {
            LosslessEventRef::Typed(ev) => match &ev.body {
                EventBody::SysEx { .. } => {
                    let payload = audio.output_events.sysex_bytes(&ev.body);
                    sysex_bytes = sysex_bytes.saturating_add(payload.len());
                    if sysex_bytes > crate::ffi::MOOSE_AAX_SYSEX_POOL_CAP as usize {
                        return MOOSE_AAX_EVENT_QUEUE_FULL;
                    }
                }
                _ => {
                    if event_to_midi1(&ev.body).is_none() {
                        return MOOSE_AAX_EVENT_UNSUPPORTED;
                    }
                }
            },
            LosslessEventRef::Exact(exact) => {
                if !exact.metadata().is_none() {
                    return MOOSE_AAX_EVENT_UNSUPPORTED;
                }
                match exact.body() {
                    ExactEventBody::Midi1 { port: 0, message } => {
                        if message.is_empty() || message.len() > 3 {
                            return MOOSE_AAX_EVENT_INVALID;
                        }
                    }
                    ExactEventBody::SysEx { port: 0 } => {
                        let Some(payload) = exact.sysex_bytes_checked() else {
                            return MOOSE_AAX_EVENT_INVALID;
                        };
                        sysex_bytes = sysex_bytes.saturating_add(payload.len());
                        if sysex_bytes > crate::ffi::MOOSE_AAX_SYSEX_POOL_CAP as usize {
                            return MOOSE_AAX_EVENT_QUEUE_FULL;
                        }
                    }
                    _ => return MOOSE_AAX_EVENT_UNSUPPORTED,
                }
            }
        }
    }
    MOOSE_AAX_EVENT_END
}

pub(crate) unsafe extern "C" fn cb_begin_output_events<P: PluginExport>(
    ctx: *mut c_void,
    num_frames: u32,
) {
    if ctx.is_null() {
        return;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let Some(mut audio) = inst.audio.try_enter() else {
        return;
    };
    let preflight_error = preflight_aax_output(&audio, num_frames);
    audio.output_tx = Some(OutputTransaction {
        cursor: LosslessEventCursor::default(),
        preflight_error,
    });
}

pub(crate) unsafe extern "C" fn cb_next_output_event<P: PluginExport>(
    ctx: *mut c_void,
    out_event: *mut MooseAaxNativeEvent,
) -> u32 {
    if ctx.is_null() || out_event.is_null() {
        return MOOSE_AAX_EVENT_INVALID;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let Some(mut audio) = inst.audio.try_enter() else {
        return MOOSE_AAX_EVENT_INVALID;
    };
    let AaxAudio {
        output_events,
        output_tx,
        ..
    } = &mut *audio;
    let Some(tx) = output_tx.as_mut() else {
        return MOOSE_AAX_EVENT_END;
    };
    if tx.preflight_error != MOOSE_AAX_EVENT_END {
        return tx.preflight_error;
    }
    let Some(item) = output_events.lossless_next(&mut tx.cursor) else {
        return MOOSE_AAX_EVENT_END;
    };
    let sample_offset = match item {
        LosslessEventRef::Typed(ev) => ev.sample_offset,
        LosslessEventRef::Exact(exact) => exact.sample_offset(),
    };
    let mut ev = MooseAaxNativeEvent {
        sample_offset,
        ..MooseAaxNativeEvent::default()
    };
    match item {
        LosslessEventRef::Typed(typed) => {
            if let EventBody::SysEx { .. } = &typed.body {
                let payload = output_events.sysex_bytes(&typed.body);
                ev.kind = MOOSE_AAX_NATIVE_EVENT_SYSEX;
                #[allow(clippy::cast_possible_truncation)]
                {
                    ev.data_len = payload.len() as u32;
                }
                ev.sysex = if payload.is_empty() {
                    std::ptr::null()
                } else {
                    payload.as_ptr()
                };
            } else {
                let Some((len, bytes)) = event_to_midi1(&typed.body) else {
                    return MOOSE_AAX_EVENT_UNSUPPORTED;
                };
                ev.kind = MOOSE_AAX_NATIVE_EVENT_MIDI1;
                ev.data_len = u32::try_from(len).unwrap_or(0);
                ev.midi[..len].copy_from_slice(&bytes[..len]);
            }
        }
        LosslessEventRef::Exact(exact) => match exact.body() {
            ExactEventBody::Midi1 { message, .. } => {
                let len = message.len();
                ev.kind = MOOSE_AAX_NATIVE_EVENT_MIDI1;
                ev.data_len = u32::try_from(len).unwrap_or(0);
                ev.midi[..len].copy_from_slice(&message.storage()[..len]);
            }
            ExactEventBody::SysEx { .. } => {
                let payload = exact.sysex_bytes_checked().unwrap_or(&[]);
                ev.kind = MOOSE_AAX_NATIVE_EVENT_SYSEX;
                #[allow(clippy::cast_possible_truncation)]
                {
                    ev.data_len = payload.len() as u32;
                }
                ev.sysex = if payload.is_empty() {
                    std::ptr::null()
                } else {
                    payload.as_ptr()
                };
            }
            _ => return MOOSE_AAX_EVENT_UNSUPPORTED,
        },
    }
    unsafe { *out_event = ev };
    MOOSE_AAX_EVENT_EMITTED
}

pub(crate) unsafe extern "C" fn cb_finish_output_events<P: PluginExport>(
    ctx: *mut c_void,
    _status: u32,
) {
    if ctx.is_null() {
        return;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let Some(mut audio) = inst.audio.try_enter() else {
        return;
    };
    audio.output_tx = None;
    audio.output_events.clear();
    audio.staging_status = MOOSE_AAX_EVENT_END;
}

// ---------------------------------------------------------------------------
// Live parameter catalog & value/text/taper callbacks
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut c_void) -> u32 {
    if ctx.is_null() {
        return 0;
    }
    run_extern_callback_with::<P, u32>("aax", "param_count", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let catalog = inst.catalog_snapshot();
        u32::try_from(catalog.len()).unwrap_or(u32::MAX)
    })
}

pub(crate) unsafe extern "C" fn cb_param_ids<P: PluginExport>(
    ctx: *mut c_void,
    out_ids: *mut u32,
    cap: u32,
) -> u32 {
    if ctx.is_null() {
        return 0;
    }
    run_extern_callback_with::<P, u32>("aax", "param_ids", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let catalog = inst.catalog_snapshot();
        let total = u32::try_from(catalog.len()).unwrap_or(u32::MAX);
        if out_ids.is_null() || cap == 0 {
            return total;
        }
        let n = (cap as usize).min(catalog.len());
        let out = unsafe { slice::from_raw_parts_mut(out_ids, n) };
        for (dst, info) in out.iter_mut().zip(catalog.iter()) {
            *dst = info.id;
        }
        total
    })
}

pub(crate) unsafe extern "C" fn cb_param_info_by_id<P: PluginExport>(
    ctx: *mut c_void,
    param_id: u32,
    out_info: *mut MooseAaxParamInfo,
) -> i32 {
    if ctx.is_null() || out_info.is_null() {
        return 0;
    }
    run_extern_callback_with::<P, i32>("aax", "param_info_by_id", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let catalog = inst.catalog_snapshot();
        let Some(info) = catalog.iter().find(|p| p.id == param_id) else {
            return 0;
        };
        fill_param_info(info, unsafe { &mut *out_info });
        1
    })
}

pub(crate) unsafe extern "C" fn cb_param_get<P: PluginExport>(
    ctx: *mut c_void,
    param_id: u32,
) -> f64 {
    if ctx.is_null() {
        return 0.0;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.params_arc.get_plain(param_id).unwrap_or(0.0)
}

pub(crate) unsafe extern "C" fn cb_param_set<P: PluginExport>(
    ctx: *mut c_void,
    param_id: u32,
    value: f64,
) {
    if ctx.is_null() || !value.is_finite() {
        return;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.params_arc.set_plain(param_id, value);
    if inst.params_arc.parameter_affects_presentation(param_id) {
        inst.host_queue.signal(HostSignal::PRESENTATION);
    }
}

pub(crate) unsafe extern "C" fn cb_param_format<P: PluginExport>(
    ctx: *mut c_void,
    param_id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    if ctx.is_null() {
        return 0;
    }
    run_extern_callback_with::<P, u32>("aax", "param_format", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        unsafe { format_param_to_c_buf(inst.params_arc.as_ref(), param_id, value, out, out_len) }
    })
}

pub(crate) unsafe extern "C" fn cb_param_parse<P: PluginExport>(
    ctx: *mut c_void,
    param_id: u32,
    text: *const c_char,
    out_plain: *mut f64,
) -> i32 {
    if ctx.is_null() {
        return 0;
    }
    run_extern_callback_with::<P, i32>("aax", "param_parse", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        unsafe { parse_param_from_c_str(inst.params_arc.as_ref(), param_id, text, out_plain) }
    })
}

pub(crate) unsafe extern "C" fn cb_param_normalize<P: PluginExport>(
    ctx: *mut c_void,
    param_id: u32,
    plain: f64,
) -> f64 {
    if ctx.is_null() {
        return 0.0;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.catalog_snapshot()
        .by_id(param_id)
        .map_or(0.0, |info| info.range.normalize(plain))
        .clamp(0.0, 1.0)
}

pub(crate) unsafe extern "C" fn cb_param_denormalize<P: PluginExport>(
    ctx: *mut c_void,
    param_id: u32,
    normalized: f64,
) -> f64 {
    if ctx.is_null() {
        return 0.0;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.catalog_snapshot().by_id(param_id).map_or(0.0, |info| {
        info.range.denormalize(normalized.clamp(0.0, 1.0))
    })
}

// ---------------------------------------------------------------------------
// State chunk callbacks (shared c_abi_driver + atomic rollback)
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn cb_state_save<P: PluginExport>(
    ctx: *mut c_void,
    out_data: *mut *mut u8,
) -> u32 {
    if ctx.is_null() || out_data.is_null() {
        return 0;
    }
    run_extern_callback_with::<P, u32>("aax", "state_save", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let Some((ptr, len)) = (unsafe {
            save_state_to_host_buf(
                &inst.plugin,
                &inst.params_arc,
                &inst.snapshot,
                &inst.pending_state,
                inst.plugin_id_hash,
                true,
                malloc,
                free,
            )
        }) else {
            unsafe { *out_data = std::ptr::null_mut() };
            return 0;
        };
        unsafe { *out_data = ptr };
        len
    })
}

pub(crate) unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut c_void,
    data: *const u8,
    len: u32,
) -> i32 {
    if ctx.is_null() || data.is_null() || len == 0 {
        return 0;
    }
    run_extern_callback_with::<P, i32>("aax", "state_load", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let blob = unsafe { slice::from_raw_parts(data, len as usize) };
        let Some((deserialized, backup)) = load_state_from_host_slice::<P>(
            blob,
            inst.plugin_id_hash,
            PluginFormat::Aax,
            None,
            inst.params_arc.as_ref(),
        ) else {
            return 0;
        };
        let ok = apply_or_queue_state(
            &inst.plugin,
            inst.params_arc.as_ref(),
            &inst.snapshot,
            &inst.pending_state,
            deserialized,
            &backup,
            true,
        )
        .is_ok();
        if ok {
            inst.sync_catalog();
            inst.host_queue.signal(HostSignal::PARAM_RESCAN);
            1
        } else {
            0
        }
    })
}

pub(crate) unsafe extern "C" fn cb_state_load_foreign<P: PluginExport>(
    ctx: *mut c_void,
    chunk_id: u32,
    data: *const u8,
    len: u32,
) -> i32 {
    if ctx.is_null() || data.is_null() || len == 0 {
        return 0;
    }
    run_extern_callback_with::<P, i32>("aax", "state_load_foreign", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let bytes = unsafe { slice::from_raw_parts(data, len as usize) };
        let key = format_fourcc_key(chunk_id);
        let Some((deserialized, backup)) = load_foreign_state_from_host_slice::<P>(
            PluginFormat::Aax,
            key.as_deref(),
            bytes,
            inst.params_arc.as_ref(),
        ) else {
            return 0;
        };
        let ok = apply_or_queue_state(
            &inst.plugin,
            inst.params_arc.as_ref(),
            &inst.snapshot,
            &inst.pending_state,
            deserialized,
            &backup,
            true,
        )
        .is_ok();
        if ok {
            inst.sync_catalog();
            inst.host_queue.signal(HostSignal::PARAM_RESCAN);
            1
        } else {
            0
        }
    })
}

pub(crate) unsafe extern "C" fn cb_state_free(data: *mut u8, _len: u32) {
    if !data.is_null() {
        unsafe { free(data.cast()) };
    }
}

// ---------------------------------------------------------------------------
// Delay & HostQueue drain
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn cb_latency_samples<P: PluginExport>(ctx: *mut c_void) -> u32 {
    if ctx.is_null() {
        return 0;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.delay.latency()
}

pub(crate) unsafe extern "C" fn cb_tail_samples<P: PluginExport>(ctx: *mut c_void) -> u32 {
    if ctx.is_null() {
        return 0;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.delay.tail()
}

pub(crate) unsafe extern "C" fn cb_drain_host_queue<P: PluginExport>(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    run_extern_callback_with::<P, ()>("aax", "drain_host_queue", (), || unsafe {
        run_host_drain::<P>(ctx);
    });
}

// ---------------------------------------------------------------------------
// Editor callbacks
// ---------------------------------------------------------------------------

fn ensure_editor<P: PluginExport>(inst: &AaxInstance<P>, gui: &mut AaxGui) -> bool {
    if gui.editor.is_some() {
        return true;
    }
    gui.editor =
        (inst.editor_builder)(Arc::clone(&inst.params_arc)).filter(|editor| editor.has_ui());
    gui.editor.is_some()
}

pub(crate) unsafe extern "C" fn cb_editor_has<P: PluginExport>(ctx: *mut c_void) -> i32 {
    if ctx.is_null() {
        return 0;
    }
    run_extern_callback_with::<P, i32>("aax", "editor_has", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let Some(mut gui) = inst.gui.try_enter() else {
            return 0;
        };
        i32::from(ensure_editor(inst, &mut gui))
    })
}

pub(crate) unsafe extern "C" fn cb_editor_get_size<P: PluginExport>(
    ctx: *mut c_void,
    width: *mut u32,
    height: *mut u32,
) -> i32 {
    if ctx.is_null() || width.is_null() || height.is_null() {
        return 0;
    }
    run_extern_callback_with::<P, i32>("aax", "editor_get_size", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let Some(mut gui) = inst.gui.try_enter() else {
            return 0;
        };
        if !ensure_editor(inst, &mut gui) {
            return 0;
        }
        let Some(editor) = gui.editor.as_ref() else {
            return 0;
        };
        let (w, h) = editor.size();
        unsafe {
            *width = w;
            *height = h;
        }
        1
    })
}

pub(crate) unsafe extern "C" fn cb_editor_open<P: PluginExport>(
    ctx: *mut c_void,
    parent_view: *mut c_void,
    platform: i32,
) -> i32 {
    if ctx.is_null() || parent_view.is_null() {
        return 0;
    }
    run_extern_callback_with::<P, i32>("aax", "editor_open", 0, || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        inst.record_ui_thread();
        let Some(mut gui) = inst.gui.try_enter() else {
            return 0;
        };
        if !ensure_editor(inst, &mut gui) {
            return 0;
        }
        let handle = match platform {
            MOOSE_AAX_VIEW_NSVIEW => RawWindowHandle::AppKit(parent_view),
            MOOSE_AAX_VIEW_HWND => RawWindowHandle::Win32(parent_view),
            _ => return 0,
        };
        if let Some(editor) = gui.editor.as_mut() {
            let context = unsafe { editor_context::<P>(ctx) };
            editor.open(handle, context);
            let opened = editor.is_open();
            gui.open = opened;
            i32::from(opened)
        } else {
            0
        }
    })
}

pub(crate) unsafe extern "C" fn cb_editor_close<P: PluginExport>(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    run_extern_callback_with::<P, ()>("aax", "editor_close", (), || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let Some(mut gui) = inst.gui.try_enter() else {
            return;
        };
        if let Some(mut editor) = gui.editor.take() {
            editor.close();
        }
        gui.open = false;
    });
}

pub(crate) unsafe extern "C" fn cb_editor_idle<P: PluginExport>(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    run_extern_callback_with::<P, ()>("aax", "editor_idle", (), || {
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        if let Some(mut gui) = inst.gui.try_enter()
            && gui.open
            && let Some(editor) = gui.editor.as_mut()
        {
            editor.idle();
        }
    });
}

// ---------------------------------------------------------------------------
// Real-time allocation smoke test (for cross_format_contract / rt-paranoid)
// ---------------------------------------------------------------------------

/// Run one prepared AAX `process_native` block inside an `rt-paranoid` window
/// and verify zero allocations on the audio thread.
///
/// # Panics
///
/// Panics if instance creation or `process_native` fails.
pub fn rt_paranoid_smoke<P: PluginExport>() {
    let cbs = crate::callbacks::<P>();
    let ctx = unsafe { (cbs.create_with_sink)(std::ptr::null()) };
    assert!(!ctx.is_null(), "aax create_with_sink returned null");
    unsafe { (cbs.reset)(ctx, 48_000.0, 128) };

    let layout = primary_bus_layout::<P>();
    let n_in = layout.total_input_channels().max(2) as usize;
    let n_out = layout.total_output_channels().max(2) as usize;
    let in_bufs: Vec<Vec<f32>> = (0..n_in).map(|_| vec![0.25f32; 64]).collect();
    let mut out_bufs: Vec<Vec<f32>> = (0..n_out).map(|_| vec![0.0f32; 64]).collect();
    let in_ptrs: Vec<*const f32> = in_bufs.iter().map(Vec::as_ptr).collect();
    let mut out_ptrs: Vec<*mut f32> = out_bufs.iter_mut().map(Vec::as_mut_ptr).collect();
    let snap = MooseAaxTransportSnapshot {
        valid: 1,
        playing: 1,
        tempo: 120.0,
        time_sig_num: 4,
        time_sig_den: 4,
        ..MooseAaxTransportSnapshot::default()
    };

    for _ in 0..2 {
        let status = unsafe {
            (cbs.process_native)(
                ctx,
                in_ptrs.as_ptr(),
                out_ptrs.as_mut_ptr(),
                u32::try_from(n_in).unwrap_or(2),
                u32::try_from(n_out).unwrap_or(2),
                1,
                1,
                64,
                std::ptr::null(),
                0,
                MOOSE_AAX_EVENT_END,
                &raw const snap,
            )
        };
        assert_eq!(status, MOOSE_AAX_EVENT_END);
        unsafe {
            (cbs.begin_output_events)(ctx, 64);
            let mut ev = MooseAaxNativeEvent::default();
            while (cbs.next_output_event)(ctx, &raw mut ev) == MOOSE_AAX_EVENT_EMITTED {}
            (cbs.finish_output_events)(ctx, MOOSE_AAX_EVENT_EMITTED);
        }
    }

    unsafe { (cbs.destroy)(ctx) };
}
