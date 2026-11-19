use super::host_value_fixture::{TestParams, TestPlugin};
use super::*;
use moose_core::buffer::AudioBuffer;
use moose_core::process::{ProcessContext, ProcessStatus};

struct Probe {
    inner: TestPlugin,
    resets: usize,
    pressed: bool,
    processed_frames: usize,
    custom: Vec<u8>,
}

impl PluginExport for Probe {
    type Params = TestParams;
    fn clap_extension(id: &CStr) -> Option<moose_core::export::CustomClapExtension> {
        if id == c"test.moose.diagnostics" || id == CLAP_EXT_PARAMS {
            // SAFETY: TABLE uses the test extension ABI and has static storage.
            Some(unsafe { moose_core::export::CustomClapExtension::from_static(&TABLE) })
        } else if id == CLAP_EXT_REMOTE_CONTROLS {
            // SAFETY: this immutable table has the declared remote-controls ABI.
            Some(unsafe { moose_core::export::CustomClapExtension::from_static(&REMOTE_TABLE) })
        } else {
            None
        }
    }
    fn with_dsp_state(&mut self, callback: &mut dyn FnMut(&mut dyn std::any::Any)) -> bool {
        callback(&mut self.resets);
        true
    }

    fn create() -> Self {
        Self {
            inner: TestPlugin::create(),
            resets: 0,
            pressed: false,
            processed_frames: 0,
            custom: Vec::new(),
        }
    }
    fn params(&self) -> &TestParams {
        self.inner.params()
    }
    fn params_arc(&self) -> Arc<TestParams> {
        self.inner.params_arc()
    }
    fn meter_store(&self) -> Arc<MeterStore> {
        self.inner.meter_store()
    }
    fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
        self.inner.snapshot_slot()
    }
}

impl PluginRuntime for Probe {
    type Sample = f32;
    fn info() -> PluginInfo {
        let mut info = TestPlugin::info();
        info.accepts_midi_in = true;
        info.emits_midi = true;
        info.midi_input_ports = 1;
        info.midi_output_ports = 1;
        info
    }
    fn reset(&mut self, _: &AudioConfig) {
        self.resets += 1;
        self.pressed = false;
        if ALLOC_IN_RESET.get() {
            std::hint::black_box(vec![0_u8; 64]);
        }
    }
    fn save_state(&self) -> Vec<u8> {
        self.custom.clone()
    }
    fn load_state(&mut self, data: &[u8]) -> Result<(), moose_core::state::StateLoadError> {
        if data == b"bad" {
            return Err(moose_core::state::StateLoadError::Other("rejected".into()));
        }
        self.custom = data.to_vec();
        Ok(())
    }
    fn validate_state(data: &[u8]) -> Result<(), moose_core::state::StateLoadError> {
        if data == b"invalid" {
            return Err(moose_core::state::StateLoadError::Other(
                "invalid framing".into(),
            ));
        }
        Ok(())
    }
    fn process(
        &mut self,
        audio: &mut AudioBuffer,
        _: &EventList,
        _: &mut ProcessContext,
    ) -> ProcessStatus {
        assert!(!PANIC_IN_PROCESS.get(), "probe panic");
        self.processed_frames += audio.num_samples();
        for channel in 0..audio.channels() {
            let (input, output) = audio.io(channel);
            output.copy_from_slice(input);
        }
        ProcessStatus::Normal
    }
}

#[cfg(feature = "rt-paranoid")]
#[global_allocator]
static TEST_ALLOC: moose_core::rt::RtCheckAlloc = moose_core::rt::RtCheckAlloc::new();

// Makes `Probe::reset` allocate, for the reset-is-real-time test.
// Thread-local so concurrent tests' resets never see it.
thread_local! {
    static PANIC_IN_PROCESS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ALLOC_IN_RESET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn with_probe(test: impl FnOnce(*const clap_plugin)) {
    // SAFETY: host and descriptor outlive the serial vtable calls and destruction.
    unsafe {
        let mut host: clap_host = std::mem::zeroed();
        host.clap_version = clap_sys::version::CLAP_VERSION;
        host.get_extension = Some(no_extension);
        let descriptor = DescriptorHolder::new(&Probe::info());
        let plugin =
            create_plugin_instance::<Probe>(&raw const descriptor.descriptor, &raw const host);
        assert!(!plugin.is_null());
        assert!((*plugin).init.unwrap()(plugin));
        test(plugin);
        (*plugin).destroy.unwrap()(plugin);
    }
}

fn audited_process(plugin: *const clap_plugin, process: &clap_process) -> (i32, u32) {
    moose_core::rt::audit(|| {
        let _rt = RtSection::enter();
        // SAFETY: the caller owns the live plugin and all process buffers.
        unsafe { (*plugin).process.unwrap()(plugin, process) }
    })
}

#[test]
#[cfg(feature = "rt-paranoid")]
fn process_allocation_checker_detects_an_allocation() {
    let ((), allocations) = moose_core::rt::audit(|| {
        let _rt = RtSection::enter();
        std::hint::black_box(Box::new(std::hint::black_box(1_u32)));
    });
    assert!(
        allocations > 0,
        "the process allocation checker must be active"
    );
}

#[test]
#[cfg(feature = "rt-paranoid")]
fn larger_reactivation_processes_in_place_without_allocating() {
    for first_frames in [0, 64, 1024] {
        with_probe(|plugin| {
            // SAFETY: each block owns its sample buffers; lifecycle calls are serial.
            unsafe {
                let callbacks = &*plugin;
                let mut left = vec![0.25_f32; 2048];
                let mut right = vec![-0.5_f32; 2048];
                let mut channels = [left.as_mut_ptr(), right.as_mut_ptr()];
                let input = clap_audio_buffer {
                    data32: channels.as_mut_ptr(),
                    data64: ptr::null_mut(),
                    channel_count: 2,
                    latency: 0,
                    constant_mask: 0,
                };
                let mut output = input;
                let mut process = clap_process {
                    steady_time: 0,
                    frames_count: first_frames,
                    transport: ptr::null(),
                    audio_inputs: &raw const input,
                    audio_outputs: &raw mut output,
                    audio_inputs_count: 1,
                    audio_outputs_count: 1,
                    in_events: ptr::null(),
                    out_events: ptr::null(),
                };
                assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 1024));
                assert!(callbacks.start_processing.unwrap()(plugin));
                assert_eq!(
                    audited_process(plugin, &process),
                    (CLAP_PROCESS_CONTINUE, 0)
                );
                callbacks.stop_processing.unwrap()(plugin);
                callbacks.deactivate.unwrap()(plugin);
                assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 2048));
                assert!(callbacks.start_processing.unwrap()(plugin));
                process.frames_count = 2048;
                let (status, allocations) = audited_process(plugin, &process);
                assert_eq!(status, CLAP_PROCESS_CONTINUE);
                assert_eq!(allocations, 0, "first block had {first_frames} frames");
                assert!(
                    left.iter()
                        .all(|sample| sample.to_bits() == 0.25_f32.to_bits())
                );
                assert!(
                    right
                        .iter()
                        .all(|sample| sample.to_bits() == (-0.5_f32).to_bits())
                );
                assert_eq!(
                    enter_plugin(&data_from_plugin::<Probe>(plugin).plugin).processed_frames,
                    first_frames as usize + 2048
                );
                callbacks.stop_processing.unwrap()(plugin);
                callbacks.deactivate.unwrap()(plugin);
            }
        });
    }
}

