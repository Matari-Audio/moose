//! Parallel-saturation dry/wet plugin.
//!
//! Generates a "wet" signal by running the input through a
//! `tanh_block` saturator (after a fixed-ish drive boost), then
//! blends dry + wet with `mix_block` at the user-controlled ratio.
//!
//! The point of this example is `mix_block`: dry/wet cross-fade is
//! its canonical use case, and no other example in the tree calls
//! it. `tanh_block` shows up here too as the cheapest "interesting"
//! signal-shape transformation to generate a wet that's audibly
//! different from the dry.
//!
//! `mix_block`'s gain coefficients are *scalar per call*, so a
//! per-sample mix envelope doesn't apply directly. We read the
//! smoothed `mix` value once per audio block
//! (zero-order hold within the block; the smoother handles
//! block-to-block transitions). For audio mixing this is
//! indistinguishable from per-sample interpolation at typical
//! smoothing times.

use moose::prelude::*;
use moose_gui::IntoLayoutEditor;
use moose_gui_types::layout::{GridLayout, knob, meter, widgets};
use moose_simd::{math, ops};

use DryWetParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct DryWetParams {
    #[param(
        name = "Drive",
        range = "linear(0, 24)",
        default = 6.0,
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub drive: FloatParam,

    #[param(
        name = "Mix",
        range = "linear(0, 1)",
        default = 0.5,
        unit = "%",
        smooth = "exp(20)"
    )]
    pub mix: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

/// Descriptor; the SIMD scratch lives in [`DryWetProcessor`], sized in `prepare`.
pub struct DryWet;

/// Per-instance SIMD scratch, sized to the host's max block in `reset`.
pub struct DryWetProcessor {
    smoothing: Smoothing,
    /// Holds the saturated signal; `tanh_block` wants distinct in/out,
    /// then `mix_block` folds wet and dry into out in one SIMD pass.
    wet: Vec<f32>,
    driven: Vec<f32>,
    /// Rate the processor was activated at, for re-seating the ramps in `reset`.
    sample_rate: f64,
}

/// The processor's ramp for each smoothed parameter. Plain state owned by
/// the audio side: only each parameter's target crosses threads.
#[derive(Clone, Copy, Debug, Default)]
pub struct Smoothing {
    drive: ParamSmoother,
    mix: ParamSmoother,
}

impl Smoothing {
    /// Jumps every ramp to its target at `sample_rate`. Never allocates.
    fn reset(&mut self, params: &DryWetParams, sample_rate: f64) {
        self.drive.reset(&params.drive, sample_rate);
        self.mix.reset(&params.mix, sample_rate);
    }
}

impl Plugin for DryWet {
    type Shared = DryWetParams;
    type MainThread = ();
    type Processor = DryWetProcessor;

    fn main_thread(_shared: &Arc<DryWetParams>) {}

    fn activate(
        params: &DryWetParams,
        config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<DryWetProcessor, PreparationError> {
        let mut smoothing = Smoothing::default();
        smoothing.reset(params, config.sample_rate);
        let n = config.max_block_size;
        Ok(DryWetProcessor {
            smoothing,
            sample_rate: config.sample_rate,
            wet: vec![0.0; n],
            driven: vec![0.0; n],
        })
    }

    fn reset(processor: &mut DryWetProcessor, params: &DryWetParams) {
        processor.smoothing.reset(params, processor.sample_rate);
        processor.wet.fill(0.0);
        processor.driven.fill(0.0);
    }

    fn process(
        state: &mut DryWetProcessor,
        params: &DryWetParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Both mix and drive are applied block-constant, so
        // `next_after(n)` advances each smoother by the whole block
        // in one atomic pair - matching the per-block consumption
        // pattern instead of silently downsampling smoother
        // convergence to once-per-block.
        let n = buffer.num_samples();
        let mix = state.smoothing.mix.next_after(&params.mix, n);
        let dry_g = 1.0 - mix;
        let wet_g = mix;

        // Drive treated as block-constant. For an example demoing
        // mix_block, the dB → linear conversion is a one-line
        // call; envelope precompute is over-engineering for two
        // gain stages.
        let drive_lin = db_to_linear(state.smoothing.drive.next_after(&params.drive, n));

        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            let n = inp.len();
            let driven = &mut state.driven[..n];
            let wet = &mut state.wet[..n];
            ops::scale_block(driven, inp, drive_lin); // driven = inp * drive
            math::tanh_block(wet, driven); // wet = tanh(driven)
            ops::mix_block(out, inp, dry_g, wet, wet_g); // out = inp * dry_g + wet * wet_g
        }

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterLeft, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterRight, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<DryWetParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Drive, "Drive").at(0, 0),
            knob(P::Mix, "Mix").at(0, 1),
            meter(&[P::MeterLeft, P::MeterRight], "Level")
                .at(1, 0)
                .rows(2),
        ])])
        .with_title("DRY/WET")
        .into_editor(&params)
    }
}

