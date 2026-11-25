//! Audio block processing, latency and tail on [`Vst3Instance`].

use crate::backend::ProcessArgs;
use crate::instance::Vst3Instance;
use crate::lifecycle::vst3_process_mode;

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
use std::sync::atomic::Ordering;

impl<P: PluginExport> Vst3Instance<P> {
    /// Shared audio process body generic over the host wire precision `H` (`f32` or `f64`).
    ///
    /// # Safety
    /// Channel pointers in `args.inputs` / `args.outputs` must be valid for
    /// `args.num_frames` samples of `H`.
    #[allow(clippy::too_many_lines)]
    pub(crate) unsafe fn process_block<H: Sample>(&self, args: &mut ProcessArgs<'_>) -> bool {
        let nf = args.num_frames as usize;
        let num_input_channels = len_u32(args.inputs.len());
        let num_output_channels = len_u32(args.outputs.len());
        let inputs: *const *const H = args.inputs.as_ptr().cast();
        let outputs: *mut *mut H = args.outputs.as_mut_ptr().cast();

        // SAFETY: The caller guarantees every non-null channel pointer in `args` is valid for
        // `num_frames` samples of `H` and stays exclusive for this call. Each dereference below
        // stays inside `num_input_channels`/`num_output_channels` and `num_frames`.
        let ok = run_audio_block_latched::<P, ()>(&self.panic_latch, "VST3", || unsafe {
            let num_frames = nf;

            // Strictly wait-free audio entry: if a misbehaving host overlaps a
            // lifecycle callback with `process`, zero outputs and return instead
            // of parking the audio thread.
            let Some(mut audio) = self.audio.try_enter() else {
                for ch in 0..num_output_channels as usize {
                    let ptr = *outputs.add(ch);
                    if !ptr.is_null() {
                        std::ptr::write_bytes(ptr, 0, num_frames);
                    }
                }
                return;
            };

            if !audio.prepared || self.panic_latch.is_failed() {
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

            // Strictly wait-free plugin entry.
            self.processing.store(true, Ordering::Release);
            let Some(mut plugin) = self.plugin.try_enter() else {
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

            self.pending_state.record_audio_block();
            if apply_queued_state(&mut *plugin, &self.pending_state).is_some_and(|r| r.is_ok()) {
                self.host_queue.signal(HostSignal::PARAM_RESCAN);
            }

            let _rt = RtSection::enter();
            let scr = &mut *audio;

            debug_assert_eq!(scr.input_num_frames as usize, num_frames);
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
            debug_assert!(args.in_bus_channels.len() <= MAX_AUDIO_BUSES);
            for (index, &channels) in args.in_bus_channels.iter().enumerate() {
                let pushed = bus_routing.push_input(
                    channels,
                    if args.in_bus_active & (1_u32 << index) == 0 {
                        BusActivation::Inactive
                    } else {
                        BusActivation::Active
                    },
                );
                debug_assert!(pushed);
            }
            debug_assert!(args.out_bus_channels.len() <= MAX_AUDIO_BUSES);
            for (index, &channels) in args.out_bus_channels.iter().enumerate() {
                let pushed = bus_routing.push_output(
                    channels,
                    if args.out_bus_active & (1_u32 << index) == 0 {
                        BusActivation::Inactive
                    } else {
                        BusActivation::Active
                    },
                );
                debug_assert!(pushed);
            }

            scr.event_list.ensure_sorted_by_offset();

            let transport = match args.transport {
                None => TransportInfo::default(),
                Some(t) => TransportInfo {
                    playing: t.playing,
                    recording: t.recording,
                    tempo: t.tempo,
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    time_sig_num: t.time_sig_num as u8,
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    time_sig_den: t.time_sig_den as u8,
                    position_samples: sample_pos_i64(t.position_samples),
                    position_seconds: if scr.sample_rate > 0.0 {
                        t.position_samples / scr.sample_rate
                    } else {
                        0.0
                    },
                    position_beats: t.position_beats,
                    bar_start_beats: t.bar_start_beats,
                    loop_active: t.cycle_active,
                    loop_start_beats: t.cycle_start_beats,
                    loop_end_beats: t.cycle_end_beats,
                },
            };

            scr.output_events.clear();
            scr.output_events.clear_overflow();
            self.transport_slot.write(&transport);

            let mut transport_snap = transport;
            let chunk_args = ChunkedProcess {
                events: &scr.event_list,
                sub_event_scratch: &mut scr.sub_event_scratch,
                transport: &mut transport_snap,
                sample_rate: scr.sample_rate,
                process_mode: vst3_process_mode(args.process_mode),
                output_events: &mut scr.output_events,
                params_fn: None,
                meters_fn: None,
                latency_change_request: None,
                param_infos: &self.param_infos,
                automation: self.automation,
            };
            process_chunked_with_bus_routing_and_catalog(
                &mut *plugin,
                self.params_arc.as_ref() as &dyn Params,
                &mut audio_buffer,
                chunk_args,
                bus_routing,
                &self.param_infos,
            );
            scr.output_events.ensure_sorted_by_offset();
            let _ = audio_buffer;
            scr.scratch
                .finish_widening(outputs, num_output_channels, len_u32(num_frames));

            if std::mem::take(&mut scr.param_changed_this_block) && self.feedback.has_waker() {
                self.host_queue
                    .signal(moose_core::host_queue::HostSignal::EDITOR_WAKE);
            }

            self.delay
                .observe_and_signal(plugin.latency(), plugin.tail(), &self.host_queue);
        })
        .is_some();

        if !ok {
            // SAFETY: Same contract: each non-null output channel holds `nf` writable samples, so
            // zeroing them stays in bounds.
            unsafe {
                for ch in 0..num_output_channels as usize {
                    let ptr = *outputs.add(ch);
                    if !ptr.is_null() {
                        std::ptr::write_bytes(ptr, 0, nf);
                    }
                }
            }
            if let Some(mut audio) = self.audio.try_enter() {
                audio.event_list.clear();
                audio.output_events.clear();
                audio.output_events.clear_overflow();
                audio.param_changed_this_block = false;
            }
        }
        ok
    }
}

/// Test-only smoke helper for the `rt-paranoid` CI gate: drives a few
/// real process blocks through this wrapper's per-block glue via
/// [`Vst3Instance::process_block`], returning the steady-state audio-thread
/// allocation count (0 = clean). Not public API.
#[doc(hidden)]
#[must_use]
pub fn rt_paranoid_smoke<P: PluginExport>() -> u32 {
    const FRAMES: u32 = 512;
    const CH: u32 = 2;
    let frames = FRAMES as usize;

    let inst = Vst3Instance::<P>::create(moose_core::info::HostIdentity::new(
        moose_core::info::HostFormat::Vst3,
    ))
    .expect("vst3 smoke: create returned None (plugin construction failed)");
    assert!(inst.prepare_impl(48_000.0, FRAMES, 0));

    let in_left = vec![0.5f32; frames];
    let in_right = vec![0.5f32; frames];
    let mut out_left = vec![0f32; frames];
    let mut out_right = vec![0f32; frames];
    let in_ptrs = [
        in_left.as_ptr().cast::<std::ffi::c_void>(),
        in_right.as_ptr().cast::<std::ffi::c_void>(),
    ];
    let mut out_ptrs = [
        out_left.as_mut_ptr().cast::<std::ffi::c_void>(),
        out_right.as_mut_ptr().cast::<std::ffi::c_void>(),
    ];
    let bus_channels = [CH];

    let mut count = 0;
    for _ in 0..3 {
        let ((), n) = audit(|| {
            inst.begin_input_events_impl(FRAMES);
            let mut args = ProcessArgs {
                inputs: &in_ptrs,
                outputs: &mut out_ptrs,
                in_bus_channels: &bus_channels,
                in_bus_active: 1,
                out_bus_channels: &bus_channels,
                out_bus_active: 1,
                num_frames: FRAMES,
                transport: None,
                process_mode: 0,
            };
            // SAFETY: `in_ptrs` / `out_ptrs` point to valid `FRAMES`-sample `f32` buffers.
            let _ = unsafe { inst.process_block::<f32>(&mut args) };
        });
        count = n;
    }

    assert!(
        out_left.iter().any(|s| s.abs() > 0.0),
        "vst3 smoke: process did not run (output stayed zero)"
    );
    count
}
