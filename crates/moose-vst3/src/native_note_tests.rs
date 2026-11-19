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
    // SAFETY: the test owns the callback context and all event pointers until destroy.
    unsafe {
        let ctx = cb_create::<Probe>();
        let instance = &*ctx.cast::<Vst3Instance<Probe>>();
        for note_id in [-1, 73] {
            for length in [0, 512, -1, i32::MIN] {
                cb_begin_input_events::<Probe>(ctx, 64);
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
                    cb_push_input_event::<Probe>(ctx, &raw const event),
                    VST3_EVENT_EMITTED,
                    "input note_id={note_id}, length={length}"
                );
                {
                    let mut audio = instance.audio.enter();
                    let events = audio.event_list.clone();
                    audio.output_events = events;
                }
                cb_begin_output_events::<Probe>(ctx);
                let mut output = Vst3NativeEvent::default();
                assert_eq!(
                    cb_next_output_event::<Probe>(ctx, &raw mut output),
                    VST3_EVENT_EMITTED,
                    "output note_id={note_id}, length={length}"
                );
                assert_eq!(output.length, length);
                assert_eq!(output.tuning.to_bits(), event.tuning.to_bits());
                assert_eq!(output.sample_offset, event.sample_offset);
            }
        }
        cb_destroy::<Probe>(ctx);
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
    unsafe {
        *SEEN_ROUTING.lock().unwrap() = None;
        let ctx = cb_create::<RoutingProbe>();
        assert!(!ctx.is_null());
        assert_eq!(cb_prepare::<RoutingProbe>(ctx, 48_000.0, 64, 0), 1);

        let in_ch = [[0.0f32; 16]; 3];
        let mut out_ch = [[0.0f32; 16]; 3];
        let in_ptrs = [in_ch[0].as_ptr(), in_ch[1].as_ptr(), in_ch[2].as_ptr()];
        let mut out_ptrs = [
            out_ch[0].as_mut_ptr(),
            out_ch[1].as_mut_ptr(),
            out_ch[2].as_mut_ptr(),
        ];
        let in_buses = [2_u32, 1];
        let out_buses = [2_u32, 1];

        cb_begin_input_events::<RoutingProbe>(ctx, 16);
        assert_eq!(
            process_block::<RoutingProbe, f32>(
                ctx,
                in_ptrs.as_ptr(),
                out_ptrs.as_mut_ptr(),
                3,
                3,
                in_buses.as_ptr(),
                2,
                0b01,
                out_buses.as_ptr(),
                2,
                0b10,
                16,
                std::ptr::null(),
                0,
            ),
            1
        );

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

        cb_destroy::<RoutingProbe>(ctx);
    }
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
    unsafe extern "C" fn noop_wake(_: *mut std::ffi::c_void) {}
    unsafe {
        *CONNECT_NOTIFIER.lock().unwrap() = None;
        let ctx = cb_create::<ConnectNoticePlugin>();
        assert!(!ctx.is_null());
        let inst = &*ctx.cast::<Vst3Instance<ConnectNoticePlugin>>();
        // Notice raised during `connect()` stays pending until the COM
        // `Host` finishes registering `ctx` and binds its catalog waker.
        cb_set_catalog_waker::<ConnectNoticePlugin>(ctx, Some(noop_wake), std::ptr::null_mut());
        assert!(
            inst.host_notifier.take().is_empty(),
            "notice raised inside connect() must be drained once the catalog waker binds"
        );
        let notifier = CONNECT_NOTIFIER
            .lock()
            .unwrap()
            .take()
            .expect("connect ran");
        notifier.notify(HostNotice::AUDIO_PORTS);
        assert!(
            inst.host_notifier.take().is_empty(),
            "subsequent notices continue to drain through the published notice_ctx"
        );
        cb_destroy::<ConnectNoticePlugin>(ctx);
    }
}