#[test]
#[cfg(feature = "rt-paranoid")]
fn larger_reactivation_processes_disconnected_channels_without_allocating() {
    with_probe(|plugin| {
        // SAFETY: null data arrays represent disconnected buses with declared channels.
        unsafe {
            let callbacks = &*plugin;
            assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 1024));
            callbacks.deactivate.unwrap()(plugin);
            assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 2048));
            assert!(callbacks.start_processing.unwrap()(plugin));
            let input = clap_audio_buffer {
                data32: ptr::null_mut(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let mut output = input;
            let process = clap_process {
                steady_time: 0,
                frames_count: 2048,
                transport: ptr::null(),
                audio_inputs: &raw const input,
                audio_outputs: &raw mut output,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: ptr::null(),
                out_events: ptr::null(),
            };
            assert_eq!(
                audited_process(plugin, &process),
                (CLAP_PROCESS_CONTINUE, 0)
            );
            callbacks.stop_processing.unwrap()(plugin);
            callbacks.deactivate.unwrap()(plugin);
        }
    });
}

#[test]
fn invalid_process_shape_is_rejected_before_plugin_or_buffer_access() {
    with_probe(|plugin| {
        // SAFETY: deliberately invalid counts use null arrays, which must not be read.
        unsafe {
            let callbacks = &*plugin;
            assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 1024));
            assert!(callbacks.start_processing.unwrap()(plugin));
            for (frames, inputs, outputs) in [(2048, 1, 1), (64, 2, 1), (64, 1, 2), (64, 0, 0)] {
                let process = clap_process {
                    steady_time: 0,
                    frames_count: frames,
                    transport: ptr::null(),
                    audio_inputs: ptr::null(),
                    audio_outputs: ptr::null_mut(),
                    audio_inputs_count: inputs,
                    audio_outputs_count: outputs,
                    in_events: ptr::null(),
                    out_events: ptr::null(),
                };
                assert_eq!(audited_process(plugin, &process), (CLAP_PROCESS_ERROR, 0));
            }
            for (inputs, outputs) in [(1, 2), (3, 2), (2, 1), (2, 3)] {
                let input = clap_audio_buffer {
                    data32: ptr::null_mut(),
                    data64: ptr::null_mut(),
                    channel_count: inputs,
                    latency: 0,
                    constant_mask: 0,
                };
                let mut output = clap_audio_buffer {
                    channel_count: outputs,
                    ..input
                };
                let process = clap_process {
                    steady_time: 0,
                    frames_count: 64,
                    transport: ptr::null(),
                    audio_inputs: &raw const input,
                    audio_outputs: &raw mut output,
                    audio_inputs_count: 1,
                    audio_outputs_count: 1,
                    in_events: ptr::null(),
                    out_events: ptr::null(),
                };
                assert_eq!(audited_process(plugin, &process), (CLAP_PROCESS_ERROR, 0));
            }
            assert_eq!(
                enter_plugin(&data_from_plugin::<Probe>(plugin).plugin).processed_frames,
                0
            );
            callbacks.stop_processing.unwrap()(plugin);
            callbacks.deactivate.unwrap()(plugin);
        }
    });
}

unsafe extern "C" fn no_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
    ptr::null()
}

#[test]
fn active_latency_change_requests_restart_without_changed_callback() {
    unsafe extern "C" fn restart(host: *const clap_host) {
        (unsafe { &*(*host).host_data.cast::<[AtomicU32; 2]>() })[0]
            .fetch_add(1, Ordering::Relaxed);
    }
    unsafe extern "C" fn changed(host: *const clap_host) {
        (unsafe { &*(*host).host_data.cast::<[AtomicU32; 2]>() })[1]
            .fetch_add(1, Ordering::Relaxed);
    }
    unsafe extern "C" fn extension(_: *const clap_host, id: *const c_char) -> *const c_void {
        static LATENCY: clap_host_latency = clap_host_latency {
            changed: Some(changed),
        };
        if unsafe { CStr::from_ptr(id) } == CLAP_EXT_LATENCY {
            (&raw const LATENCY).cast()
        } else {
            ptr::null()
        }
    }
    unsafe {
        let calls = [AtomicU32::new(0), AtomicU32::new(0)];
        let mut host: clap_host = std::mem::zeroed();
        host.clap_version = clap_sys::version::CLAP_VERSION;
        host.host_data = (&raw const calls).cast_mut().cast();
        host.get_extension = Some(extension);
        host.request_restart = Some(restart);
        let descriptor = DescriptorHolder::new(&Probe::info());
        let plugin =
            create_plugin_instance::<Probe>(&raw const descriptor.descriptor, &raw const host);
        let callbacks = &*plugin;
        assert!(callbacks.init.unwrap()(plugin));
        assert!(callbacks.activate.unwrap()(plugin, 48000., 1, 1024));
        let active_latency = latency_get::<Probe>(plugin);
        // A block saw the plugin's latency move: the restart is requested,
        // but the host keeps reading the active latency until then.
        data_from_plugin::<Probe>(plugin)
            .latency_live
            .store(active_latency + 64, Ordering::Relaxed);
        data_from_plugin::<Probe>(plugin)
            .latency_dirty
            .store(true, Ordering::Relaxed);
        callbacks.on_main_thread.unwrap()(plugin);
        assert_eq!(calls[0].load(Ordering::Relaxed), 1);
        assert_eq!(calls[1].load(Ordering::Relaxed), 0);
        assert_eq!(latency_get::<Probe>(plugin), active_latency);
        callbacks.deactivate.unwrap()(plugin);
        data_from_plugin::<Probe>(plugin)
            .latency_dirty
            .store(true, Ordering::Relaxed);
        callbacks.on_main_thread.unwrap()(plugin);
        assert_eq!(calls[1].load(Ordering::Relaxed), 1);
        callbacks.destroy.unwrap()(plugin);
    }
}

