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
