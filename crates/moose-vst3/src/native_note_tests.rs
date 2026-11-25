use super::host_value_fixture::{TestParams, TestPlugin};
use super::*;
use moose_core::buffer::AudioBuffer;
use moose_core::bus_routing::{BusActivation, BusRouting};
use moose_core::events::EventBody;
use moose_core::main_thread::{HostNotice, HostNotifier};
use moose_core::process::{ProcessContext, ProcessStatus};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Probe {
    inner: TestPlugin,
}

impl PluginExport for Probe {
    type Params = TestParams;
    fn create() -> Self {
        Self {
            inner: TestPlugin::create(),
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
fn signed_note_lengths_survive_input_and_output_callbacks() {
    let instance =
        Vst3Instance::<Probe>::create(crate::lifecycle::vst3_test_host()).expect("create");
    for note_id in [-1, 73] {
        for length in [0, 512, -1, i32::MIN] {
            instance.begin_input_events_impl(64);
            let event = Vst3NativeEvent {
                kind: VST3_EVENT_NOTE_ON,
                sample_offset: 3,
                bus_index: 0,
                channel: 2,
                pitch: 60,
                note_id,
                velocity: 0.75,
                tuning: 12.5,
                length,
                ..Vst3NativeEvent::default()
            };
            assert_eq!(
                instance.push_input_event_impl(&event, None),
                VST3_EVENT_EMITTED,
                "input note_id={note_id}, length={length}"
            );
            {
                let mut audio = instance.audio.enter();
                let events = audio.event_list.clone();
                audio.output_events = events;
            }
            instance.begin_output_events_impl();
            let mut output = Vst3NativeEvent::default();
            assert_eq!(
                instance.next_output_event_impl(&mut output),
                VST3_EVENT_EMITTED,
                "output note_id={note_id}, length={length}"
            );
            assert_eq!(output.length, length);
            assert_eq!(output.tuning.to_bits(), event.tuning.to_bits());
            assert_eq!(output.sample_offset, event.sample_offset);
        }
    }
}

static SEEN_ROUTING: Mutex<Option<BusRouting>> = Mutex::new(None);

struct RoutingProbe {
    inner: TestPlugin,
}

impl PluginExport for RoutingProbe {
    type Params = TestParams;
    fn create() -> Self {
        Self {
            inner: TestPlugin::create(),
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

impl PluginRuntime for RoutingProbe {
    type Sample = f32;
    fn info() -> PluginInfo {
        TestPlugin::info()
    }
    fn reset(&mut self, _: &AudioConfig) {}
    fn process(
        &mut self,
        _: &mut AudioBuffer,
        _: &EventList,
        cx: &mut ProcessContext,
    ) -> ProcessStatus {
        *SEEN_ROUTING.lock().unwrap() = Some(cx.bus_routing);
        ProcessStatus::Normal
    }
}

#[test]
fn process_block_populates_bus_routing_for_inputs_and_outputs() {
    *SEEN_ROUTING.lock().unwrap() = None;
    let inst =
        Vst3Instance::<RoutingProbe>::create(crate::lifecycle::vst3_test_host()).expect("create");
    assert!(inst.prepare_impl(48_000.0, 64, 0));

    let in_ch = [[0.0f32; 16]; 3];
    let mut out_ch = [[0.0f32; 16]; 3];
    let in_ptrs = [
        in_ch[0].as_ptr().cast::<std::ffi::c_void>(),
        in_ch[1].as_ptr().cast::<std::ffi::c_void>(),
        in_ch[2].as_ptr().cast::<std::ffi::c_void>(),
    ];
    let mut out_ptrs = [
        out_ch[0].as_mut_ptr().cast::<std::ffi::c_void>(),
        out_ch[1].as_mut_ptr().cast::<std::ffi::c_void>(),
        out_ch[2].as_mut_ptr().cast::<std::ffi::c_void>(),
    ];
    let in_buses = [2_u32, 1];
    let out_buses = [2_u32, 1];

    inst.begin_input_events_impl(16);
    let mut args = ProcessArgs {
        inputs: &in_ptrs,
        outputs: &mut out_ptrs,
        in_bus_channels: &in_buses,
        in_bus_active: 0b01,
        out_bus_channels: &out_buses,
        out_bus_active: 0b10,
        num_frames: 16,
        transport: None,
        process_mode: 0,
    };
    // SAFETY: Every channel pointer in `args` points at a local `f32` buffer of `num_frames`
    // samples that outlives the call.
    assert!(unsafe { inst.process_block::<f32>(&mut args) });

    let routing = SEEN_ROUTING
        .lock()
        .unwrap()
        .take()
        .expect("process ran and recorded bus_routing");
    assert_eq!(routing.input_count(), 2);
    assert_eq!(routing.output_count(), 2);
    let in0 = routing.input(0).expect("input bus 0");
    assert_eq!(in0.channel_count(), 2);
    assert_eq!(in0.activation(), BusActivation::Active);
    let in1 = routing.input(1).expect("input bus 1");
    assert_eq!(in1.channel_count(), 1);
    assert_eq!(in1.activation(), BusActivation::Inactive);

    let out0 = routing.output(0).expect("output bus 0");
    assert_eq!(out0.channel_count(), 2);
    assert_eq!(out0.activation(), BusActivation::Inactive);
    let out1 = routing.output(1).expect("output bus 1");
    assert_eq!(out1.channel_count(), 1);
    assert_eq!(out1.activation(), BusActivation::Active);
}

static CONNECT_NOTIFIER: Mutex<Option<HostNotifier>> = Mutex::new(None);

struct ConnectNoticeHooks;

impl MainThreadHooks for ConnectNoticeHooks {
    fn connect(&mut self, notifier: HostNotifier) {
        notifier.notify(HostNotice::AUDIO_PORTS);
        *CONNECT_NOTIFIER.lock().unwrap() = Some(notifier);
    }
}

struct ConnectNoticePlugin {
    inner: TestPlugin,
}

impl PluginExport for ConnectNoticePlugin {
    type Params = TestParams;
    fn create() -> Self {
        Self {
            inner: TestPlugin::create(),
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
    fn main_thread(&self) -> Box<dyn MainThreadHooks> {
        Box::new(ConnectNoticeHooks)
    }
}

impl PluginRuntime for ConnectNoticePlugin {
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
fn notice_raised_during_connect_is_drained_when_catalog_waker_binds() {
    *CONNECT_NOTIFIER.lock().unwrap() = None;
    let inst = Vst3Instance::<ConnectNoticePlugin>::create(crate::lifecycle::vst3_test_host())
        .expect("create");
    assert!(
        inst.host_queue
            .pending()
            .notices()
            .contains(HostNotice::AUDIO_PORTS),
        "notice raised inside connect() must remain pending until drained"
    );
    inst.set_catalog_waker_impl(Some(Box::new(|| {})));
    inst.drain_host_queue_impl();
    assert!(
        inst.host_queue.pending().notices().is_empty(),
        "notice raised inside connect() must be drained once the host queue drains"
    );
    let notifier = CONNECT_NOTIFIER
        .lock()
        .unwrap()
        .take()
        .expect("connect ran");
    notifier.notify(HostNotice::AUDIO_PORTS);
    assert!(
        inst.host_queue
            .pending()
            .notices()
            .contains(HostNotice::AUDIO_PORTS),
        "subsequent notices must queue into host_queue"
    );
    inst.drain_host_queue_impl();
    assert!(
        inst.host_queue.pending().notices().is_empty(),
        "subsequent notices must drain cleanly through drain_host_queue"
    );
}

#[test]
fn anonymous_note_expression_prefers_active_retriggered_slot_and_overlapping_note_off_is_fifo() {
    let mut ids = OutputNoteIds::new();

    let (id1, m1) = ids
        .propose_note_on(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("first note on");
    ids.commit(m1);
    let (off1, m_off1) = ids
        .propose_note_off(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("first note off");
    assert_eq!(off1, id1);
    ids.commit(m_off1);

    assert_eq!(
        ids.note_id_for_expression(OutputSourceIdentity::Anonymous, 0, 0, 60),
        Some(id1)
    );

    let (id2, m2) = ids
        .propose_note_on(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("retriggered note on");
    assert_ne!(id1, id2);
    ids.commit(m2);

    assert_eq!(
        ids.note_id_for_expression(OutputSourceIdentity::Anonymous, 0, 0, 60),
        Some(id2)
    );

    let (id3, m3) = ids
        .propose_note_on(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("overlapping note on");
    assert_ne!(id2, id3);
    ids.commit(m3);

    let (off2, m_off2) = ids
        .propose_note_off(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("oldest overlapping note off");
    assert_eq!(off2, id2);
    ids.commit(m_off2);

    assert_eq!(
        ids.note_id_for_expression(OutputSourceIdentity::Anonymous, 0, 0, 60),
        Some(id3)
    );

    let (off3, m_off3) = ids
        .propose_note_off(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("second overlapping note off");
    assert_eq!(off3, id3);
    ids.commit(m_off3);

    assert!(
        ids.propose_note_off(OutputSourceIdentity::Anonymous, 0, 0, 60)
            .is_none(),
        "all active slots released"
    );
}

#[test]
#[allow(clippy::float_cmp)]
fn output_param_count_and_getter_filter_invalid_and_unknown_param_changes() {
    use moose_core::events::Event;

    let inst = Vst3Instance::<Probe>::create(crate::lifecycle::vst3_test_host()).expect("create");
    {
        let mut audio = inst.audio.enter();
        audio.input_num_frames = 64;
        audio.output_events.clear();
        audio.output_events.push(Event::new(
            64,
            EventBody::ParamChange { id: 0, value: -6.0 },
        ));
        audio.output_events.push(Event::new(
            4,
            EventBody::ParamChange {
                id: 0,
                value: f64::NAN,
            },
        ));
        audio.output_events.push(Event::new(
            8,
            EventBody::ParamChange {
                id: 9999,
                value: 0.5,
            },
        ));
        audio.output_events.push(Event::new(
            12,
            EventBody::ParamChange { id: 0, value: -6.0 },
        ));
    }

    assert_eq!(inst.output_param_count_impl(), 1);
    let (id, offset, value) = inst.output_param_impl(0).expect("valid output param");
    let expected_norm = inst
        .param_infos
        .iter()
        .find(|i| i.id == 0)
        .expect("param 0")
        .range
        .normalize(-6.0);
    assert_eq!(id, 0);
    assert_eq!(offset, 12);
    assert!((value - expected_norm).abs() < 1e-12);
}

static SMOKE_DROPS: AtomicUsize = AtomicUsize::new(0);

struct PanicCreatePlugin;

impl PluginExport for PanicCreatePlugin {
    type Params = TestParams;
    fn create() -> Self {
        panic!("simulated constructor failure");
    }
    fn params(&self) -> &TestParams {
        unreachable!()
    }
    fn params_arc(&self) -> Arc<TestParams> {
        unreachable!()
    }
    fn meter_store(&self) -> Arc<MeterStore> {
        unreachable!()
    }
    fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
        unreachable!()
    }
}

impl PluginRuntime for PanicCreatePlugin {
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

struct SilentDropProbe {
    inner: TestPlugin,
}

impl Drop for SilentDropProbe {
    fn drop(&mut self) {
        SMOKE_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

impl PluginExport for SilentDropProbe {
    type Params = TestParams;
    fn create() -> Self {
        Self {
            inner: TestPlugin::create(),
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

impl PluginRuntime for SilentDropProbe {
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
fn rt_paranoid_smoke_checks_null_create_and_destroys_instance_on_panic() {
    let null_err = std::panic::catch_unwind(rt_paranoid_smoke::<PanicCreatePlugin>)
        .expect_err("failed create must panic cleanly");
    let null_msg = null_err
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| null_err.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("");
    assert!(
        null_msg.contains("create returned None"),
        "expected failed-create panic message, got: {null_msg:?}"
    );

    SMOKE_DROPS.store(0, Ordering::SeqCst);
    let silent_res = std::panic::catch_unwind(rt_paranoid_smoke::<SilentDropProbe>);
    assert!(silent_res.is_err(), "silent output must trip smoke assert");
    assert_eq!(
        SMOKE_DROPS.load(Ordering::SeqCst),
        1,
        "owned Vst3Instance must drop when an assertion inside rt_paranoid_smoke panics"
    );
}

struct PanicAfterEmitPlugin {
    params: Arc<TestParams>,
    meters: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
}

impl PluginExport for PanicAfterEmitPlugin {
    type Params = TestParams;
    fn create() -> Self {
        Self {
            params: Arc::new(TestParams::default()),
            meters: MeterStore::new(),
            snapshot: SnapshotSlot::new(),
        }
    }
    fn params(&self) -> &TestParams {
        &self.params
    }
    fn params_arc(&self) -> Arc<TestParams> {
        Arc::clone(&self.params)
    }
    fn meter_store(&self) -> Arc<MeterStore> {
        Arc::clone(&self.meters)
    }
    fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
        Arc::clone(&self.snapshot)
    }
}

impl PluginRuntime for PanicAfterEmitPlugin {
    type Sample = f32;
    fn info() -> PluginInfo {
        let mut info = TestPlugin::info();
        info.emits_midi = true;
        info.midi_output_ports = 1;
        info
    }
    fn reset(&mut self, _: &AudioConfig) {}
    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _: &EventList,
        ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        for ch in 0..buffer.num_output_channels() {
            buffer.output(ch).fill(0.5);
        }
        let _ = ctx
            .output_events
            .try_push(moose_core::events::Event::on_port(
                0,
                0,
                moose_core::events::EventBody::NoteOn {
                    group: 0,
                    channel: 0,
                    note: 60,
                    velocity: 100,
                },
            ));
        panic!("boom after emitting note");
    }
}

#[test]
fn process_panic_clears_output_and_input_event_lists() {
    use crate::backend::ProcessArgs;
    use crate::input_events::VST3_EVENT_END;

    let instance = Vst3Instance::<PanicAfterEmitPlugin>::create(crate::lifecycle::vst3_test_host())
        .expect("create");
    assert!(instance.prepare_impl(48_000.0, 32, 0));

    let in_l = [0.25f32; 32];
    let in_r = [0.25f32; 32];
    let mut out_l = [1.0f32; 32];
    let mut out_r = [1.0f32; 32];
    let in_ptrs = [
        in_l.as_ptr().cast::<std::ffi::c_void>(),
        in_r.as_ptr().cast::<std::ffi::c_void>(),
    ];
    let mut out_ptrs = [
        out_l.as_mut_ptr().cast::<std::ffi::c_void>(),
        out_r.as_mut_ptr().cast::<std::ffi::c_void>(),
    ];
    let bus_channels = [2u32];
    let mut args = ProcessArgs {
        inputs: &in_ptrs,
        outputs: &mut out_ptrs,
        in_bus_channels: &bus_channels,
        in_bus_active: 1,
        out_bus_channels: &bus_channels,
        out_bus_active: 1,
        num_frames: 32,
        process_mode: 0,
        transport: None,
    };

    // SAFETY: Every channel pointer in `args` points at a local `f32` buffer of `num_frames`
    // samples that outlives the call.
    let ok = unsafe { instance.process_block::<f32>(&mut args) };
    assert!(!ok, "panicking process must report failure");
    assert!(out_l.iter().all(|&s| s == 0.0));
    assert!(out_r.iter().all(|&s| s == 0.0));

    instance.begin_output_events_impl();
    let mut out_ev = Vst3NativeEvent::default();
    assert_eq!(
        instance.next_output_event_impl(&mut out_ev),
        VST3_EVENT_END,
        "events emitted before panic must not leak to the host"
    );
    assert!(instance.audio.enter().event_list.is_empty());
}

struct PanicStateChangedEditor;

impl moose_core::editor::Editor for PanicStateChangedEditor {
    fn size(&self) -> (u32, u32) {
        (300, 200)
    }
    fn open(
        &mut self,
        _parent: moose_core::editor::RawWindowHandle,
        _ctx: moose_core::PluginContext,
    ) {
    }
    fn close(&mut self) {}
    fn state_changed(&mut self) {
        panic!("editor state_changed boom");
    }
}

#[test]
fn panicking_editor_state_changed_does_not_fail_accepted_load_or_skip_rescan() {
    use moose_core::host_queue::HostSignal;
    use moose_core::thread::MainThread;

    let instance =
        Vst3Instance::<TestPlugin>::create(crate::lifecycle::vst3_test_host()).expect("create");
    let blob = instance.save_state_bytes();

    // Install an open editor whose `state_changed` panics.
    instance.gui.enter().editor = Some(Box::new(PanicStateChangedEditor));

    assert!(
        instance.load_state_bytes(&blob),
        "editor notification panic must not turn an accepted state load into failure"
    );

    // SAFETY: test thread stands in for the host UI thread.
    let main = unsafe { MainThread::assume() };
    let drain = instance.host_queue.drain(&main);
    assert!(
        drain.signals.contains(HostSignal::PARAM_RESCAN),
        "PARAM_RESCAN must still be signaled even when editor.state_changed panics"
    );
}