#[test]
fn note_ports_advertise_mpe_only_on_input_when_enabled() {
    unsafe {
        let mut host: clap_host = std::mem::zeroed();
        host.clap_version = clap_sys::version::CLAP_VERSION;
        host.get_extension = Some(no_extension);
        let descriptor = DescriptorHolder::new(&Probe::info());
        let plugin =
            create_plugin_instance::<Probe>(&raw const descriptor.descriptor, &raw const host);
        let callbacks = &*plugin;
        assert!(callbacks.init.unwrap()(plugin));
        let ports = &*callbacks.get_extension.unwrap()(plugin, CLAP_EXT_NOTE_PORTS.as_ptr())
            .cast::<clap_plugin_note_ports>();
        for is_input in [false, true] {
            let mut info: clap_note_port_info = std::mem::zeroed();
            assert!(ports.get.unwrap()(plugin, 0, is_input, &raw mut info));
            assert_eq!(info.preferred_dialect, CLAP_NOTE_DIALECT_CLAP);
            let mpe = clap_sys::ext::note_ports::CLAP_NOTE_DIALECT_MIDI_MPE;
            assert_eq!(
                info.supported_dialects & mpe != 0,
                cfg!(feature = "mpe-input") && is_input
            );
        }
        callbacks.destroy.unwrap()(plugin);
    }
}

