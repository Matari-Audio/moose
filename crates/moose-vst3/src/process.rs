//! The audio process callbacks, latency and tail.

use crate::ffi;
use crate::input_events::cb_begin_input_events;
use crate::instance::Vst3Instance;
use crate::lifecycle::{cb_create, cb_destroy, cb_prepare, vst3_process_mode};

use moose_core::bus_routing::{BusActivation, BusRouting, MAX_AUDIO_BUSES};
use moose_core::cast::{len_u32, sample_pos_i64};
use moose_core::chunked_process::{ChunkedProcess, process_chunked_with_bus_routing_and_catalog};
use moose_core::events::TransportInfo;
use moose_core::export::PluginExport;
use moose_core::host_queue::HostSignal;
use moose_core::rt::{RtSection, audit};
use moose_core::wrapper::{apply_queued_state, run_audio_block_latched};
use moose_params::Params;
use moose_params::sample::Sample;
use std::slice;
use std::sync::atomic::Ordering;

pub(crate) unsafe extern "C" fn cb_process<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const std::ffi::c_void,
    outputs: *mut *mut std::ffi::c_void,
    num_input_channels: u32,
    num_output_channels: u32,
    input_bus_channels: *const u32,
    num_input_buses: u32,
    input_bus_active: u32,
    output_bus_channels: *const u32,
    num_output_buses: u32,
    output_bus_active: u32,
    num_frames: u32,
    transport_ptr: *const ffi::Vst3Transport,
    process_mode: i32,
) -> u32 {
    // SAFETY: forwarded - the COM layer's contract is the same.
    unsafe {
        process_block::<P, f32>(
            ctx,
            inputs.cast(),
            outputs.cast(),
            num_input_channels,
            num_output_channels,
            input_bus_channels,
            num_input_buses,
            input_bus_active,
            output_bus_channels,
            num_output_buses,
            output_bus_active,
            num_frames,
            transport_ptr,
            process_mode,
        )
    }
}

/// 64-bit wire twin of [`cb_process`]. The shim routes here when the
/// host negotiated `kSample64` in `setupProcessing` (only offered for
/// `f64` plugins), so an `f64` plugin reads and writes host memory
/// directly with no widen/narrow pass.
pub(crate) unsafe extern "C" fn cb_process_f64<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const std::ffi::c_void,
    outputs: *mut *mut std::ffi::c_void,
    num_input_channels: u32,
    num_output_channels: u32,
    input_bus_channels: *const u32,
    num_input_buses: u32,
    input_bus_active: u32,
    output_bus_channels: *const u32,
    num_output_buses: u32,
    output_bus_active: u32,
    num_frames: u32,
    transport_ptr: *const ffi::Vst3Transport,
    process_mode: i32,
) -> u32 {
    // SAFETY: forwarded - the COM layer's contract is the same.
    unsafe {
        process_block::<P, f64>(
            ctx,
            inputs.cast(),
            outputs.cast(),
            num_input_channels,
            num_output_channels,
            input_bus_channels,
            num_input_buses,
            input_bus_active,
            output_bus_channels,
            num_output_buses,
            output_bus_active,
            num_frames,
            transport_ptr,
            process_mode,
        )
    }
}

