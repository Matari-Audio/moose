//! Regression test: `ParamChange` events carry PLAIN values.
//! `StaticShell` must use `set_plain()`, not `set_normalized()`.
//! Bug: double-denormalization caused gain to slam to extremes in VST3.

use moose::{Lifecycle, Plugin};
use moose_core::AudioConfig;
use moose_core::buffer::AudioBuffer;
use moose_core::events::{Event, EventBody, EventList, TransportInfo};
use moose_core::plugin::{
    PluginRuntime, PreparationConfig, PreparationError, PreparationFrameBound,
};
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::tasks::InitContext;
use moose_derive::Params;
use moose_params::{FloatParamReadF32, Params};
use std::sync::Arc;

#[derive(Params)]
struct TestParams {
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)", unit = "dB")]
    gain: moose_params::FloatParam,
}

struct TestPlugin;

#[derive(Default)]
struct TestDspState {
    last_gain_plain: f64,
}

impl Plugin for TestPlugin {
    type Shared = TestParams;
    type MainThread = ();
    type Processor = TestDspState;

    fn main_thread(_shared: &Arc<TestParams>) {}

    fn activate(
        shared: &TestParams,
        config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<TestDspState, PreparationError> {
        shared.set_sample_rate(config.sample_rate);
        Ok(TestDspState::default())
    }

    fn process(
        state: &mut TestDspState,
        params: &TestParams,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        // Record what the plugin sees as the gain value.
        state.last_gain_plain = f64::from(params.gain.value());
        ProcessStatus::Normal
    }

    fn editor(_params: Arc<TestParams>) -> Box<dyn moose::prelude::Editor> {
        // Param-sync test; the editor slot is never opened.
        Box::new(NoEditor)
    }
}

struct NoEditor;
impl moose::prelude::Editor for NoEditor {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn open(&mut self, _: moose_core::editor::RawWindowHandle, _: moose::prelude::PluginContext) {}
    fn close(&mut self) {}
    fn idle(&mut self) {}
}

#[test]
fn plain_param_not_double_denormalized() {
    // Simulate what format wrappers do: send a PLAIN value in ParamChange.
    // The shell must use set_plain, not set_normalized.
    // If it uses set_normalized, -27.0 dB would be treated as normalized
    // and denormalized to -60 + (-27 * 66) = way out of range.

    let params = Arc::new(TestParams::new());
    let mut shell =
        moose_loader::static_shell::StaticShell::<TestParams, Lifecycle<TestPlugin>>::from_parts(
            params, None, None,
        );
    shell
        .prepare(&PreparationConfig::new(
            AudioConfig::new(44100.0, 512),
            PreparationFrameBound::ExplicitDriver,
        ))
        .unwrap();

    let input = vec![0.5f32; 512];
    let mut output = vec![0.0f32; 512];
    let inputs: Vec<&[f32]> = vec![&input];
    let mut outputs: Vec<&mut [f32]> = vec![&mut output];
    // SAFETY: `from_slices` requires every slice to be valid for the buffer lifetime and at least
    // `num_samples` long, and no input to alias an output. These are distinct local `Vec`s/arrays
    // of exactly that length.
    let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 512) };

    // ParamChange with PLAIN value -27.0 dB (this is what VST3/CLAP wrappers send).
    let mut events = EventList::default();
    events.push(Event::new(
        0,
        EventBody::ParamChange {
            id: 0,
            value: -27.0,
        },
    ));

    let transport = TransportInfo::default();
    let mut output_events = EventList::default();
    let param_fn = |_id: u32| -> f64 { 0.0 };
    let meter_fn = |_id: u32, _v: f32| {};
    let mut ctx = ProcessContext::new(&transport, 44100.0, 512, &mut output_events)
        .with_params(&param_fn)
        .with_meters(&meter_fn);

    shell.process(&mut buffer, &events, &mut ctx);

    // The plugin should see -27.0 dB (the plain value), NOT some
    // double-denormalized extreme.
    let gain = shell.state_ref().processor().unwrap().last_gain_plain;
    assert!(
        (gain - (-27.0)).abs() < 0.1,
        "Expected gain ≈ -27.0 dB, got {gain}. Likely double-denormalization bug."
    );
}