#[test]
fn zero_frame_process_applies_params_and_requests_presentation_refresh() {
    unsafe extern "C" fn count(_: *const clap_input_events) -> u32 {
        1
    }
    unsafe extern "C" fn get(events: *const clap_input_events, _: u32) -> *const clap_event_header {
        unsafe { (*events).ctx.cast() }
    }
    unsafe extern "C" fn callback(host: *const clap_host) {
        unsafe { &*(*host).host_data.cast::<AtomicU32>() }.fetch_add(1, Ordering::Relaxed);
    }
    unsafe {
        let calls = AtomicU32::new(0);
        let mut host: clap_host = std::mem::zeroed();
        host.clap_version = clap_sys::version::CLAP_VERSION;
        host.host_data = (&raw const calls).cast_mut().cast();
        host.get_extension = Some(no_extension);
        host.request_callback = Some(callback);
        let descriptor = DescriptorHolder::new(&Probe::info());
        let plugin =
            create_plugin_instance::<Probe>(&raw const descriptor.descriptor, &raw const host);
        let callbacks = &*plugin;
        assert!(callbacks.init.unwrap()(plugin));
        assert!(callbacks.activate.unwrap()(plugin, 48000., 1, 1024));
        assert!(callbacks.start_processing.unwrap()(plugin));
        let params = data_from_plugin::<Probe>(plugin).params_arc.clone();
        params.structural_gain.store(true, Ordering::Relaxed);
        let value = clap_event_param_value {
            header: clap_event_header {
                size: size_of_u32::<clap_event_param_value>(),
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: 0,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: -12.,
        };
        let events = clap_input_events {
            ctx: (&raw const value).cast_mut().cast(),
            size: Some(count),
            get: Some(get),
        };
        let process = clap_process {
            steady_time: 0,
            frames_count: 0,
            transport: ptr::null(),
            audio_inputs: ptr::null(),
            audio_outputs: ptr::null_mut(),
            audio_inputs_count: 0,
            audio_outputs_count: 0,
            in_events: &raw const events,
            out_events: ptr::null(),
        };
        assert_eq!(
            callbacks.process.unwrap()(plugin, &raw const process),
            CLAP_PROCESS_CONTINUE
        );
        assert_eq!(params.get_plain(0), Some(-12.));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        callbacks.stop_processing.unwrap()(plugin);
        callbacks.deactivate.unwrap()(plugin);
        callbacks.destroy.unwrap()(plugin);
    }
}

#[test]
fn deactivate_resets_plugin_and_sounding_notes() {
    // SAFETY: the test owns the host, descriptor and plugin through destruction;
    // callbacks run serially with processing stopped before deactivation.
    unsafe {
        let mut host: clap_host = std::mem::zeroed();
        host.clap_version = clap_sys::version::CLAP_VERSION;
        host.get_extension = Some(no_extension);
        let descriptor = DescriptorHolder::new(&Probe::info());
        let plugin =
            create_plugin_instance::<Probe>(&raw const descriptor.descriptor, &raw const host);
        let callbacks = &*plugin;
        assert!(callbacks.init.unwrap()(plugin));
        assert!(callbacks.activate.unwrap()(plugin, 48000., 1, 1024));
        let data = data_from_plugin::<Probe>(plugin);
        assert_eq!(enter_plugin(&data.plugin).resets, 1);
        assert!(callbacks.start_processing.unwrap()(plugin));
        enter_plugin(&data.plugin).pressed = true;
        data.audio.enter().sounding_notes.set(0, 0, 60);
        callbacks.stop_processing.unwrap()(plugin);
        callbacks.deactivate.unwrap()(plugin);
        {
            let instance = enter_plugin(&data.plugin);
            assert_eq!(instance.resets, 2, "deactivation must reset the plugin");
            assert!(!instance.pressed, "deactivation must release held notes");
        }
        let mut sounding = Vec::new();
        data.audio
            .enter()
            .sounding_notes
            .for_each_matching(0, None, None, |ch, key| sounding.push((ch, key)));
        assert!(
            sounding.is_empty(),
            "stale notes survived deactivation: {sounding:?}"
        );
        assert!(callbacks.activate.unwrap()(plugin, 96000., 1, 2048));
        assert!(
            !enter_plugin(&data.plugin).pressed,
            "reactivation resurrected a held note"
        );
        callbacks.deactivate.unwrap()(plugin);
        callbacks.destroy.unwrap()(plugin);
    }
}

#[repr(C)]
struct TestExtension {
    get: unsafe extern "C" fn(*const clap_plugin) -> usize,
}
unsafe extern "C" fn get_resets(plugin: *const clap_plugin) -> usize {
    // SAFETY: the test passes a live Probe and serializes lifecycle operations.
    unsafe { try_with_dsp_state::<Probe, usize, _>(plugin, |state| *state).unwrap_or(usize::MAX) }
}
static TABLE: TestExtension = TestExtension { get: get_resets };
unsafe extern "C" fn remote_count(_: *const clap_plugin) -> u32 {
    17
}
unsafe extern "C" fn remote_get(
    _: *const clap_plugin,
    _: u32,
    _: *mut clap_remote_controls_page,
) -> bool {
    false
}
static REMOTE_TABLE: clap_plugin_remote_controls = clap_plugin_remote_controls {
    count: Some(remote_count),
    get: Some(remote_get),
};

#[test]
fn custom_extension_queries_real_instances_and_rejects_busy() {
    with_probe(|first| {
        with_probe(|second| unsafe {
            let query = (*first).get_extension.unwrap();
            assert!(query(first, ptr::null()).is_null());
            assert!(query(first, c"unknown".as_ptr()).is_null());
            assert!(query(ptr::null(), c"unknown".as_ptr()).is_null());
            let ext = &*query(first, c"test.moose.diagnostics".as_ptr()).cast::<TestExtension>();
            let remote = query(first, CLAP_EXT_REMOTE_CONTROLS.as_ptr());
            assert_eq!(
                remote,
                query(first, CLAP_EXT_REMOTE_CONTROLS_COMPAT.as_ptr())
            );
            assert_eq!(
                (*remote.cast::<clap_plugin_remote_controls>())
                    .count
                    .unwrap()(first),
                17
            );
            assert_ne!(
                query(first, CLAP_EXT_PARAMS.as_ptr()),
                (&raw const TABLE).cast()
            );
            assert_eq!((ext.get)(first), 0);
            assert_eq!((ext.get)(second), 0);
            assert_eq!(
                try_with_dsp_state::<Probe, usize, _>(first, |n| {
                    *n = 5;
                    *n
                }),
                Some(5)
            );
            assert_eq!((ext.get)(first), 5);
            assert_eq!((ext.get)(second), 0);
            let guard = data_from_plugin::<Probe>(first).plugin.try_enter().unwrap();
            assert_eq!((ext.get)(first), usize::MAX);
            assert_eq!(
                try_with_stopped_dsp_state::<Probe, usize, _>(first, |_| 1),
                None
            );
            drop(guard);
            assert_eq!(try_with_dsp_state::<Probe, u8, _>(first, |_| 1), None);
            assert_eq!(
                try_with_dsp_state::<Probe, usize, ()>(first, |_| panic!("test panic")),
                None
            );
            assert_eq!((ext.get)(first), 5);
            assert!((*first).activate.unwrap()(first, 48_000.0, 1, 64));
            assert_eq!((ext.get)(first), usize::MAX);
            assert!(try_with_stopped_dsp_state::<Probe, usize, _>(first, |_| 1).is_some());
            assert!((*first).start_processing.unwrap()(first));
            assert_eq!(
                try_with_stopped_dsp_state::<Probe, usize, _>(first, |_| 1),
                None
            );
            assert_eq!(
                try_with_params::<Probe, _>(first, |params| params.param_infos().len()),
                Some(2)
            );
            assert_eq!(
                try_with_params::<Probe, ()>(first, |_| panic!("test params panic")),
                None
            );
            assert_eq!(try_with_params::<Probe, _>(ptr::null(), |_| 1), None);
            (*first).stop_processing.unwrap()(first);
            assert!(try_with_stopped_dsp_state::<Probe, usize, _>(first, |_| 1).is_some());
            assert_eq!((ext.get)(first), usize::MAX);
            (*first).deactivate.unwrap()(first);
            assert_ne!((ext.get)(first), usize::MAX);
        });
    });
}

unsafe extern "C" fn read_cursor(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    // SAFETY: `ctx` is the test's live `(Vec<u8>, usize)` cursor.
    unsafe {
        let (bytes, at) = &mut *(*stream).ctx.cast::<(Vec<u8>, usize)>();
        let n = (bytes.len() - *at).min(usize::try_from(size).unwrap_or(usize::MAX));
        ptr::copy_nonoverlapping(bytes.as_ptr().add(*at), buffer.cast::<u8>(), n);
        *at += n;
        i64::try_from(n).unwrap_or(-1)
    }
}

unsafe extern "C" fn write_vec(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    // SAFETY: `ctx` is the test's live output `Vec<u8>`.
    unsafe {
        let out = &mut *(*stream).ctx.cast::<Vec<u8>>();
        out.extend_from_slice(std::slice::from_raw_parts(
            buffer.cast::<u8>(),
            usize::try_from(size).unwrap(),
        ));
        i64::try_from(size).unwrap_or(-1)
    }
}

/// Load-then-save in each state where the host thread owns the plugin:
/// inactive, and active between `stop_processing` and `start_processing`
/// (audit finding 1). The save must return the loaded custom state, with
/// no `process` call in between.
#[test]
fn load_then_save_while_not_processing_returns_loaded_custom_state() {
    for phase in ["inactive", "activated", "stopped"] {
        with_probe(|plugin| {
            // SAFETY: serial lifecycle calls on a live instance; the streams'
            // contexts outlive the calls that read them.
            unsafe {
                let callbacks = &*plugin;
                if phase != "inactive" {
                    assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 1024));
                }
                if phase == "stopped" {
                    assert!(callbacks.start_processing.unwrap()(plugin));
                    callbacks.stop_processing.unwrap()(plugin);
                }
                let state_ext = make_state_extension::<Probe>();
                let hash = data_from_plugin::<Probe>(plugin).plugin_id_hash;
                let blob = state::serialize_state(hash, &[], &[], &[7, 8, 9], &[]);
                let mut cursor = (blob, 0_usize);
                let input = clap_istream {
                    ctx: (&raw mut cursor).cast(),
                    read: Some(read_cursor),
                };
                assert!(state_ext.load.unwrap()(plugin, &raw const input), "{phase}");
                let mut saved = Vec::new();
                let output = clap_ostream {
                    ctx: (&raw mut saved).cast(),
                    write: Some(write_vec),
                };
                assert!(
                    state_ext.save.unwrap()(plugin, &raw const output),
                    "{phase}"
                );
                let restored = state::deserialize_state(&saved, hash).expect("own envelope");
                assert_eq!(restored.extra.as_deref(), Some(&[7, 8, 9][..]), "{phase}");
                if phase != "inactive" {
                    callbacks.deactivate.unwrap()(plugin);
                }
            }
        });
    }
}

