//! The process callback and GUI-initiated output parameter changes.

use crate::CLAP_VERSION;
use crate::create_plugin_instance;
use crate::data::ClapAudio;
use crate::data::ClapPluginData;
use crate::data::GuiParamChange;
use crate::data::HostOutPtr;
use crate::data::input_channel_slice;
use crate::data::input_overlaps_outputs;
use crate::data::output_channel_slice;
use crate::data_from_plugin;
use crate::events::ExactEmit;
use crate::events::build_transport_info;
use crate::events::convert_input_events;
use crate::events::emit_exact_clap;
use crate::events::is_portless_output_body;
use crate::events::outbound_note_end_pattern;
use crate::events::preflight_clap_output;
use crate::events::typed_output_body_is_valid;
use crate::params_ext::params_flush;
use crate::state_ext::request_changed_presentation;
use crate::state_ext::request_value_rescan;

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_IS_LIVE, CLAP_EVENT_MIDI, CLAP_EVENT_MIDI_SYSEX,
    CLAP_EVENT_MIDI2, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_GESTURE_BEGIN,
    CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_VALUE, clap_event_header, clap_event_midi,
    clap_event_midi_sysex, clap_event_midi2, clap_event_note, clap_event_param_gesture,
    clap_event_param_value, clap_output_events,
};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{
    CLAP_PROCESS_CONTINUE, CLAP_PROCESS_CONTINUE_IF_NOT_QUIET, CLAP_PROCESS_ERROR,
    CLAP_PROCESS_SLEEP, CLAP_PROCESS_TAIL, clap_process,
};
use moose_core::Float;
use moose_core::buffer::AudioBuffer;
use moose_core::bus_routing::BusRoute;
use moose_core::cast::{len_u32, size_of_u32};
use moose_core::chunked_process::{
    ChunkedProcess, process_chunked_with_bus_routing_catalog_and_pool,
};
use moose_core::config::ProcessMode;
use moose_core::events::{EventBody, LosslessEventRef, OutputEventStatus, TransportInfo};
use moose_core::export::PluginExport;
use moose_core::host_queue::HostSignal;
use moose_core::info::MidiDialect;
use moose_core::midi::pitch_bend_to_bytes;
use moose_core::process::ProcessStatus;
use moose_core::rt::{RtSection, audit};
use moose_core::ump::encode_ump_channel_voice_2;
use moose_core::wrapper::{apply_queued_state, run_audio_block_latched};
use moose_params::Params;
use std::ffi::{c_char, c_void};
use std::mem::transmute;
use std::ptr;
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// Flush GUI-initiated param changes to CLAP output events
// ---------------------------------------------------------------------------

pub(crate) unsafe fn flush_gui_changes<P: PluginExport>(
    data: &ClapPluginData<P>,
    out_events: *const clap_output_events,
) -> bool {
    unsafe {
        if out_events.is_null() {
            return true;
        }
        let Some(try_push) = (*out_events).try_push else {
            return true;
        };

        // Take a bounded snapshot before reading the gate. A later producer
        // cannot substitute its new event for one in this FIFO prefix.
        #[cfg(feature = "clap-draft")]
        let limit = data.undo_groups.gui_drained()
            + (data.undo_groups.retry_gui_change.len() + data.gui_changes.len()) as u64;
        #[cfg(feature = "clap-draft")]
        let limit = limit.min(data.undo_groups.begin_gate());
        loop {
            #[cfg(feature = "clap-draft")]
            if data.undo_groups.gui_drained() >= limit {
                break;
            }
            #[cfg(feature = "clap-draft")]
            let next = data
                .undo_groups
                .retry_gui_change
                .pop()
                .or_else(|| data.gui_changes.pop());
            #[cfg(not(feature = "clap-draft"))]
            let next = data.gui_changes.pop();
            let Some(change) = next else {
                break;
            };
            let accepted = match change {
                GuiParamChange::GestureBegin(id) => {
                    let event = clap_event_param_gesture {
                        header: clap_event_header {
                            size: size_of_u32::<clap_event_param_gesture>(),
                            time: 0,
                            space_id: CLAP_CORE_EVENT_SPACE_ID,
                            type_: CLAP_EVENT_PARAM_GESTURE_BEGIN,
                            flags: CLAP_EVENT_IS_LIVE,
                        },
                        param_id: id,
                    };
                    try_push(out_events, &raw const event.header)
                }
                GuiParamChange::Value(id, plain) => {
                    let event = clap_event_param_value {
                        header: clap_event_header {
                            size: size_of_u32::<clap_event_param_value>(),
                            time: 0,
                            space_id: CLAP_CORE_EVENT_SPACE_ID,
                            type_: CLAP_EVENT_PARAM_VALUE,
                            flags: CLAP_EVENT_IS_LIVE,
                        },
                        param_id: id,
                        cookie: ptr::null_mut(),
                        note_id: -1,
                        port_index: -1,
                        channel: -1,
                        key: -1,
                        value: plain,
                    };
                    let accepted =
                        !plain.is_finite() || try_push(out_events, &raw const event.header);
                    if !accepted {
                        request_value_rescan(data);
                    }
                    accepted
                }
                GuiParamChange::GestureEnd(id) => {
                    let event = clap_event_param_gesture {
                        header: clap_event_header {
                            size: size_of_u32::<clap_event_param_gesture>(),
                            time: 0,
                            space_id: CLAP_CORE_EVENT_SPACE_ID,
                            type_: CLAP_EVENT_PARAM_GESTURE_END,
                            flags: CLAP_EVENT_IS_LIVE,
                        },
                        param_id: id,
                    };
                    try_push(out_events, &raw const event.header)
                }
            };
            if !accepted {
                #[cfg(feature = "clap-draft")]
                {
                    // Only this consumer accesses the retry slot. Keep the refused event first.
                    let _ = data.undo_groups.retry_gui_change.push(change);
                    data.host_queue.signal(HostSignal::UNDO_GROUP);
                }
                return false;
            }
            #[cfg(feature = "clap-draft")]
            data.undo_groups.note_gui_drained();
        }
        #[cfg(feature = "clap-draft")]
        if data.undo_groups.should_wake_main_thread() {
            data.host_queue.signal(HostSignal::UNDO_GROUP);
        }
        // A rescan before draining could be overwritten by older FIFO values.
        // Re-read only after all of those events have reached the host.
        if data.gui_changes.is_empty() && data.gui_overflow.swap(false, Ordering::AcqRel) {
            request_value_rescan(data);
        }
        true
    }
}

/// Stack-pinned thin wrapper around `&dyn Fn(u32) + Sync` stored in
/// `ClapPluginData::active_pool_task` for the exact duration of
/// `clap_host_thread_pool.request_exec`.
struct PoolTask<'a> {
    f: &'a (dyn Fn(u32) + Sync),
}

struct ClearPoolTask<'a>(&'a std::sync::atomic::AtomicPtr<c_void>);