/// Shared body of [`cb_process`] / [`cb_process_f64`], generic over
/// the host wire precision `H`. `RawBufferScratch` zero-copies when
/// `H` matches the plugin's `Sample` and converts through scratch
/// otherwise, so both wires work for both plugin precisions.
// The parameter list mirrors the C ABI callback signature 1:1.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(crate) unsafe fn process_block<P: PluginExport, H: Sample>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const H,
    outputs: *mut *mut H,
    num_input_channels: u32,
    num_output_channels: u32,
    input_bus_channels: *const u32,
    num_input_buses: u32,
    input_bus_active: u32,
    output_bus_channels: *const u32,
    num_output_buses: u32,
    output_bus_active: u32,
    num_frames: u32,
    transport_ptr: *const ffi::Vst3Transport,
    process_mode: i32,
) -> u32 {
    let nf = num_frames as usize;
    // SAFETY: `ctx` is this wrapper's live instance.
    let latch = unsafe { &(*ctx.cast::<Vst3Instance<P>>()).panic_latch };
    let ok = run_audio_block_latched::<P, ()>(latch, "VST3", || unsafe {
        // Shared `&Vst3Instance` (never a whole-struct `&mut`) - the audio
        // scratch is reached through its ownership cell, so a concurrent
        // host-thread `&*ctx` (param reads, GUI) can't alias us.
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let num_frames = nf;

        // Strictly wait-free audio entry: if a misbehaving host overlaps a
        // lifecycle callback with `process`, zero outputs and return instead
        // of parking the audio thread.
        let Some(mut audio) = inst.audio.try_enter() else {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, num_frames);
                }
            }
            return;
        };

        // Host called process() before setActive(true) - the plugin
        // hasn't been told its sample rate / max block size yet, so
        // running DSP would feed garbage out of un-snapped smoothers.
        // Zero outputs and bail. Same after a panic until the next
        // prepare: the plugin's state is unknown, so it isn't re-entered.
        if !audio.prepared || inst.panic_latch.is_failed() {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, num_frames);
                }
            }
            audio.event_list.clear();
            audio.output_events.clear();
            audio.output_events.clear_overflow();
            return;
        }

        // Strictly wait-free plugin entry: an uncontended `Acquire`, never
        // parking the audio thread. A host that never calls
        // `setProcessing(true)` still marks the audio thread as owner before
        // the first block.
        inst.processing.store(true, Ordering::Release);
        let Some(mut plugin) = inst.plugin.try_enter() else {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, num_frames);
                }
            }
            audio.event_list.clear();
            audio.output_events.clear();
            audio.output_events.clear_overflow();
            return;
        };

        // Apply any pending state-load before per-block work so the
        // plugin sees consistent params and extra state for the
        // entire block. See `pending_state` field comment for the
        // queue-overflow policy.
        if apply_queued_state(&mut *plugin, &inst.pending_state).is_some_and(|r| r.is_ok()) {
            // Wait-free signal; the UI thread drains it as `kParamValuesChanged`.
            inst.host_queue.signal(HostSignal::PARAM_RESCAN);
        }

        // Paranoid allocation check (the `rt-paranoid` feature): guard the
        // wrapper's per-block glue - event conversion, transport, process,
        // output encode, snapshot publish - as well as the plugin. Placed
        // after the state-load apply above, since `load_state` legitimately
        // allocates. No-op and zero-sized when the feature is off.
        let _rt = RtSection::enter();

        // One reborrow of the scratch so the disjoint fields below (event
        // list, sub-block scratch, output events, build scratch) can be
        // borrowed simultaneously - guard `Deref` can't split-borrow.
        let scr = &mut *audio;

        debug_assert_eq!(scr.input_num_frames as usize, num_frames);

        // Build AudioBuffer from raw pointers. Uses the per-instance
        // `scratch` so the audio thread doesn't heap-allocate.
        debug_assert!(
            num_frames <= scr.max_block_size,
            "host violated VST3 contract: process() got {num_frames} frames \
             but setupProcessing declared max {}",
            scr.max_block_size
        );
        let mut audio_buffer = scr.scratch.build(
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            len_u32(num_frames),
            P::supports_in_place(),
        );
        let mut bus_routing = BusRouting::new();
        if !input_bus_channels.is_null() {
            debug_assert!((num_input_buses as usize) <= MAX_AUDIO_BUSES);
            for (index, &channels) in
                slice::from_raw_parts(input_bus_channels, num_input_buses as usize)
                    .iter()
                    .enumerate()
            {
                let pushed = bus_routing.push_input(
                    channels,
                    if input_bus_active & (1_u32 << index) == 0 {
                        BusActivation::Inactive
                    } else {
                        BusActivation::Active
                    },
                );
                debug_assert!(pushed);
            }
        }
        if !output_bus_channels.is_null() {
            debug_assert!((num_output_buses as usize) <= MAX_AUDIO_BUSES);
            for (index, &channels) in
                slice::from_raw_parts(output_bus_channels, num_output_buses as usize)
                    .iter()
                    .enumerate()
            {
                let pushed = bus_routing.push_output(
                    channels,
                    if output_bus_active & (1_u32 << index) == 0 {
                        BusActivation::Inactive
                    } else {
                        BusActivation::Active
                    },
                );
                debug_assert!(pushed);
            }
        }

        // The COM layer already merged notes and parameter points by sample
        // offset, so this is normally the O(1) already-sorted check. Otherwise
        // (an unsorted host list, a `PerSample` ramp starting before earlier
        // events) it runs an allocation-free `sort_unstable_by_key` on
        // `(sample_offset, push sequence)`: the unique sequence keeps
        // equal-offset events in push order, as a stable sort would.
        scr.event_list.ensure_sorted_by_offset();

        let transport = if transport_ptr.is_null() {
            TransportInfo::default()
        } else {
            let t = &*transport_ptr;
            TransportInfo {
                playing: t.playing != 0,
                recording: t.recording != 0,
                tempo: t.tempo,
                // VST3 hosts deliver `i32` time-signature fields; the
                // u8 narrowing is bounded by the MIDI domain (≤ 255).
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                time_sig_num: t.time_sig_num as u8,
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                time_sig_den: t.time_sig_den as u8,
                position_samples: sample_pos_i64(t.position_samples),
                // Derived from samples so a plugin reading
                // `position_seconds` gets the same value on every format
                // (CLAP populates it directly). Guard the pre-reset zero SR.
                position_seconds: if scr.sample_rate > 0.0 {
                    t.position_samples / scr.sample_rate
                } else {
                    0.0
                },
                position_beats: t.position_beats,
                bar_start_beats: t.bar_start_beats,
                loop_active: t.cycle_active != 0,
                loop_start_beats: t.cycle_start_beats,
                loop_end_beats: t.cycle_end_beats,
            }
        };

        scr.output_events.clear();
        scr.output_events.clear_overflow();
        inst.transport_slot.write(&transport);

        let mut transport_snap = transport;
        let chunk_args = ChunkedProcess {
            events: &scr.event_list,
            sub_event_scratch: &mut scr.sub_event_scratch,
            transport: &mut transport_snap,
            sample_rate: scr.sample_rate,
            process_mode: vst3_process_mode(process_mode),
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
        scr.output_events.ensure_sorted_by_offset();
        // End the `audio_buffer` borrow before reaching back into scratch.
        let _ = audio_buffer;
        // For `f64` plugins the scratch holds the rendered output -
        // copy + narrow it back to the host's `f32` pointers here.
        // No-op for `f32` plugins (output already pointed at the
        // host buffer).
        scr.scratch
            .finish_widening(outputs, num_output_channels, len_u32(num_frames));

        // A latency or tail change raises a wait-free signal; the UI thread
        // drains it as `kLatencyChanged`, and the new values are reported
        // from the next activation.
        inst.delay
            .observe_and_signal(plugin.latency(), plugin.tail(), &inst.host_queue);
    })
    .is_some();
    if !ok {
        // Panic in plugin.process() - zero outputs so the host
        // doesn't keep playing whatever stale samples were in the
        // buffer when DSP died.
        unsafe {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, nf);
                }
            }
        }
    }
    u32::from(ok)
}