#[test]
fn rejected_load_fails_and_keeps_the_previous_state() {
    use moose_params::Params;
    for phase in ["inactive", "stopped"] {
        with_probe(|plugin| {
            // SAFETY: serial lifecycle calls on a live instance; the streams'
            // contexts outlive the calls that read them.
            unsafe {
                let callbacks = &*plugin;
                if phase == "stopped" {
                    assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 1024));
                    assert!(callbacks.start_processing.unwrap()(plugin));
                    callbacks.stop_processing.unwrap()(plugin);
                }
                let state_ext = make_state_extension::<Probe>();
                let data = data_from_plugin::<Probe>(plugin);
                let hash = data.plugin_id_hash;
                let load = |gain: f64, extra: &[u8]| {
                    let blob = state::serialize_state(hash, &[0], &[gain], extra, &[]);
                    let mut cursor = (blob, 0_usize);
                    let input = clap_istream {
                        ctx: (&raw mut cursor).cast(),
                        read: Some(read_cursor),
                    };
                    state_ext.load.unwrap()(plugin, &raw const input)
                };
                assert!(load(-12.0, &[1]), "{phase}");
                assert!(
                    !load(-30.0, b"bad"),
                    "{phase}: rejected load reports failure"
                );
                assert_eq!(
                    data.params_arc.get_plain(0),
                    Some(-12.0),
                    "{phase}: params kept"
                );
                let mut saved = Vec::new();
                let output = clap_ostream {
                    ctx: (&raw mut saved).cast(),
                    write: Some(write_vec),
                };
                assert!(
                    state_ext.save.unwrap()(plugin, &raw const output),
                    "{phase}"
                );
                let restored = state::deserialize_state(&saved, hash).expect("own envelope");
                assert_eq!(
                    restored.extra.as_deref(),
                    Some(&[1][..]),
                    "{phase}: custom kept"
                );
                if phase != "inactive" {
                    callbacks.deactivate.unwrap()(plugin);
                }
            }
        });
    }
}

/// A load queued for an audio thread that isn't running yet still fails
/// when the plugin's `validate_state` rejects it on the host thread, and
/// leaves params as they were (step 2's `validate` hook).
#[test]
fn validate_state_rejects_a_queued_load_up_front() {
    use moose_params::Params;
    with_probe(|plugin| {
        // SAFETY: serial lifecycle calls on a live instance; the stream's
        // context outlives the call that reads it.
        unsafe {
            let callbacks = &*plugin;
            assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 1024));
            // Processing, but the host sends no blocks: the load is queued.
            assert!(callbacks.start_processing.unwrap()(plugin));
            let state_ext = make_state_extension::<Probe>();
            let data = data_from_plugin::<Probe>(plugin);
            let before = data.params_arc.get_plain(0);
            let blob = state::serialize_state(data.plugin_id_hash, &[0], &[-30.0], b"invalid", &[]);
            let mut cursor = (blob, 0_usize);
            let input = clap_istream {
                ctx: (&raw mut cursor).cast(),
                read: Some(read_cursor),
            };
            // Without the hook this load would time out queued and report success.
            assert!(
                !state_ext.load.unwrap()(plugin, &raw const input),
                "invalid state reports failure"
            );
            assert_eq!(data.params_arc.get_plain(0), before, "params kept");
            callbacks.stop_processing.unwrap()(plugin);
            callbacks.deactivate.unwrap()(plugin);
        }
    });
}

/// CLAP `reset` is [audio-thread]: the author's `reset` runs in an
/// rt-paranoid section, so an allocation there is reported (finding 3).
#[test]
fn clap_reset_runs_in_the_real_time_section() {
    with_probe(|plugin| {
        // SAFETY: serial lifecycle calls on a live instance.
        unsafe {
            let callbacks = &*plugin;
            assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 1024));
            let reset = callbacks.reset.unwrap();
            let ((), clean) = moose_core::rt::audit(|| reset(plugin));
            assert_eq!(clean, 0);
            ALLOC_IN_RESET.set(true);
            let ((), dirty) = moose_core::rt::audit(|| reset(plugin));
            ALLOC_IN_RESET.set(false);
            assert_eq!(dirty > 0, moose_core::rt::is_active());
            callbacks.deactivate.unwrap()(plugin);
        }
    });
}

