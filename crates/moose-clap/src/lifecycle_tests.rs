use super::host_value_fixture::{TestParams, TestPlugin};
use super::*;
use moose_core::buffer::AudioBuffer;
use moose_core::process::{ProcessContext, ProcessStatus};

struct Probe {
    inner: TestPlugin,
    resets: usize,
    pressed: bool,
}

impl PluginExport for Probe {
    type Params = TestParams;
    fn create() -> Self {
        Self {
            inner: TestPlugin::create(),
            resets: 0,
            pressed: false,
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
    }
    fn process(
        &mut self,
        _: &mut AudioBuffer,
        _: &EventList,
        _: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }
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
        data_from_plugin::<Probe>(plugin)
            .latency_dirty
            .store(true, Ordering::Relaxed);
        callbacks.on_main_thread.unwrap()(plugin);
        assert_eq!(calls[0].load(Ordering::Relaxed), 1);
        assert_eq!(calls[1].load(Ordering::Relaxed), 0);
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