/// Test-only smoke helper for the `rt-paranoid` CI gate: drives a few
/// real process blocks through this wrapper's per-block glue via the
/// shared `process_block` body (with null events / transport / param
/// changes and small stereo buffers), returning the steady-state
/// audio-thread allocation count (0 = clean). Vacuously 0 unless the
/// `rt-paranoid` feature installs the checking allocator. Not public API.
#[doc(hidden)]
#[must_use]
pub fn rt_paranoid_smoke<P: PluginExport>() -> u32 {
    struct DestroyGuard<P: PluginExport>(*mut std::ffi::c_void, std::marker::PhantomData<P>);
    impl<P: PluginExport> Drop for DestroyGuard<P> {
        fn drop(&mut self) {
            // SAFETY: owns `self.0` constructed by `cb_create::<P>()`.
            unsafe { cb_destroy::<P>(self.0) };
        }
    }

    const FRAMES: u32 = 512;
    const CH: u32 = 2;
    let frames = FRAMES as usize;
    // SAFETY: constructs, drives, and destroys its own instance; all
    // pointers below outlive each `process_block` call, buffers sized to
    // `FRAMES`, and the event / transport / param pointers are null
    // (which `process_block` tolerates).
    unsafe {
        let ctx = cb_create::<P>();
        assert!(
            !ctx.is_null(),
            "vst3 smoke: cb_create returned null (plugin construction failed)"
        );
        let _guard = DestroyGuard::<P>(ctx, std::marker::PhantomData);
        assert_eq!(cb_prepare::<P>(ctx, 48_000.0, FRAMES, 0), 1);

        // Non-zero input so the sanity check below can confirm the block
        // actually processed (a no-op harness would leave zeros).
        let in_left = vec![0.5f32; frames];
        let in_right = vec![0.5f32; frames];
        let mut out_left = vec![0f32; frames];
        let mut out_right = vec![0f32; frames];
        let in_ptrs: [*const f32; 2] = [in_left.as_ptr(), in_right.as_ptr()];
        let mut out_ptrs: [*mut f32; 2] = [out_left.as_mut_ptr(), out_right.as_mut_ptr()];
        let bus_channels = [CH];

        let mut count = 0;
        for _ in 0..3 {
            let ((), n) = audit(|| {
                cb_begin_input_events::<P>(ctx, FRAMES);
                process_block::<P, f32>(
                    ctx,
                    in_ptrs.as_ptr(),
                    out_ptrs.as_mut_ptr(),
                    CH,
                    CH,
                    bus_channels.as_ptr(),
                    1,
                    1,
                    bus_channels.as_ptr(),
                    1,
                    1,
                    FRAMES,
                    std::ptr::null(),
                    0,
                );
            });
            count = n;
        }

        assert!(
            out_left.iter().any(|s| s.abs() > 0.0),
            "vst3 smoke: process did not run (output stayed zero)"
        );
        count
    }
}

// ---------------------------------------------------------------------------
// Latency + tail callbacks
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn cb_get_latency<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        inst.delay.latency()
    }
}

pub(crate) unsafe extern "C" fn cb_get_tail<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        inst.delay.tail()
    }
}