/// After a panic in `process` the plugin isn't re-entered: one error
/// status, then silence, until the next activate (finding 5). Not under
/// rt-paranoid: unwinding allocates inside the real-time section.
#[test]
#[cfg(not(feature = "rt-paranoid"))]
fn process_panic_latches_until_reactivation() {
    with_probe(|plugin| {
        // SAFETY: each block owns its sample buffers; lifecycle calls are serial.
        unsafe {
            let callbacks = &*plugin;
            let mut in_l = vec![0.5_f32; 64];
            let mut in_r = vec![0.5_f32; 64];
            let mut out_l = vec![0.0_f32; 64];
            let mut out_r = vec![0.0_f32; 64];
            let mut ins = [in_l.as_mut_ptr(), in_r.as_mut_ptr()];
            let mut outs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
            let input = clap_audio_buffer {
                data32: ins.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let mut output = clap_audio_buffer {
                data32: outs.as_mut_ptr(),
                ..input
            };
            let process = clap_process {
                steady_time: 0,
                frames_count: 64,
                transport: ptr::null(),
                audio_inputs: &raw const input,
                audio_outputs: &raw mut output,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: ptr::null(),
                out_events: ptr::null(),
            };
            let run = || callbacks.process.unwrap()(plugin, &raw const process);
            let frames =
                || enter_plugin(&data_from_plugin::<Probe>(plugin).plugin).processed_frames;
            assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 1024));
            assert!(callbacks.start_processing.unwrap()(plugin));
            PANIC_IN_PROCESS.set(true);
            assert_eq!(
                run(),
                CLAP_PROCESS_ERROR,
                "the panic block reports an error"
            );
            PANIC_IN_PROCESS.set(false);
            let before = frames();
            out_l.fill(1.0);
            assert_eq!(
                run(),
                clap_sys::process::CLAP_PROCESS_SLEEP,
                "latched: no second error"
            );
            assert_eq!(frames(), before, "latched: plugin not re-entered");
            assert!(out_l.iter().all(|s| *s == 0.0), "latched: silence");
            callbacks.stop_processing.unwrap()(plugin);
            callbacks.deactivate.unwrap()(plugin);
            assert!(callbacks.activate.unwrap()(plugin, 48_000., 1, 1024));
            assert!(callbacks.start_processing.unwrap()(plugin));
            assert_eq!(run(), CLAP_PROCESS_CONTINUE, "re-activate clears the latch");
            assert_eq!(frames(), before + 64);
            callbacks.stop_processing.unwrap()(plugin);
            callbacks.deactivate.unwrap()(plugin);
        }
    });
}

#[test]
fn configurable_ports_pick_a_declared_layout_and_activation_reaches_routing() {
    with_probe(|plugin| unsafe {
        let get = (*plugin).get_extension.unwrap();
        let ports = &*get(plugin, CLAP_EXT_CONFIGURABLE_AUDIO_PORTS.as_ptr())
            .cast::<clap_plugin_configurable_audio_ports>();
        let request = |is_input, port_index, channel_count| clap_audio_port_configuration_request {
            is_input,
            port_index,
            channel_count,
            port_type: std::ptr::null(),
            port_details: std::ptr::null(),
        };
        let mono = [request(true, 0, 1), request(false, 0, 1)];
        let quad = [request(false, 0, 4)];
        let missing = [request(true, 1, 2)];
        let mut untyped = request(false, 0, 2);
        untyped.port_type = c"".as_ptr();
        assert!(ports.can_apply_configuration.unwrap()(
            plugin,
            &raw const untyped,
            1
        ));
        let mut mapped = request(false, 0, 2);
        mapped.port_details = [1_u8, 0].as_ptr().cast();
        assert!(
            !ports.can_apply_configuration.unwrap()(plugin, &raw const mapped, 1),
            "stereo has no channel map"
        );
        let details = [1_u8, 0];
        for (port_type, channel_count) in [(CLAP_PORT_MONO, 1), (CLAP_PORT_STEREO, 2)] {
            let mut typed = request(false, 0, channel_count);
            typed.port_type = port_type.as_ptr();
            typed.port_details = details.as_ptr().cast();
            assert!(ports.can_apply_configuration.unwrap()(
                plugin,
                &raw const typed,
                1
            ));
            assert!(ports.apply_configuration.unwrap()(
                plugin,
                &raw const typed,
                1
            ));
        }
        assert!(!ports.can_apply_configuration.unwrap()(
            plugin,
            quad.as_ptr(),
            1
        ));
        assert!(!ports.can_apply_configuration.unwrap()(
            plugin,
            missing.as_ptr(),
            1
        ));
        assert!(ports.apply_configuration.unwrap()(plugin, mono.as_ptr(), 2));
        let data = data_from_plugin::<Probe>(plugin);
        assert_eq!(data.selected_config.load(Ordering::Relaxed), 1);

        let activation = &*get(plugin, CLAP_EXT_AUDIO_PORTS_ACTIVATION.as_ptr())
            .cast::<clap_plugin_audio_ports_activation>();
        let set_active = activation.set_active.unwrap();
        assert!(
            !set_active(plugin, true, 1, false, 32),
            "no second input port"
        );
        assert!(set_active(plugin, true, 0, false, 32));
        assert!((*plugin).activate.unwrap()(plugin, 48_000.0, 1, 64));
        let routing = data.audio.enter().bus_routing;
        assert_eq!(
            routing.input(0).unwrap().activation(),
            BusActivation::Inactive
        );
        assert_eq!(
            routing.output(0).unwrap().activation(),
            BusActivation::Active
        );
        assert!(
            !ports.apply_configuration.unwrap()(plugin, mono.as_ptr(), 2),
            "active"
        );
        (*plugin).deactivate.unwrap()(plugin);
    });
}