moose::plugin! {
    plugin: DryWet,
    params: DryWetParams,
}

moose::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_is_allocation_free() {
        use moose_test::{InputSource, assert_no_audio_alloc, driver};
        use std::time::Duration;
        assert_no_audio_alloc(|| {
            driver!(PluginShell)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .script(|s| {
                    s.set_param(P::Drive, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Drive, 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

    #[test]
    fn large_block_processes_full_buffer() {
        use moose_test::{InputSource, assertions, driver};
        use std::time::Duration;
        // A block larger than the former hard-coded 1024 scratch. The whole
        // block must be written, not truncated at 1024 samples.
        let result = driver!(PluginShell)
            .block_size(2048)
            .duration(Duration::from_millis(100))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        let ch0 = &result.output[0];
        assert!(ch0.len() >= 2048);
        assert!(
            ch0[1024..2048].iter().all(|&s| s != 0.0),
            "output past 1024 samples was not processed"
        );
    }

    #[test]
    fn info_is_valid() {
        moose_test::assert_valid_info::<PluginShell>();
    }

    #[test]
    fn renders_nonzero_output() {
        use moose_test::{InputSource, assertions, driver};
        use std::time::Duration;
        let result = driver!(PluginShell)
            .duration(Duration::from_millis(12))
            .input(InputSource::Constant(0.3))
            .run();
        assertions::assert_nonzero(&result);
        assertions::assert_no_nans(&result);
    }

    #[test]
    fn has_editor() {
        moose_test::assert_has_editor::<PluginShell>();
    }

    #[test]
    fn state_round_trips() {
        moose_test::assert_state_round_trip::<PluginShell>();
    }

    #[test]
    fn param_defaults_match() {
        moose_test::assert_param_defaults_match::<PluginShell>();
    }

    #[test]
    fn param_normalized_clamped() {
        moose_test::assert_param_normalized_clamped::<PluginShell>();
    }

    #[test]
    fn param_normalized_roundtrip() {
        moose_test::assert_param_normalized_roundtrip::<PluginShell>();
    }

    #[test]
    fn no_duplicate_param_ids() {
        moose_test::assert_no_duplicate_param_ids::<PluginShell>();
    }

    #[test]
    fn output_within_bounds() {
        use moose_test::{InputSource, assertions, driver};
        use std::time::Duration;
        let result = driver!(PluginShell)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.4))
            .run();
        assertions::assert_no_nans(&result);
        // dry/wet sum can lift signal above input briefly during
        // smoother transitions; keep ceiling at 1.0 (tanh-bounded).
        assertions::assert_peak_below(&result, 1.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        moose_test::screenshot!(PluginShell, "screenshots/block_drywet_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        moose_test::screenshot!(PluginShell, "screenshots/block_drywet_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        moose_test::screenshot!(PluginShell, "screenshots/block_drywet_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