impl Drop for ClearPoolTask<'_> {
    fn drop(&mut self) {
        self.0.store(ptr::null_mut(), Ordering::Release);
    }
}

/// `clap.thread-pool::exec` — invoked concurrently on host RT worker threads
/// while `request_exec` is active on the audio thread.
pub(crate) unsafe extern "C" fn thread_pool_exec<P: PluginExport>(
    plugin: *const clap_plugin,
    task_index: u32,
) {
    if plugin.is_null() {
        return;
    }
    // SAFETY: the host passes its live plugin instance while `request_exec`
    // is active on the audio thread.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    // Route worker panics through the same silent latched audio boundary as
    // `clap_plugin_process` so a panicking worker records to `panic_latch`
    // without printing on a host RT thread.
    let _ = run_audio_block_latched::<P, ()>(&data.panic_latch, "CLAP", || unsafe {
        let raw = data.active_pool_task.load(Ordering::Acquire);
        if raw.is_null() {
            return;
        }
        // SAFETY: `raw` points to a stack-pinned `PoolTask` kept alive by
        // `request_exec` on the audio thread until all worker `exec` calls join.
        let task = &*raw.cast::<PoolTask<'_>>();
        (task.f)(task_index);
    });
}

// ---------------------------------------------------------------------------
// Process callback
// ---------------------------------------------------------------------------

/// # Safety
/// Non-null bus arrays must contain `count` readable CLAP buffers.
pub(crate) unsafe fn audio_bus_channels_match(
    buffers: *const clap_audio_buffer,
    count: u32,
    channels: impl Iterator<Item = usize>,
) -> bool {
    if count != 0 && buffers.is_null() {
        return false;
    }
    let mut channels = channels.filter(|&count| count != 0);
    for index in 0..count {
        let Some(expected) = channels.next() else {
            return false;
        };
        // SAFETY: the caller supplies a live array; excess buses are rejected above.
        if unsafe { (*buffers.add(index as usize)).channel_count as usize } != expected {
            return false;
        }
    }
    channels.next().is_none()
}