#[test]
fn two_byte_clap_midi_events_preserve_length_and_round_trip_exact_replay() {
    use crate::events::{ExactEmit, convert_input_events, emit_exact_clap};
    use clap_sys::events::{
        CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, clap_event_header, clap_event_midi,
        clap_input_events, clap_output_events,
    };
    use moose_core::events::{ExactEventBody, LosslessEventRef};

    unsafe extern "C" fn in_size(list: *const clap_input_events) -> u32 {
        let slice = unsafe { &*(*list).ctx.cast::<[clap_event_midi; 2]>() };
        len_u32(slice.len())
    }
    unsafe extern "C" fn in_get(
        list: *const clap_input_events,
        index: u32,
    ) -> *const clap_event_header {
        let slice = unsafe { &*(*list).ctx.cast::<[clap_event_midi; 2]>() };
        &raw const slice[index as usize].header
    }
    unsafe extern "C" fn out_push(
        list: *const clap_output_events,
        header: *const clap_event_header,
    ) -> bool {
        let pushed = unsafe { &mut *(*list).ctx.cast::<Vec<clap_event_midi>>() };
        assert_eq!(unsafe { (*header).type_ }, CLAP_EVENT_MIDI);
        pushed.push(unsafe { *header.cast::<clap_event_midi>() });
        true
    }

    with_probe(|plugin| unsafe {
        let data = data_from_plugin::<Probe>(plugin);
        let mut audio = data.audio.enter();
        let packets = [
            clap_event_midi {
                header: clap_event_header {
                    size: size_of_u32::<clap_event_midi>(),
                    time: 3,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_MIDI,
                    flags: 0,
                },
                port_index: 0,
                data: [0xC2, 42, 0x7F],
            },
            clap_event_midi {
                header: clap_event_header {
                    size: size_of_u32::<clap_event_midi>(),
                    time: 7,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_MIDI,
                    flags: 0,
                },
                port_index: 0,
                data: [0xD5, 100, 0x7F],
            },
        ];
        let in_events = clap_input_events {
            ctx: (&raw const packets).cast_mut().cast(),
            size: Some(in_size),
            get: Some(in_get),
        };
        let info = Probe::info();
        convert_input_events(
            &mut audio,
            &info,
            &raw const in_events,
            false,
            false,
            Some(64),
        );

        let typed: Vec<_> = audio.event_list.iter().map(|e| e.body).collect();
        assert_eq!(
            typed,
            [
                EventBody::ProgramChange {
                    group: 0,
                    channel: 2,
                    program: 42,
                },
                EventBody::ChannelPressure {
                    group: 0,
                    channel: 5,
                    pressure: 100,
                },
            ]
        );

        let mut emitted = Vec::<clap_event_midi>::new();
        let out_events = clap_output_events {
            ctx: (&raw mut emitted).cast(),
            try_push: Some(out_push),
        };
        let exact_items: Vec<_> = audio
            .event_list
            .lossless_iter()
            .map(|item| match item {
                LosslessEventRef::Exact(exact) => exact,
                LosslessEventRef::Typed(_) => panic!("expected exact representation"),
            })
            .collect();
        assert_eq!(exact_items.len(), 2);
        for (exact, expected_bytes) in exact_items.into_iter().zip([&[0xC2, 42][..], &[0xD5, 100]])
        {
            match exact.body() {
                ExactEventBody::Midi1 { message, .. } => {
                    assert_eq!(message.bytes(), expected_bytes);
                }
                other => panic!("unexpected exact body: {other:?}"),
            }
            assert!(matches!(
                emit_exact_clap(&raw const out_events, out_push, exact, &info, 64),
                ExactEmit::Emitted
            ));
        }
        assert_eq!(emitted.len(), 2);
        assert_eq!(&emitted[0].data[..2], &[0xC2, 42]);
        assert_eq!(&emitted[1].data[..2], &[0xD5, 100]);
    });
}

#[derive(moose_derive::Params)]
#[params(presentation = "panic_pres")]
struct PanickingPresentationParams {
    #[param(id = 0, name = "Gain", range = "linear(-24.0, 24.0)", default = 0.0)]
    gain: moose_params::FloatParam,
    #[skip]
    armed: AtomicBool,
}

impl PanickingPresentationParams {
    fn panic_pres(&self, _id: u32) -> Option<moose_params::ParameterPresentation> {
        assert!(
            !self.armed.load(Ordering::Acquire),
            "simulated parameter_presentation panic"
        );
        None
    }
}

struct PanickingPresentationPlugin {
    params: Arc<PanickingPresentationParams>,
    meters: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
}

