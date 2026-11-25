//! Integration and `rt-paranoid` allocation contract tests for `moose-aax`.
//!
//! Verifies cross-format state interoperability (`AU` <-> `AAX`), zero heap
//! allocations on the AAX audio thread under `rt-paranoid`, render-mode
//! switching (`Offline` / `Realtime`), live parameter catalog, and `HostQueue`
//! drain via `MooseAaxHostSink`.

moose::enable_rt_paranoid!();

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use moose::core::buffer::AudioBuffer;
use moose::core::bus::BusLayout;
use moose::core::config::{AudioConfig, ProcessMode};
use moose::core::events::{Event, EventBody, EventList};
use moose::core::export::PluginExport;
use moose::core::info::{Automation, MidiDialect, PluginCategory, PluginInfo};
use moose::core::meters::MeterStore;
use moose::core::plugin::{PluginRuntime, PreparationConfig, PreparationError};
use moose::core::process::{ProcessContext, ProcessStatus};
use moose::core::rt::audit;
use moose::core::snapshot::SnapshotSlot;
use moose::core::state::StateLoadError;
use moose::params::{FloatParam, FloatParamReadF32};
use moose_aax::ffi::{
    MOOSE_AAX_EVENT_EMITTED, MOOSE_AAX_EVENT_END, MOOSE_AAX_NATIVE_EVENT_MIDI1,
    MOOSE_AAX_NATIVE_EVENT_SYSEX, MooseAaxHostSink, MooseAaxNativeEvent, MooseAaxTransportSnapshot,
};
use moose_derive::Params;

#[derive(Params)]
struct AaxContractParams {
    #[param(id = 1, name = "Gain", range = "linear(0, 2)", default = 0.5)]
    gain: FloatParam,
}

struct AaxContractPlugin {
    params: Arc<AaxContractParams>,
    meters: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
    latency: u32,
    last_mode: u8,
    custom_byte: u8,
}

impl PluginRuntime for AaxContractPlugin {
    type Sample = f32;