#[allow(clippy::too_many_lines)]
pub(crate) unsafe extern "C" fn clap_plugin_process<P: PluginExport>(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> i32 {
    let mut audio_buffers_valid = false;
    // SAFETY: the host passes its live plugin instance.
    let latch = unsafe { &data_from_plugin::<P>(plugin).panic_latch };
    let result = run_audio_block_latched::<P, i32>(latch, "CLAP", || unsafe {
        if process.is_null() {
            return CLAP_PROCESS_ERROR;
        }

        let proc = &*process;
        let data = data_from_plugin::<P>(plugin);
        data.host.debug_assert_audio_thread(data.host_thread_check);
        let num_frames = proc.frames_count as usize;

        if num_frames == 0 {
            params_flush::<P>(plugin, proc.in_events, proc.out_events);
            return CLAP_PROCESS_CONTINUE;
        }

        // Strictly wait-free audio entry: if a misbehaving host overlaps a
        // main-thread lifecycle call (`activate`/`deactivate`) with `process`,
        // we cannot read `audio.bus_routing` to validate `proc.audio_outputs`
        // channel counts, so returning `CLAP_PROCESS_ERROR` without touching
        // unvalidated buffer pointers is the only sound option. Once validated
        // below, contention on `data.plugin` zeroes outputs and returns
        // `CLAP_PROCESS_CONTINUE` so the host graph stays alive.
        let Some(mut audio) = data.audio.try_enter() else {
            return CLAP_PROCESS_ERROR;
        };
        if num_frames > audio.max_block_size
            || !audio_bus_channels_match(
                proc.audio_inputs,
                proc.audio_inputs_count,
                audio.bus_routing.inputs().map(BusRoute::channel_count),
            )
            || !audio_bus_channels_match(
                proc.audio_outputs,
                proc.audio_outputs_count,
                audio.bus_routing.outputs().map(BusRoute::channel_count),
            )
        {
            return CLAP_PROCESS_ERROR;
        }
        audio_buffers_valid = true;

        // Panicked earlier and not re-activated since: don't re-enter a
        // plugin in an unknown state. Silence; the error was reported once.
        if data.panic_latch.is_failed() {
            zero_clap_output_buffers(process);
            return CLAP_PROCESS_SLEEP;
        }

        // Strictly wait-free plugin entry: an uncontended `Acquire`, never
        // parking the audio thread. If a misbehaving host overlaps a
        // main-thread callback with `process`, output silence for this block.
        let Some(mut instance) = data.plugin.try_enter() else {
            zero_clap_output_buffers(process);
            return CLAP_PROCESS_CONTINUE;
        };

        // Apply any state-load that the host or editor handed us
        // since the last block. Runs before per-block work so the
        // plugin sees consistent params for the entire block. The
        // single-slot queue means a rapid double-recall lands the
        // newest blob and the older one is dropped - preferred to
        // the audio thread chasing stale state across blocks.
        data.pending_state.record_audio_block();
        let state_loaded = apply_queued_state(&mut *instance, &data.pending_state).is_some();

        // Paranoid allocation check (the `rt-paranoid` feature): guard the
        // wrapper's per-block glue - event conversion, transport, process,
        // output encode, snapshot publish - as well as the plugin. Placed
        // after the state-load apply above, since `load_state` legitimately
        // allocates. No-op and zero-sized when the feature is off.
        let _rt = RtSection::enter();

        // One reborrow so the disjoint scratch fields (event list, slices,
        // output events, build scratch) can be borrowed simultaneously -
        // the guard's `Deref` can't split-borrow.
        let scr = &mut *audio;

        // Convert CLAP input events to our EventList - sort by
        // sample offset so the plugin sees them in time order.
        // `state_loaded` causes ParamValue/ParamMod events to be
        // dropped because they predate the state-load intent.
        convert_input_events::<P>(
            scr,
            &data.info,
            proc.in_events,
            true,
            state_loaded,
            Some(proc.frames_count),
        );

        // Track polyphonic modulation ranges and wake any open 0 Hz editor on
        // host automation or modulation events (RT-safe: pure atomics, zero allocs).
        let mut wake_editor = false;
        let mut saw_poly_mod = false;
        for event in scr.event_list.iter() {
            match event.body {
                EventBody::ParamChange { .. } => {
                    wake_editor = true;
                }
                EventBody::ParamMod { id, note_id, value } => {
                    if note_id >= 0 {
                        if !saw_poly_mod {
                            data.feedback.begin_poly_mod_block();
                            saw_poly_mod = true;
                        }
                        data.feedback.record_poly_mod(id, value);
                    }
                    wake_editor = true;
                }
                _ => {}
            }
        }
        if !saw_poly_mod
            && scr.sounding_notes.voices.is_empty()
            && scr.sounding_notes.complete
            && data.feedback.clear_poly_mod()
        {
            wake_editor = true;
        }
        if wake_editor && data.feedback.has_waker() {
            data.host_queue.signal(HostSignal::EDITOR_WAKE);
        }

        // Build transport info from the CLAP transport event (or default).
        let transport = if proc.transport.is_null() {
            TransportInfo::default()
        } else {
            build_transport_info(&*proc.transport, scr.sample_rate)
        };

        // Build AudioBuffer from CLAP audio buffers.
        //
        // Three soundness considerations matching the format-wrapper
        // pattern in `RawBufferScratch::build`:
        //
        // 1. **No per-block heap allocation.** We reuse `data.input_slices`
        //    and `data.output_slices` (cleared each call) so the audio
        //    thread doesn't `Vec::new()` per process.
        // 2. **Channel indexing preserved.** A null channel pointer
        //    becomes an empty slice at the same flat-channel index
        //    rather than being dropped; densifying the channel list
        //    would silently re-map indices when only some channels
        //    were null.
        // 3. **No auto input→output copy.** Plugins that want
        //    pass-through must do `output.copy_from_slice(input)`
        //    themselves; auto-copying clobbers the previous-block tail
        //    that delay/reverb feedback paths read back from the output.
        // Build per-channel slices preserving channel index across
        // every bus. A null bus (neither data pointer set) emits
        // empty slices for each of its declared channels rather than
        // being skipped - skipping would shift downstream buses'
        // channel indices and silently re-route audio onto the wrong
        // bus for multi-bus plugins.
        //
        // The host picks the wire precision per port: `data64` is
        // only set on ports that advertised
        // `CLAP_AUDIO_PORT_SUPPORTS_64BITS` (f64 plugins). When the
        // wire matches `P::Sample`, slices point straight at host
        // memory (zero-copy); otherwise the channel is converted
        // through the matching `input_widen` / `output_narrow` slot
        // and copied back after `process()` returns.
        scr.input_slices.clear();
        // Reset each inner scratch buffer's length to 0 (preserves
        // its heap allocation), don't `.clear()` the outer
        // `Vec<Vec<_>>` - that would drop every inner Vec and force
        // the per-channel push below to re-allocate every block,
        // defeating the activate-time pre-grow.
        for buf in &mut scr.input_widen {
            buf.clear();
        }
        let mut flat_in_idx = 0usize;
        let bus_routing = scr.bus_routing;
        for bus_idx in 0..proc.audio_inputs_count {
            let buf = &*proc.audio_inputs.add(bus_idx as usize);
            let bus_is_f64 = !buf.data64.is_null();
            for ch in 0..buf.channel_count {
                let scratch = &mut scr.input_widen[flat_in_idx];
                let slice: &'static [P::Sample] = if bus_is_f64 {
                    let host_ptr: *const f64 = *buf.data64.add(ch as usize);
                    input_channel_slice::<P::Sample, f64>(
                        host_ptr,
                        num_frames,
                        scratch,
                        input_overlaps_outputs(
                            host_ptr.cast(),
                            num_frames * size_of::<f64>(),
                            proc,
                        ),
                    )
                } else if buf.data32.is_null() {
                    // Whole input port disconnected (null channel-pointer
                    // array): feed this channel block-length silence.
                    input_channel_slice::<P::Sample, f32>(
                        std::ptr::null(),
                        num_frames,
                        scratch,
                        false,
                    )
                } else {
                    let host_ptr: *const f32 = *buf.data32.add(ch as usize);
                    input_channel_slice::<P::Sample, f32>(
                        host_ptr,
                        num_frames,
                        scratch,
                        input_overlaps_outputs(
                            host_ptr.cast(),
                            num_frames * size_of::<f32>(),
                            proc,
                        ),
                    )
                };
                scr.input_slices.push(slice);
                flat_in_idx += 1;
            }
        }

        scr.output_slices.clear();
        // Same reasoning as `input_widen` above: clear each inner
        // scratch buffer in place so its heap allocation survives.
        for buf in &mut scr.output_narrow {
            buf.clear();
        }
        scr.host_out_ptrs.clear();
        let mut flat_out_idx = 0usize;
        for bus_idx in 0..proc.audio_outputs_count {
            let buf = &mut *proc.audio_outputs.add(bus_idx as usize);
            let bus_is_f64 = !buf.data64.is_null();
            for ch in 0..buf.channel_count {
                let scratch = &mut scr.output_narrow[flat_out_idx];
                let slice: &'static mut [P::Sample] = if bus_is_f64 {
                    let host_ptr: *mut f64 = *buf.data64.add(ch as usize);
                    scr.host_out_ptrs.push(if host_ptr.is_null() {
                        HostOutPtr::Null
                    } else {
                        HostOutPtr::F64(host_ptr)
                    });
                    output_channel_slice::<P::Sample, f64>(host_ptr, num_frames, scratch)
                } else {
                    let host_ptr: *mut f32 = if buf.data32.is_null() {
                        std::ptr::null_mut()
                    } else {
                        *buf.data32.add(ch as usize)
                    };
                    scr.host_out_ptrs.push(if host_ptr.is_null() {
                        HostOutPtr::Null
                    } else {
                        HostOutPtr::F32(host_ptr)
                    });
                    output_channel_slice::<P::Sample, f32>(host_ptr, num_frames, scratch)
                };
                scr.output_slices.push(slice);
                flat_out_idx += 1;
            }
        }

        // Construct the AudioBuffer with a borrow scope tied to this
        // call only. Without the transmute, the borrow checker
        // propagates the `'static` lifetimes inside `input_slices`
        // out to the AudioBuffer's lifetime parameter - which would
        // pin the scratch mutably for the rest of the function. Same
        // pattern as `RawBufferScratch::build`.
        let scr_ptr: *mut ClapAudio<P> = scr;
        let s = &mut *scr_ptr;
        let mut audio_buffer =
            transmute::<AudioBuffer<'static, P::Sample>, AudioBuffer<'_, P::Sample>>(
                AudioBuffer::from_slices(&s.input_slices, &mut s.output_slices, num_frames),
            );

        scr.output_events.clear();
        scr.output_events.clear_overflow();

        // Publish transport to the editor slot before the plugin runs.
        data.transport_slot.write(&transport);

        // Sample-accurate parameter chunking: split the audio block
        // at every chunkable param-change / transport event, deferring
        // the `set_plain` apply to the sub-block boundary the event
        // sits on. On blocks with no chunkable events this runs
        // `plugin.process` exactly once - the splitting machinery is
        // inert. See `chunked_process` for the per-sub-block contract.
        let mut transport_snap = transport;
        let request_latency_change = || data.host_queue.signal(HostSignal::LATENCY);
        let chunk_args = ChunkedProcess {
            events: &scr.event_list,
            sub_event_scratch: &mut scr.sub_event_scratch,
            transport: &mut transport_snap,
            sample_rate: scr.sample_rate,
            process_mode: ProcessMode::from_u8(data.render_mode.load(Ordering::Relaxed)),
            output_events: &mut scr.output_events,
            params_fn: None,
            meters_fn: None,
            latency_change_request: Some(&request_latency_change),
            param_infos: &scr.catalog,
            automation: data.info.automation,
        };
        let thread_pool_exec = |num_tasks: u32, f: &(dyn Fn(u32) + Sync)| -> bool {
            let ext = data.host_thread_pool;
            if ext.is_null() {
                return false;
            }
            let Some(request_exec) = (*ext).request_exec else {
                return false;
            };
            let host_ptr = data.host.as_ptr();
            if host_ptr.is_null() {
                return false;
            }
            let task = PoolTask { f };
            let ptr = (&raw const task).cast_mut().cast::<c_void>();
            if data
                .active_pool_task
                .compare_exchange(ptr::null_mut(), ptr, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                return false;
            }
            let _clear = ClearPoolTask(&data.active_pool_task);
            request_exec(host_ptr, num_tasks)
        };
        let pool_arg: Option<moose_core::process::ThreadPoolExec<'_>> =
            (!chunk_args.process_mode.is_offline() && !data.host_thread_pool.is_null())
                .then_some(&thread_pool_exec);
        let status = process_chunked_with_bus_routing_catalog_and_pool(
            &mut *instance,
            data.params_arc.as_ref() as &dyn Params,
            &mut audio_buffer,
            chunk_args,
            bus_routing,
            &scr.catalog,
            pool_arg,
        );
        if data.panic_latch.is_failed() {
            // A host thread-pool worker panicked during `parallel_for`: do not
            // emit partially written scratch buffers or audio; return
            // `CLAP_PROCESS_ERROR` so `zero_clap_output_buffers` silences this
            // block and `request_callback` reports the latched panic.
            scr.input_slices.clear();
            scr.output_slices.clear();
            data.host.on_audio_thread().as_any().request_callback();
            return CLAP_PROCESS_ERROR;
        }

        // Parameters have reached their sample offsets before discovery is
        // scheduled. No metadata lookup or catalog walk runs on audio.
        request_changed_presentation(data, &scr.event_list);

        // Convert + copy back to host outputs for every channel that
        // rendered into scratch (wire precision differed from
        // `P::Sample`). Zero-copy channels leave their scratch slot
        // empty and are skipped - the plugin already wrote host
        // memory. `zip` over the two vectors instead of indexing -
        // if either is shorter (it shouldn't be, but a future drift
        // would hit this), iteration stops at the min cleanly rather
        // than panicking on an out-of-bounds index.
        for (host_ptr, plugin) in scr.host_out_ptrs.iter().zip(scr.output_narrow.iter()) {
            if plugin.is_empty() {
                continue;
            }
            match *host_ptr {
                HostOutPtr::Null => {}
                HostOutPtr::F32(p) => {
                    let host = std::slice::from_raw_parts_mut(p, num_frames);
                    for (h, &s) in host.iter_mut().zip(plugin.iter()) {
                        *h = s.to_f32();
                    }
                }
                HostOutPtr::F64(p) => {
                    let host = std::slice::from_raw_parts_mut(p, num_frames);
                    for (h, &s) in host.iter_mut().zip(plugin.iter()) {
                        *h = s.to_f64();
                    }
                }
            }
        }

        // Latency cannot change while active (`clap.latency`), so signal the
        // main thread to request a restart; the new latency publishes on the
        // next `activate`. Tail changes are `[audio-thread]` in CLAP 1.2
        // (`clap/ext/tail.h`): publish atomically and notify `clap.tail.changed`
        // directly without forcing a disruptive restart.
        data.delay
            .observe_latency_and_signal(instance.latency(), &data.host_queue);
        if data.delay.publish_tail(instance.tail()) {
            data.host.on_audio_thread().tail_changed(data.host_tail);
        }

        // GUI delivery has its own queue semantics. The public status belongs
        // to the plugin output lane, so an unrelated GUI refusal cannot hide
        // a staging BufferFull root cause.
        let _ = flush_gui_changes::<P>(data, proc.out_events);
        let mut output_status = scr
            .output_events
            .overflow()
            .map_or(OutputEventStatus::Success, OutputEventStatus::BufferFull);
        if output_status == OutputEventStatus::Success && !scr.output_events.is_empty() {
            // CLAP requires globally time-sorted output. Preflight the exact
            // sequence the host will see before allowing the first push.
            scr.output_events.ensure_sorted_by_offset();
            output_status = preflight_clap_output(
                &scr.output_events,
                &data.info,
                &scr.catalog,
                proc.frames_count,
            );
        }

        // Forward plugin output events (MIDI output from instruments/effects)
        if output_status == OutputEventStatus::Success && !scr.output_events.is_empty() {
            let try_push = if proc.out_events.is_null() {
                None
            } else {
                (*proc.out_events).try_push
            };
            let Some(try_push) = try_push else {
                scr.output_events
                    .set_output_status(OutputEventStatus::Unsupported);
                scr.input_slices.clear();
                scr.output_slices.clear();
                return match status {
                    ProcessStatus::Normal => CLAP_PROCESS_CONTINUE,
                    ProcessStatus::Tail(0) => CLAP_PROCESS_SLEEP,
                    ProcessStatus::Tail(_) => CLAP_PROCESS_TAIL,
                    ProcessStatus::KeepAlive => CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
                };
            };
            // Exact payloads are authoritative. `lossless_iter` suppresses a
            // linked typed view, and an unsupported exact representation is
            // never replayed through a lossy compatibility conversion.
            macro_rules! push_output {
                ($queue:ident, $header:expr) => {
                    $queue = try_push(proc.out_events, $header);
                    if !$queue {
                        output_status = OutputEventStatus::HostQueueFull;
                    }
                };
            }
            'output_events: for replay in scr.output_events.lossless_iter() {
                let queue_open;
                let event = match replay {
                    LosslessEventRef::Typed(event) => event,
                    LosslessEventRef::Exact(exact) => {
                        let note_end = outbound_note_end_pattern(exact, &data.info);
                        match emit_exact_clap(
                            proc.out_events,
                            try_push,
                            exact,
                            &data.info,
                            proc.frames_count,
                        ) {
                            ExactEmit::Emitted => {
                                if let Some(pattern) = note_end {
                                    scr.sounding_notes.remove_pattern(pattern);
                                }
                                continue;
                            }
                            ExactEmit::Invalid => {
                                output_status = OutputEventStatus::Invalid;
                                break 'output_events;
                            }
                            ExactEmit::Unsupported => {
                                output_status = OutputEventStatus::Unsupported;
                                break 'output_events;
                            }
                            ExactEmit::QueueFull => {
                                output_status = OutputEventStatus::HostQueueFull;
                                break 'output_events;
                            }
                        }
                    }
                };
                if event.sample_offset >= proc.frames_count {
                    output_status = OutputEventStatus::Invalid;
                    break 'output_events;
                }
                if !typed_output_body_is_valid(&event.body) {
                    output_status = OutputEventStatus::Invalid;
                    break 'output_events;
                }
                if !is_portless_output_body(&event.body)
                    && (data.info.midi_output_ports == 0
                        || event.port >= data.info.midi_output_ports)
                {
                    output_status = OutputEventStatus::Invalid;
                    break 'output_events;
                }
                let out_port = event.port;
                match &event.body {
                    EventBody::NoteOn {
                        channel,
                        note,
                        velocity,
                        ..
                    } => {
                        let ev = clap_event_note {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_note>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_NOTE_ON,
                                flags: 0,
                            },
                            note_id: -1,
                            port_index: i16::from(out_port),
                            channel: i16::from(*channel),
                            key: i16::from(*note),
                            velocity: f64::from(*velocity) / 127.0,
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    EventBody::NoteOff {
                        channel,
                        note,
                        velocity,
                        ..
                    } => {
                        let ev = clap_event_note {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_note>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_NOTE_OFF,
                                flags: 0,
                            },
                            note_id: -1,
                            port_index: i16::from(out_port),
                            channel: i16::from(*channel),
                            key: i16::from(*note),
                            velocity: f64::from(*velocity) / 127.0,
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    // CLAP carries MIDI 1.0 control / channel events as
                    // `CLAP_EVENT_MIDI` 3-byte packets. The host
                    // demuxes them on the receiving side; we just
                    // build the standard MIDI status byte and pass
                    // the data bytes through.
                    EventBody::ControlChange {
                        channel, cc, value, ..
                    } => {
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xB0 | *channel, *cc, *value],
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    EventBody::Aftertouch {
                        channel,
                        note,
                        pressure,
                        ..
                    } => {
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xA0 | *channel, *note, *pressure],
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    EventBody::ChannelPressure {
                        channel, pressure, ..
                    } => {
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xD0 | *channel, *pressure, 0],
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    EventBody::PitchBend { channel, value, .. } => {
                        let (lsb, msb) = pitch_bend_to_bytes(*value);
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xE0 | *channel, lsb, msb],
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    EventBody::ProgramChange {
                        channel, program, ..
                    } => {
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xC0 | *channel, *program, 0],
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    EventBody::ParamChange { id, value } => {
                        // CLAP params are global, not tied to a specific
                        // note/audio port - every key uses the `-1`
                        // wildcard so hosts that route automation by
                        // port_index match GUI-driven and process-driven
                        // param events on the same key. The
                        // `flush_gui_changes` path uses the same shape.
                        let ev = clap_event_param_value {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_param_value>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_PARAM_VALUE,
                                flags: 0,
                            },
                            param_id: *id,
                            cookie: scr.catalog.index_of(*id).map_or(ptr::null_mut(), |idx| {
                                ptr::without_provenance_mut(
                                    moose_params::ParamCatalog::cookie_for_index(idx),
                                )
                            }),
                            note_id: -1,
                            port_index: -1,
                            channel: -1,
                            key: -1,
                            value: *value,
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    EventBody::SysEx { .. } => {
                        // CLAP's contract for the output direction
                        // is narrower than the input's: the
                        // `buffer` pointer needs to stay valid for
                        // the duration of the `try_push` call only
                        // (the host is expected to copy before
                        // returning). Our pool is cleared at the
                        // *start* of the next block, so pointing
                        // the host at `pool_offset` satisfies that
                        // strictly - and any future host that
                        // defers the copy until later in
                        // `process()` is still fine because the
                        // pool stays valid through the whole block.
                        let Some(bytes) = scr.output_events.sysex_bytes_checked(&event.body) else {
                            output_status = OutputEventStatus::Invalid;
                            break 'output_events;
                        };
                        let ev = clap_event_midi_sysex {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi_sysex>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI_SYSEX,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            buffer: bytes.as_ptr(),
                            size: len_u32(bytes.len()),
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    // Typed MIDI 2.0 remains native UMP. The strict domain
                    // check above makes the shared encoder's masks inert, so
                    // group, attributes, banks, controllers, and wide values
                    // reach the host unchanged.
                    body @ (EventBody::NoteOn2 { .. }
                    | EventBody::NoteOff2 { .. }
                    | EventBody::PolyPressure2 { .. }
                    | EventBody::PerNoteCC { .. }
                    | EventBody::PerNotePitchBend { .. }
                    | EventBody::PerNoteManagement { .. }
                    | EventBody::ControlChange2 { .. }
                    | EventBody::ChannelPressure2 { .. }
                    | EventBody::PitchBend2 { .. }
                    | EventBody::ProgramChange2 { .. }
                    | EventBody::RegisteredController { .. }
                    | EventBody::AssignableController { .. }) => {
                        if data.info.midi_output_dialect != MidiDialect::Midi2 {
                            output_status = OutputEventStatus::Unsupported;
                            break 'output_events;
                        }
                        let Some(words) = encode_ump_channel_voice_2(body) else {
                            output_status = OutputEventStatus::Invalid;
                            break 'output_events;
                        };
                        let ev = clap_event_midi2 {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi2>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI2,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: words,
                        };
                        push_output!(queue_open, &raw const ev.header);
                    }
                    EventBody::ParamMod { .. }
                    | EventBody::ParamRamp { .. }
                    | EventBody::Transport(_) => {
                        output_status = OutputEventStatus::Unsupported;
                        break 'output_events;
                    }
                }
                if !queue_open {
                    break 'output_events;
                }
            }
        }
        scr.output_events.set_output_status(output_status);

        // Drop the channel-slice borrows we transmuted to `'static`
        // for the duration of this call. The host's `audio_inputs` /
        // `audio_outputs` buffers may be reused or freed once
        // `clap_plugin_process` returns; leaving the slice scratch
        // populated would pin dangling pointers across blocks. Clear
        // does NOT shrink capacity, so the next block reuses the
        // same allocation without touching the global allocator.
        scr.input_slices.clear();
        scr.output_slices.clear();

        match status {
            ProcessStatus::Normal => CLAP_PROCESS_CONTINUE,
            ProcessStatus::Tail(0) => CLAP_PROCESS_SLEEP,
            ProcessStatus::Tail(_) => CLAP_PROCESS_TAIL,
            ProcessStatus::KeepAlive => CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
        }
    });
    let result = result.unwrap_or_else(|| {
        // First panic: print the latched message from the main thread.
        // SAFETY: the wrapper retains the host for the instance's lifetime;
        // `request_callback` is thread-safe (CLAP host.h).
        unsafe {
            data_from_plugin::<P>(plugin)
                .host
                .on_audio_thread()
                .as_any()
                .request_callback();
        }
        CLAP_PROCESS_ERROR
    });
    // A mid-process panic is caught by `run_audio_block_latched`, which latches and returns
    // the `CLAP_PROCESS_ERROR` fallback - but the plugin may have left the
    // output buffers half-written. Zero them so the host doesn't play stale
    // garbage for a block before it reacts to the error, matching VST3 / VST2
    // / AAX. An invalid host shape has no validated storage to clear.
    if result == CLAP_PROCESS_ERROR && audio_buffers_valid {
        unsafe { zero_clap_output_buffers(process) };
    }
    result
}

/// Zero every output-buffer channel in `process` (both `f32` and `f64` wires).
/// Used on the process error path so a caught panic doesn't leak stale samples
/// into the host's speakers.
///
/// # Safety
/// `process`, when non-null, must be a valid `clap_process` whose output
/// buffers are sized to `frames_count`.
pub(crate) unsafe fn zero_clap_output_buffers(process: *const clap_process) {
    if process.is_null() {
        return;
    }
    unsafe {
        let proc = &*process;
        let num_frames = proc.frames_count as usize;
        for bus_idx in 0..proc.audio_outputs_count {
            let buf = &*proc.audio_outputs.add(bus_idx as usize);
            let ch_count = buf.channel_count as usize;
            if !buf.data32.is_null() {
                for ch in 0..ch_count {
                    let ptr = *buf.data32.add(ch);
                    if !ptr.is_null() {
                        std::ptr::write_bytes(ptr, 0, num_frames);
                    }
                }
            } else if !buf.data64.is_null() {
                for ch in 0..ch_count {
                    let ptr = *buf.data64.add(ch);
                    if !ptr.is_null() {
                        std::ptr::write_bytes(ptr, 0, num_frames);
                    }
                }
            }
        }
    }
}

/// Test-only smoke helper for the `rt-paranoid` CI gate: drives a few
/// real CLAP `process` callbacks through this wrapper's per-block glue
/// (event conversion, transport, `process_chunked`, output narrow,
/// snapshot publish) via the full plugin vtable, and returns the
/// audio-thread allocation count of a steady-state block (0 = clean).
/// Small stereo buffers, no input events. Vacuously 0 unless the
/// `rt-paranoid` feature installs the checking allocator. Not public API.
#[doc(hidden)]
#[must_use]
pub fn rt_paranoid_smoke<P: PluginExport>() -> u32 {
    const FRAMES: u32 = 512;
    const CHANNELS: u32 = 2;

    // `clap_plugin_init` queries the params extension through the host's
    // `get_extension`, so that pointer must be a live stub; the rest of
    // the host callbacks go unused on this path.
    unsafe extern "C" fn no_extension(
        _host: *const clap_host,
        _id: *const c_char,
    ) -> *const c_void {
        ptr::null()
    }
    // No-op output-event sink. The wrapper null-checks `out_events` and
    // only calls `try_push` when the plugin emits events (this passthrough
    // does not), but a live sink keeps the harness honest if that changes.
    unsafe extern "C" fn no_push(
        _list: *const clap_output_events,
        _event: *const clap_event_header,
    ) -> bool {
        true
    }

    let frames = FRAMES as usize;
    // Leaked so their addresses outlive the plugin instance below.
    let descriptor: &'static clap_plugin_descriptor = Box::leak(Box::new(clap_plugin_descriptor {
        clap_version: CLAP_VERSION,
        id: ptr::null(),
        name: ptr::null(),
        vendor: ptr::null(),
        url: ptr::null(),
        manual_url: ptr::null(),
        support_url: ptr::null(),
        version: ptr::null(),
        description: ptr::null(),
        features: ptr::null(),
    }));
    let host: &'static clap_host = Box::leak(Box::new(clap_host {
        clap_version: CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: ptr::null(),
        vendor: ptr::null(),
        url: ptr::null(),
        version: ptr::null(),
        get_extension: Some(no_extension),
        request_restart: None,
        request_process: None,
        request_callback: None,
    }));

    // SAFETY: constructs, drives, and destroys its own instance through
    // the plugin vtable. Every pointer handed to `process` (audio
    // buffers, the channel-pointer arrays, the output-event sink)
    // outlives each call, and the buffers are sized to `FRAMES`.
    unsafe {
        let plugin = create_plugin_instance::<P>(descriptor, host);
        let vtable = &*plugin;
        (vtable.init.unwrap())(plugin);
        (vtable.activate.unwrap())(plugin, 48_000.0, 1, FRAMES);
        (vtable.start_processing.unwrap())(plugin);

        // Non-zero constant input so a working passthrough-times-gain
        // renders non-zero output; the assert below fails loudly if a
        // regressed harness never actually ran `process`. CLAP's
        // `clap_audio_buffer::data32` is `*mut *mut f32`, so the input
        // storage is mutable too even though the wrapper only reads it.
        let mut in_left = vec![0.5f32; frames];
        let mut in_right = vec![0.5f32; frames];
        let mut out_left = vec![0f32; frames];
        let mut out_right = vec![0f32; frames];
        let mut in_ptrs: [*mut f32; 2] = [in_left.as_mut_ptr(), in_right.as_mut_ptr()];
        let mut out_ptrs: [*mut f32; 2] = [out_left.as_mut_ptr(), out_right.as_mut_ptr()];

        let input_bus = clap_audio_buffer {
            data32: in_ptrs.as_mut_ptr(),
            data64: ptr::null_mut(),
            channel_count: CHANNELS,
            latency: 0,
            constant_mask: 0,
        };
        let mut output_bus = clap_audio_buffer {
            data32: out_ptrs.as_mut_ptr(),
            data64: ptr::null_mut(),
            channel_count: CHANNELS,
            latency: 0,
            constant_mask: 0,
        };
        let sink = clap_output_events {
            ctx: ptr::null_mut(),
            try_push: Some(no_push),
        };
        let process = clap_process {
            steady_time: 0,
            frames_count: FRAMES,
            transport: ptr::null(),
            audio_inputs: &raw const input_bus,
            audio_outputs: &raw mut output_bus,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: ptr::null(),
            out_events: &raw const sink,
        };

        let mut count = 0;
        // A few blocks so any legitimate first-block warmup is behind us;
        // the last block is the steady-state measurement.
        for _ in 0..3 {
            let ((), n) = audit(|| {
                (vtable.process.unwrap())(plugin, &raw const process);
            });
            count = n;
        }

        (vtable.stop_processing.unwrap())(plugin);
        (vtable.deactivate.unwrap())(plugin);
        (vtable.destroy.unwrap())(plugin);

        // A no-op harness (process never ran) leaves the output silent;
        // a working passthrough-times-gain does not.
        debug_assert!(
            out_left
                .iter()
                .chain(out_right.iter())
                .any(|s| s.abs() > 0.0),
            "CLAP smoke harness produced silent output - process did not run"
        );
        count
    }
}