impl PluginExport for PanickingPresentationPlugin {
    type Params = PanickingPresentationParams;
    fn create() -> Self {
        Self {
            params: Arc::new(PanickingPresentationParams::new()),
            meters: MeterStore::new(),
            snapshot: SnapshotSlot::new(),
        }
    }
    fn params(&self) -> &Self::Params {
        &self.params
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

impl PluginRuntime for PanickingPresentationPlugin {
    type Sample = f32;
    fn info() -> PluginInfo {
        TestPlugin::info()
    }
    fn reset(&mut self, _: &AudioConfig) {}
    fn process(
        &mut self,
        _: &mut AudioBuffer,
        _: &EventList,
        _: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }
}

#[test]
fn params_get_info_catches_panic_in_parameter_presentation() {
    unsafe {
        let mut host: clap_host = std::mem::zeroed();
        host.clap_version = clap_sys::version::CLAP_VERSION;
        host.get_extension = Some(no_extension);
        let descriptor = DescriptorHolder::new(&PanickingPresentationPlugin::info());
        let plugin = create_plugin_instance::<PanickingPresentationPlugin>(
            &raw const descriptor.descriptor,
            &raw const host,
        );
        assert!(!plugin.is_null());
        assert!((*plugin).init.unwrap()(plugin));
        data_from_plugin::<PanickingPresentationPlugin>(plugin)
            .params_arc
            .armed
            .store(true, Ordering::Release);
        let mut info: clap_param_info = std::mem::zeroed();
        assert!(
            !params_get_info::<PanickingPresentationPlugin>(plugin, 0, &raw mut info),
            "panic in parameter_presentation must be caught and return false"
        );
        (*plugin).destroy.unwrap()(plugin);
    }
}

#[test]
fn audio_ports_config_select_and_list_rescan_clear_cached_ports_inactive() {
    with_probe(|plugin| unsafe {
        let get = (*plugin).get_extension.unwrap();
        let activation = &*get(plugin, CLAP_EXT_AUDIO_PORTS_ACTIVATION.as_ptr())
            .cast::<clap_plugin_audio_ports_activation>();
        let set_active = activation.set_active.unwrap();
        let data = data_from_plugin::<Probe>(plugin);

        // 1. Deactivate input port 0, then select a valid audio_ports_config.
        assert!(set_active(plugin, true, 0, false, 32));
        assert_ne!(data.ports_inactive[0].load(Ordering::Relaxed), 0);
        assert!(
            !audio_ports_config_select::<Probe>(plugin, 99),
            "out-of-range config select fails and keeps activation mask"
        );
        assert_ne!(data.ports_inactive[0].load(Ordering::Relaxed), 0);

        assert!(audio_ports_config_select::<Probe>(plugin, 1));
        assert_eq!(data.ports_inactive[0].load(Ordering::Relaxed), 0);
        assert_eq!(data.ports_inactive[1].load(Ordering::Relaxed), 0);

        assert!((*plugin).activate.unwrap()(plugin, 48_000.0, 1, 64));
        assert_eq!(
            data.audio
                .enter()
                .bus_routing
                .input(0)
                .unwrap()
                .activation(),
            BusActivation::Active
        );
        (*plugin).deactivate.unwrap()(plugin);

        // 2. Deactivate input port 0 again, then trigger CLAP_AUDIO_PORTS_RESCAN_LIST.
        assert!(set_active(plugin, true, 0, false, 32));
        assert_ne!(data.ports_inactive[0].load(Ordering::Relaxed), 0);
        data.host_notifier
            .notify(moose_core::main_thread::HostNotice::AUDIO_PORTS);
        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(data.ports_inactive[0].load(Ordering::Relaxed), 0);
        assert_eq!(data.ports_inactive[1].load(Ordering::Relaxed), 0);

        // 3. Deactivate input port 0 again, then apply a configurable-audio-ports layout.
        assert!(set_active(plugin, true, 0, false, 32));
        assert_ne!(data.ports_inactive[0].load(Ordering::Relaxed), 0);
        let configurable = &*get(plugin, CLAP_EXT_CONFIGURABLE_AUDIO_PORTS.as_ptr())
            .cast::<clap_plugin_configurable_audio_ports>();
        let stereo = [
            clap_audio_port_configuration_request {
                is_input: true,
                port_index: 0,
                channel_count: 2,
                port_type: CLAP_PORT_STEREO.as_ptr(),
                port_details: ptr::null(),
            },
            clap_audio_port_configuration_request {
                is_input: false,
                port_index: 0,
                channel_count: 2,
                port_type: CLAP_PORT_STEREO.as_ptr(),
                port_details: ptr::null(),
            },
        ];
        assert!(configurable.apply_configuration.unwrap()(
            plugin,
            stereo.as_ptr(),
            2
        ));
        assert_eq!(data.ports_inactive[0].load(Ordering::Relaxed), 0);
        assert_eq!(data.ports_inactive[1].load(Ordering::Relaxed), 0);

        assert!((*plugin).activate.unwrap()(plugin, 48_000.0, 1, 64));
        assert_eq!(
            data.audio
                .enter()
                .bus_routing
                .input(0)
                .unwrap()
                .activation(),
            BusActivation::Active
        );
        (*plugin).deactivate.unwrap()(plugin);
    });
}

#[test]
fn preset_load_enforces_bounded_file_read_and_rejects_oversized_files() {
    use crate::state_ext::{
        MAX_PRESET_FILE_BYTES, preset_load_from_location, read_preset_file_bounded,
    };
    use clap_sys::factory::preset_discovery::CLAP_PRESET_DISCOVERY_LOCATION_FILE;
    use moose_params::Params;
    use std::ffi::CString;

    with_probe(|plugin| unsafe {
        let data = data_from_plugin::<Probe>(plugin);
        let dir =
            std::env::temp_dir().join(format!("moose_clap_preset_bound_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let valid_path = dir.join("valid.trucepreset");
        let blob = state::serialize_state(data.plugin_id_hash, &[0], &[-9.0], b"ok", &[]);
        let meta = b"name=Valid\n";
        let mut preset_bytes = Vec::new();
        preset_bytes.extend_from_slice(b"TRPS");
        preset_bytes.extend_from_slice(&1u32.to_le_bytes());
        preset_bytes.extend_from_slice(&len_u32(meta.len()).to_le_bytes());
        preset_bytes.extend_from_slice(meta);
        preset_bytes.extend_from_slice(&(blob.len() as u64).to_le_bytes());
        preset_bytes.extend_from_slice(&blob);
        std::fs::write(&valid_path, &preset_bytes).unwrap();

        // Direct bounded reader rejects when file exceeds a tight limit.
        assert!(read_preset_file_bounded(&valid_path, preset_bytes.len()).is_some());
        assert!(read_preset_file_bounded(&valid_path, preset_bytes.len() - 1).is_none());

        let valid_c = CString::new(valid_path.to_str().unwrap()).unwrap();
        assert!(preset_load_from_location::<Probe>(
            plugin,
            CLAP_PRESET_DISCOVERY_LOCATION_FILE,
            valid_c.as_ptr(),
            ptr::null(),
        ));
        assert_eq!(data.params_arc.get_plain(0), Some(-9.0));

        // Oversized valid TRPS preset file (> MAX_PRESET_FILE_BYTES) is rejected by the size cap.
        let oversized_path = dir.join("oversized.trucepreset");
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&oversized_path).unwrap();
            let oversized_blob_len = (MAX_PRESET_FILE_BYTES + 1) as u64;
            file.write_all(b"TRPS").unwrap();
            file.write_all(&1u32.to_le_bytes()).unwrap();
            file.write_all(&len_u32(meta.len()).to_le_bytes()).unwrap();
            file.write_all(meta).unwrap();
            file.write_all(&oversized_blob_len.to_le_bytes()).unwrap();
            file.set_len(20 + meta.len() as u64 + oversized_blob_len)
                .unwrap();
        }
        let oversized_c = CString::new(oversized_path.to_str().unwrap()).unwrap();
        assert!(!preset_load_from_location::<Probe>(
            plugin,
            CLAP_PRESET_DISCOVERY_LOCATION_FILE,
            oversized_c.as_ptr(),
            ptr::null(),
        ));
        assert_eq!(
            data.params_arc.get_plain(0),
            Some(-9.0),
            "rejected oversized preset leaves existing state intact"
        );

        let _ = std::fs::remove_dir_all(&dir);
    });
}