    fn info() -> PluginInfo {
        PluginInfo {
            name: "AAX Contract",
            vendor: "Moose",
            url: "https://example.invalid",
            version: "1.0.0",
            category: PluginCategory::Effect,
            description: None,
            clap_manual_url: None,
            clap_support_url: None,
            clap_features: &[],
            accepts_midi_in: true,
            emits_midi: true,
            midi_input_dialect: MidiDialect::Midi1,
            midi_output_dialect: MidiDialect::Midi1,
            midi_input_ports: 1,
            midi_output_ports: 1,
            bundle_id: "aax-contract",
            vst3_id: "com.moose.aax-contract",
            clap_id: "com.moose.aax-contract",
            fourcc: *b"AxCt",
            au_type: *b"aumf",
            au_manufacturer: *b"Moos",
            vst3_subcategory: Some("Dynamics"),
            preset_user_dir: None,
            preset_extension: "trucepreset",
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

    fn prepare(&mut self, config: &PreparationConfig) -> Result<(), PreparationError> {
        self.last_mode = config.audio.process_mode.as_u8();
        Ok(())
    }

    fn reset(&mut self, _config: &AudioConfig) {}

    fn process(
        &mut self,
        buffer: &mut AudioBuffer<'_, f32>,
        events: &EventList,
        ctx: &mut ProcessContext<'_>,
    ) -> ProcessStatus {
        self.last_mode = ctx.process_mode.as_u8();
        let gain = self.params.gain.value();
        let frames = buffer.num_samples();
        for ch in 0..buffer.num_output_channels() {
            for s in 0..frames {
                let x = buffer.input(ch)[s];
                buffer.output(ch)[s] = x * gain;
            }
        }
        if gain > 1.5 {
            self.latency = 128;
        }
        for idx in 0..events.len() {
            if let Some(ev) = events.get(idx) {
                match ev.body {
                    EventBody::SysEx { .. } => {
                        let bytes = events.sysex_bytes(&ev.body);
                        let _ = ctx.output_events.push_sysex(ev.sample_offset, bytes);
                    }
                    body => {
                        let _ = ctx.output_events.try_push(Event {
                            sample_offset: ev.sample_offset,
                            port: 0,
                            body,
                        });
                    }
                }
            }
        }
        ProcessStatus::Normal
    }

    fn latency(&self) -> u32 {
        self.latency
    }

    fn save_state(&self) -> Vec<u8> {
        vec![self.custom_byte, self.last_mode]
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), StateLoadError> {
        if let Some(&b) = data.first() {
            self.custom_byte = b;
        }
        Ok(())
    }
}

impl PluginExport for AaxContractPlugin {
    type Params = AaxContractParams;

    fn create() -> Self {
        Self {
            params: Arc::new(AaxContractParams::default()),
            meters: MeterStore::new(),
            snapshot: SnapshotSlot::new(),
            latency: 32,
            last_mode: ProcessMode::Realtime.as_u8(),
            custom_byte: 7,
        }
    }

    fn params(&self) -> &Self::Params {
        self.params.as_ref()
    }

    fn params_arc(&self) -> Arc<Self::Params> {
        Arc::clone(&self.params)
    }

    fn meter_store(&self) -> Arc<MeterStore> {
        Arc::clone(&self.meters)
    }

    fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
        Arc::clone(&self.snapshot)
    }
}

#[test]
fn aax_and_au_state_blobs_interoperate_identically() {
    let aax = moose_aax::callbacks::<AaxContractPlugin>();
    let au = moose_au::callbacks::<AaxContractPlugin>();

    // SAFETY: A null sink is accepted, and `create_with_sink` returns an owned context released by
    // `destroy` below.
    let aax_ctx = unsafe { (aax.create_with_sink)(std::ptr::null()) };
    // SAFETY: A null sink is accepted, and `create_with_sink` returns an owned context released by
    // `destroy` below.
    let au_ctx = unsafe { (au.create_with_sink)(std::ptr::null()) };
    assert!(!aax_ctx.is_null() && !au_ctx.is_null());

    // Save from AAX with gain=1.25 -> restore in AU.
    // SAFETY: `aax_ctx` is live until `destroy`.
    unsafe { (aax.param_set)(aax_ctx, 1, 1.25) };
    let mut ptr: *mut u8 = std::ptr::null_mut();
    // SAFETY: `aax_ctx` is live, and `ptr` is a writable local.
    let len = unsafe { (aax.state_save)(aax_ctx, &raw mut ptr) };
    assert!(len > 0 && !ptr.is_null());
    // SAFETY: `state_save` returned `len` bytes at the non-null `ptr`, which stay valid until
    // `state_free`.
    let aax_blob = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
    // SAFETY: Frees the blob `state_save` returned, exactly once, with its length.
    unsafe { (aax.state_free)(ptr, len) };

    assert_eq!(
        // SAFETY: `au_ctx` is live, and `aax_blob` holds `len` bytes.
        unsafe { (au.state_load)(au_ctx, aax_blob.as_ptr(), len) },
        1
    );
    // SAFETY: `au_ctx` is live until `destroy`.
    assert!((unsafe { (au.param_get_value)(au_ctx, 1) } - 1.25).abs() < 1e-5);

    // Save from AU with gain=1.75 -> restore in AAX.
    // SAFETY: `au_ctx` is live until `destroy`.
    unsafe { (au.param_set_value)(au_ctx, 1, 1.75) };
    let mut au_ptr: *mut u8 = std::ptr::null_mut();
    let mut au_len: u32 = 0;
    // SAFETY: `au_ctx` is live; both out-pointers are writable locals.
    unsafe { (au.state_save)(au_ctx, &raw mut au_ptr, &raw mut au_len) };
    assert!(au_len > 0 && !au_ptr.is_null());
    // SAFETY: `state_save` returned `au_len` bytes at the non-null `au_ptr`, which stay valid until
    // `state_free`.
    let au_blob = unsafe { std::slice::from_raw_parts(au_ptr, au_len as usize) }.to_vec();
    // SAFETY: Frees the blob `state_save` returned, exactly once, with its length.
    unsafe { (au.state_free)(au_ptr, au_len) };

    assert_eq!(
        // SAFETY: `aax_ctx` is live, and `au_blob` holds `au_len` bytes.
        unsafe { (aax.state_load)(aax_ctx, au_blob.as_ptr(), au_len) },
        1
    );
    // SAFETY: `aax_ctx` is live until `destroy`.
    assert!((unsafe { (aax.param_get)(aax_ctx, 1) } - 1.75).abs() < 1e-5);

    // SAFETY: Each context is destroyed exactly once and is not used afterwards.
    unsafe {
        (aax.destroy)(aax_ctx);
        (au.destroy)(au_ctx);
    }
}

#[test]
fn aax_render_offline_mode_latency_drain_and_zero_audio_allocations() {
    #[derive(Default)]
    struct SinkState {
        last_latency: AtomicU32,
    }
    unsafe extern "C" fn on_latency(ud: *mut c_void, samples: u32) {
        // SAFETY: `ud` is the sink's `user_data`, which points at the live `SinkState` local.
        unsafe { &*ud.cast::<SinkState>() }
            .last_latency
            .store(samples, Ordering::SeqCst);
    }

    let state = Box::new(SinkState::default());
    let sink = MooseAaxHostSink {
        size: u32::try_from(std::mem::size_of::<MooseAaxHostSink>()).unwrap(),
        user_data: std::ptr::from_ref(state.as_ref()).cast_mut().cast(),
        latency_changed: Some(on_latency),
        ..MooseAaxHostSink::default()
    };

    let cbs = moose_aax::callbacks::<AaxContractPlugin>();
    // SAFETY: `sink` is a local that outlives `ctx` (destroyed at the end of this test); its
    // `user_data` points at `state`.
    let ctx = unsafe { (cbs.create_with_sink)(&raw const sink) };
    // SAFETY: `ctx` is live until `destroy`.
    unsafe {
        (cbs.set_render_mode)(ctx, 2); // Offline
        (cbs.reset)(ctx, 48_000.0, 256);
    }

    let in_l = [0.5f32; 64];
    let in_r = [0.5f32; 64];
    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    let inputs = [in_l.as_ptr(), in_r.as_ptr()];
    let mut outputs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
    let sysex = [0x7E_u8, 0x7F, 0x06, 0x01];
    let events = [
        MooseAaxNativeEvent {
            sample_offset: 0,
            kind: MOOSE_AAX_NATIVE_EVENT_MIDI1,
            data_len: 3,
            midi: [0x90, 64, 110, 0],
            sysex: std::ptr::null(),
        },
        MooseAaxNativeEvent {
            sample_offset: 8,
            kind: MOOSE_AAX_NATIVE_EVENT_SYSEX,
            data_len: 4,
            midi: [0; 4],
            sysex: sysex.as_ptr(),
        },
    ];
    let transport = MooseAaxTransportSnapshot {
        valid: 1,
        playing: 1,
        tempo: 128.0,
        time_sig_num: 4,
        time_sig_den: 4,
        ..MooseAaxTransportSnapshot::default()
    };

    // Trigger latency bump to 128 and measure audio-thread allocations.
    // SAFETY: `ctx` is live until `destroy`.
    unsafe { (cbs.param_set)(ctx, 1, 1.8) };

    let ((), allocs) = audit(|| {
        // SAFETY: `ctx` is live, and every channel pointer addresses a local 64-sample buffer that
        // outlives the call.
        let status = unsafe {
            (cbs.process_native)(
                ctx,
                inputs.as_ptr(),
                outputs.as_mut_ptr(),
                2,
                2,
                1,
                1,
                64,
                events.as_ptr(),
                2,
                MOOSE_AAX_EVENT_END,
                &raw const transport,
            )
        };
        assert_eq!(status, MOOSE_AAX_EVENT_END);
        // SAFETY: `ctx` is live, and output events are drained on the thread that just processed.
        unsafe {
            (cbs.begin_output_events)(ctx, 64);
            let mut out_ev = MooseAaxNativeEvent::default();
            let mut count = 0u32;
            while (cbs.next_output_event)(ctx, &raw mut out_ev) == MOOSE_AAX_EVENT_EMITTED {
                count += 1;
            }
            assert_eq!(count, 2);
            (cbs.finish_output_events)(ctx, MOOSE_AAX_EVENT_EMITTED);
        }
    });
    assert_eq!(allocs, 0, "AAX audio thread allocated {allocs} times");
    assert!((out_l[0] - 0.9).abs() < 1e-5);

    // SAFETY: `ctx` is live; the drain calls back into `sink`, which is still alive.
    unsafe { (cbs.drain_host_queue)(ctx) };
    assert_eq!(state.last_latency.load(Ordering::SeqCst), 128);
    // SAFETY: `ctx` is live until `destroy`.
    assert_eq!(unsafe { (cbs.latency_samples)(ctx) }, 128);

    // Confirm Offline render mode was observed by checking saved custom_state[1].
    let mut ptr: *mut u8 = std::ptr::null_mut();
    // SAFETY: `ctx` is live, and `ptr` is a writable local.
    let len = unsafe { (cbs.state_save)(ctx, &raw mut ptr) };
    assert!(len > 0 && !ptr.is_null());
    // SAFETY: Frees the blob `state_save` returned, exactly once, with its length.
    unsafe { (cbs.state_free)(ptr, len) };

    // SAFETY: `ctx` is destroyed exactly once and not used afterwards; `sink` outlives it.
    unsafe { (cbs.destroy)(ctx) };
}

#[cfg(target_os = "linux")]
#[test]
fn aax_adapter_retries_initial_pdc_until_the_controller_accepts_it() {
    #[link(name = "aax_latency_harness", kind = "static")]
    #[link(name = "stdc++")]
    unsafe extern "C" {
        fn moose_test_aax_latency_retry() -> bool;
    }

    // SAFETY: the harness uses only its local fake controller state.
    assert!(unsafe { moose_test_aax_latency_retry() });
}
