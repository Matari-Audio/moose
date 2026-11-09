use super::host_value_fixture::{TestParams, TestPlugin};
use super::*;
use moose_core::buffer::AudioBuffer;
use moose_core::process::{ProcessContext, ProcessStatus};

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