#[test]
fn anonymous_note_expression_prefers_active_retriggered_slot_and_overlapping_note_off_is_fifo() {
    let mut ids = OutputNoteIds::new();

    // First NoteOn on (bus 0, ch 0, pitch 60), then released.
    let (id1, m1) = ids
        .propose_note_on(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("first note on");
    ids.commit(m1);
    let (off1, m_off1) = ids
        .propose_note_off(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("first note off");
    assert_eq!(off1, id1);
    ids.commit(m_off1);

    // While only a single released slot exists, expression fallback still resolves it.
    assert_eq!(
        ids.note_id_for_expression(OutputSourceIdentity::Anonymous, 0, 0, 60),
        Some(id1)
    );

    // Retrigger on the same (bus 0, ch 0, pitch 60) while the released slot is still present.
    let (id2, m2) = ids
        .propose_note_on(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("retriggered note on");
    assert_ne!(id1, id2);
    ids.commit(m2);

    // Expression lookup must prefer the unique active slot rather than failing due to the released slot.
    assert_eq!(
        ids.note_id_for_expression(OutputSourceIdentity::Anonymous, 0, 0, 60),
        Some(id2)
    );

    // Overlapping second active anonymous NoteOn on the same (bus 0, ch 0, pitch 60).
    let (id3, m3) = ids
        .propose_note_on(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("overlapping note on");
    assert_ne!(id2, id3);
    ids.commit(m3);

    // NoteOff must release the oldest active slot (FIFO by age: id2 first, then id3), not return None.
    let (off2, m_off2) = ids
        .propose_note_off(OutputSourceIdentity::Anonymous, 0, 0, 60)
        .expect("oldest overlapping note off");
    assert_eq!(off2, id2);
    ids.commit(m_off2);

    // Now id3 is the unique active slot, so expression lookup resolves to id3.
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
    unsafe {
        let ctx = cb_create::<Probe>();
        let inst = &*ctx.cast::<Vst3Instance<Probe>>();
        {
            let mut audio = inst.audio.enter();
            audio.input_num_frames = 64;
            audio.output_events.clear();
            // 1. Out-of-bounds sample_offset (>= input_num_frames) -> filtered out.
            audio.output_events.push(Event::new(
                64,
                EventBody::ParamChange { id: 0, value: -6.0 },
            ));
            // 2. Non-finite value -> filtered out.
            audio.output_events.push(Event::new(
                4,
                EventBody::ParamChange {
                    id: 0,
                    value: f64::NAN,
                },
            ));
            // 3. Unknown param ID -> filtered out.
            audio.output_events.push(Event::new(
                8,
                EventBody::ParamChange {
                    id: 9999,
                    value: 0.5,
                },
            ));
            // 4. Valid param ID 0 (range -24..=12, midpoint -6.0 -> normalized 0.5).
            audio.output_events.push(Event::new(
                12,
                EventBody::ParamChange { id: 0, value: -6.0 },
            ));
        }

        assert_eq!(cb_get_output_param_count::<Probe>(ctx), 1);
        let (mut id, mut offset, mut value) = (999u32, -1i32, -1.0f64);
        cb_get_output_param::<Probe>(ctx, 0, &raw mut id, &raw mut offset, &raw mut value);
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

        cb_destroy::<Probe>(ctx);
    }
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
        // Leaves outputs zero, triggering the post-process assertion in rt_paranoid_smoke.
        ProcessStatus::Normal
    }
}

#[test]
fn rt_paranoid_smoke_checks_null_create_and_destroys_instance_on_panic() {
    let null_err = std::panic::catch_unwind(rt_paranoid_smoke::<PanicCreatePlugin>)
        .expect_err("null cb_create must panic cleanly");
    let null_msg = null_err
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| null_err.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("");
    assert!(
        null_msg.contains("cb_create returned null"),
        "expected null-create panic message, got: {null_msg:?}"
    );

    SMOKE_DROPS.store(0, Ordering::SeqCst);
    let silent_res = std::panic::catch_unwind(rt_paranoid_smoke::<SilentDropProbe>);
    assert!(silent_res.is_err(), "silent output must trip smoke assert");
    assert_eq!(
        SMOKE_DROPS.load(Ordering::SeqCst),
        1,
        "DestroyGuard must run cb_destroy when an assertion inside rt_paranoid_smoke panics"
    );
}