#[cfg(test)]
mod thread_pool_tests {
    use super::*;
    use clap_sys::ext::thread_pool::{
        CLAP_EXT_THREAD_POOL, clap_host_thread_pool, clap_plugin_thread_pool,
    };
    use moose_core::bus::BusLayout;
    use moose_core::events::EventList;
    use moose_core::info::{Automation, MidiDialect, PluginCategory, PluginInfo};
    use moose_core::meters::MeterStore;
    use moose_core::plugin::PluginRuntime;
    use moose_core::process::ProcessContext;
    use moose_core::snapshot::SnapshotSlot;
    use moose_params::DynamicParams;
    use std::ffi::CStr;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    static PARALLEL_SUM: AtomicU32 = AtomicU32::new(0);

    struct PoolPlugin {
        params: Arc<DynamicParams<1>>,
        meters: Arc<MeterStore>,
        snapshot: Arc<SnapshotSlot>,
    }

    impl PluginRuntime for PoolPlugin {
        type Sample = f32;
        fn info() -> PluginInfo {
            PluginInfo {
                name: "PoolTest",
                vendor: "Matari",
                url: "",
                version: "1.0.0",
                category: PluginCategory::Effect,
                description: None,
                clap_manual_url: None,
                clap_support_url: None,
                clap_features: &[],
                accepts_midi_in: false,
                emits_midi: false,
                midi_input_dialect: MidiDialect::Midi1,
                midi_output_dialect: MidiDialect::Midi1,
                midi_input_ports: 0,
                midi_output_ports: 0,
                bundle_id: "pool-test",
                vst3_id: "com.matari.pooltest",
                clap_id: "com.matari.pooltest",
                fourcc: *b"PlTs",
                au_type: *b"aufx",
                au_manufacturer: *b"Mtri",
                vst3_subcategory: None,
                preset_user_dir: None,
                preset_extension: "preset",
                vst3_name: None,
                clap_name: None,
                au_name: None,
                au3_name: None,
                mute_preview_output: false,
                automation: Automation::DEFAULT,
                legacy_au_keys: &[],
                legacy_aax_chunks: &[],
            }
        }
        fn bus_layouts() -> Vec<BusLayout> {
            vec![BusLayout::stereo()]
        }
        fn reset(&mut self, _: &moose_core::AudioConfig) {}
        fn process(
            &mut self,
            _buffer: &mut AudioBuffer<f32>,
            _events: &EventList,
            ctx: &mut ProcessContext<'_>,
        ) -> ProcessStatus {
            ctx.parallel_for(8, |i| {
                PARALLEL_SUM.fetch_add(i + 1, Ordering::Relaxed);
            });
            ProcessStatus::Normal
        }
    }

    impl PluginExport for PoolPlugin {
        type Params = DynamicParams<1>;
        fn create() -> Self {
            Self {
                params: Arc::new(DynamicParams::new()),
                meters: MeterStore::new(),
                snapshot: SnapshotSlot::new(),
            }
        }
        fn params(&self) -> &DynamicParams<1> {
            &self.params
        }
        fn params_arc(&self) -> Arc<DynamicParams<1>> {
            Arc::clone(&self.params)
        }
        fn meter_store(&self) -> Arc<MeterStore> {
            Arc::clone(&self.meters)
        }
        fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
            Arc::clone(&self.snapshot)
        }
    }

    struct HostState {
        plugin: *const clap_plugin,
        plugin_pool: *const clap_plugin_thread_pool,
        requests: AtomicU32,
    }

    unsafe extern "C" fn host_request_exec(host: *const clap_host, num_tasks: u32) -> bool {
        unsafe {
            let state = &*(*host).host_data.cast::<HostState>();
            state.requests.fetch_add(1, Ordering::Relaxed);
            let exec = (*state.plugin_pool).exec.unwrap();
            let plugin_addr = state.plugin as usize;
            std::thread::scope(|s| {
                for i in 0..num_tasks {
                    s.spawn(move || {
                        exec(plugin_addr as *const clap_plugin, i);
                    });
                }
            });
        }
        true
    }

    static HOST_POOL: clap_host_thread_pool = clap_host_thread_pool {
        request_exec: Some(host_request_exec),
    };

    unsafe extern "C" fn host_get_ext(
        _host: *const clap_host,
        ext_id: *const c_char,
    ) -> *const c_void {
        if unsafe { CStr::from_ptr(ext_id) } == CLAP_EXT_THREAD_POOL {
            (&raw const HOST_POOL).cast()
        } else {
            ptr::null()
        }
    }

    unsafe extern "C" fn nop_host(_: *const clap_host) {}

    #[test]
    fn clap_thread_pool_executes_tasks_across_host_workers() {
        PARALLEL_SUM.store(0, Ordering::Relaxed);

        let desc = clap_plugin_descriptor {
            clap_version: CLAP_VERSION,
            id: c"com.matari.pooltest".as_ptr(),
            name: c"PoolTest".as_ptr(),
            vendor: c"Matari".as_ptr(),
            url: c"".as_ptr(),
            manual_url: c"".as_ptr(),
            support_url: c"".as_ptr(),
            version: c"1.0.0".as_ptr(),
            description: c"".as_ptr(),
            features: [ptr::null()].as_ptr(),
        };
        let host_state = std::cell::UnsafeCell::new(HostState {
            plugin: ptr::null(),
            plugin_pool: ptr::null(),
            requests: AtomicU32::new(0),
        });
        let host = clap_host {
            clap_version: CLAP_VERSION,
            host_data: host_state.get().cast(),
            name: c"TestHost".as_ptr(),
            vendor: c"".as_ptr(),
            url: c"".as_ptr(),
            version: c"".as_ptr(),
            get_extension: Some(host_get_ext),
            request_restart: Some(nop_host),
            request_process: Some(nop_host),
            request_callback: Some(nop_host),
        };

        unsafe {
            let plugin = create_plugin_instance::<PoolPlugin>(&raw const desc, &raw const host);
            assert!(!plugin.is_null());
            let vtable = &*plugin;
            assert!((vtable.init.unwrap())(plugin));

            let ext = (vtable.get_extension.unwrap())(plugin, CLAP_EXT_THREAD_POOL.as_ptr())
                .cast::<clap_plugin_thread_pool>();
            assert!(!ext.is_null());
            (*host_state.get()).plugin = plugin;
            (*host_state.get()).plugin_pool = ext;

            // Stray `exec` outside `request_exec` is a safe no-op.
            ((*ext).exec.unwrap())(plugin, 0);
            assert_eq!(PARALLEL_SUM.load(Ordering::Relaxed), 0);

            assert!((vtable.activate.unwrap())(plugin, 48_000.0, 1, 64));
            assert!((vtable.start_processing.unwrap())(plugin));

            let mut in_l = [0.0f32; 16];
            let mut in_r = [0.0f32; 16];
            let mut out_l = [0.0f32; 16];
            let mut out_r = [0.0f32; 16];
            let mut in_ptrs = [in_l.as_mut_ptr(), in_r.as_mut_ptr()];
            let mut out_ptrs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
            let in_bus = clap_audio_buffer {
                data32: in_ptrs.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let mut out_bus = clap_audio_buffer {
                data32: out_ptrs.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let process = clap_process {
                steady_time: 0,
                frames_count: 16,
                transport: ptr::null(),
                audio_inputs: &raw const in_bus,
                audio_outputs: &raw mut out_bus,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: ptr::null(),
                out_events: ptr::null(),
            };

            let status = (vtable.process.unwrap())(plugin, &raw const process);
            assert_eq!(status, CLAP_PROCESS_CONTINUE);
            assert_eq!((*host_state.get()).requests.load(Ordering::Relaxed), 1);
            assert_eq!(PARALLEL_SUM.load(Ordering::Relaxed), 36);

            // Pointer is cleared immediately after `request_exec` returns.
            ((*ext).exec.unwrap())(plugin, 0);
            assert_eq!(PARALLEL_SUM.load(Ordering::Relaxed), 36);

            assert!(crate::gui::render_set::<PoolPlugin>(
                plugin,
                clap_sys::ext::render::CLAP_RENDER_OFFLINE
            ));
            assert_eq!(
                (vtable.process.unwrap())(plugin, &raw const process),
                CLAP_PROCESS_CONTINUE
            );
            assert_eq!((*host_state.get()).requests.load(Ordering::Relaxed), 1);
            assert_eq!(PARALLEL_SUM.load(Ordering::Relaxed), 72);

            (vtable.stop_processing.unwrap())(plugin);
            (vtable.deactivate.unwrap())(plugin);
            (vtable.destroy.unwrap())(plugin);
        }
    }

    #[test]
    fn clap_thread_pool_worker_panic_latches_and_silences_outputs() {
        struct PanicPoolPlugin(PoolPlugin);
        impl PluginRuntime for PanicPoolPlugin {
            type Sample = f32;
            fn info() -> PluginInfo {
                PoolPlugin::info()
            }
            fn bus_layouts() -> Vec<BusLayout> {
                PoolPlugin::bus_layouts()
            }
            fn reset(&mut self, config: &moose_core::AudioConfig) {
                self.0.reset(config);
            }
            fn process(
                &mut self,
                buffer: &mut AudioBuffer<f32>,
                _events: &EventList,
                context: &mut ProcessContext<'_>,
            ) -> ProcessStatus {
                // Pre-fill outputs with non-zero sentinel samples so the test
                // proves a worker panic zeroes the block rather than leaking
                // partial samples.
                for ch in 0..buffer.num_output_channels() {
                    buffer.output(ch).fill(0.75);
                }
                context.parallel_for(4, |idx| {
                    assert!(idx != 2, "worker task exploded");
                });
                ProcessStatus::Normal
            }
        }
        impl PluginExport for PanicPoolPlugin {
            type Params = DynamicParams<1>;
            fn create() -> Self {
                Self(PoolPlugin::create())
            }
            fn params(&self) -> &Self::Params {
                self.0.params()
            }
            fn params_arc(&self) -> Arc<Self::Params> {
                self.0.params_arc()
            }
            fn meter_store(&self) -> Arc<MeterStore> {
                self.0.meter_store()
            }
            fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
                self.0.snapshot_slot()
            }
        }

        let desc = clap_plugin_descriptor {
            clap_version: CLAP_VERSION,
            id: c"com.matari.panicpooltest".as_ptr(),
            name: c"PanicPoolTest".as_ptr(),
            vendor: c"Matari".as_ptr(),
            url: c"".as_ptr(),
            manual_url: c"".as_ptr(),
            support_url: c"".as_ptr(),
            version: c"1.0.0".as_ptr(),
            description: c"".as_ptr(),
            features: [ptr::null()].as_ptr(),
        };
        let host_state = std::cell::UnsafeCell::new(HostState {
            plugin: ptr::null(),
            plugin_pool: ptr::null(),
            requests: AtomicU32::new(0),
        });
        let host = clap_host {
            clap_version: CLAP_VERSION,
            host_data: host_state.get().cast(),
            name: c"TestHost".as_ptr(),
            vendor: c"".as_ptr(),
            url: c"".as_ptr(),
            version: c"".as_ptr(),
            get_extension: Some(host_get_ext),
            request_restart: Some(nop_host),
            request_process: Some(nop_host),
            request_callback: Some(nop_host),
        };

        unsafe {
            let plugin =
                create_plugin_instance::<PanicPoolPlugin>(&raw const desc, &raw const host);
            assert!(!plugin.is_null());
            let vtable = &*plugin;
            assert!((vtable.init.unwrap())(plugin));
            let ext = (vtable.get_extension.unwrap())(plugin, CLAP_EXT_THREAD_POOL.as_ptr())
                .cast::<clap_plugin_thread_pool>();
            assert!(!ext.is_null());
            (*host_state.get()).plugin = plugin;
            (*host_state.get()).plugin_pool = ext;

            assert!((vtable.activate.unwrap())(plugin, 48_000.0, 1, 64));
            assert!((vtable.start_processing.unwrap())(plugin));

            let mut in_l = [0.5f32; 16];
            let mut in_r = [0.5f32; 16];
            let mut out_l = [1.0f32; 16];
            let mut out_r = [1.0f32; 16];
            let mut in_ptrs = [in_l.as_mut_ptr(), in_r.as_mut_ptr()];
            let mut out_ptrs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
            let in_bus = clap_audio_buffer {
                data32: in_ptrs.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let mut out_bus = clap_audio_buffer {
                data32: out_ptrs.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let process = clap_process {
                steady_time: 0,
                frames_count: 16,
                transport: ptr::null(),
                audio_inputs: &raw const in_bus,
                audio_outputs: &raw mut out_bus,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: ptr::null(),
                out_events: ptr::null(),
            };

            // Worker panic latches, returns error, and zeroes output buffers.
            assert_eq!(
                (vtable.process.unwrap())(plugin, &raw const process),
                CLAP_PROCESS_ERROR
            );
            assert!(
                data_from_plugin::<PanicPoolPlugin>(plugin)
                    .panic_latch
                    .is_failed()
            );
            assert!(out_l.iter().all(|&s| s == 0.0));
            assert!(out_r.iter().all(|&s| s == 0.0));

            // Subsequent block stays latched & silent without invoking process().
            out_l.fill(0.5);
            out_r.fill(0.5);
            assert_eq!(
                (vtable.process.unwrap())(plugin, &raw const process),
                CLAP_PROCESS_SLEEP
            );
            assert!(out_l.iter().all(|&s| s == 0.0));
            assert!(out_r.iter().all(|&s| s == 0.0));

            (vtable.stop_processing.unwrap())(plugin);
            (vtable.deactivate.unwrap())(plugin);
            (vtable.destroy.unwrap())(plugin);
        }
    }
}
