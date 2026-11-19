//! Stereo utility: independent gain, polarity, and delay per side.
//!
//! The left and right channels are processed by the *same* control
//! group, so `ChannelStrip` is declared once and pulled in twice with
//! `#[nested]`. Bare `#[nested]` (no base) rebases each slot to its own
//! distinct id range derived from the field name, so `left` and `right`
//! resolve to separate controls with no ids or bases written by hand.

use moose::prelude::*;
use moose_gui::IntoLayoutEditor;
use moose_gui_types::layout::{GridLayout, knob, section, toggle};

use std::sync::Arc;

/// Longest per-side delay, in milliseconds. Sizes the delay lines at
/// `reset()`.
const MAX_DELAY_MS: f64 = 50.0;

/// One side's controls. Declared once, reused for both channels.
#[derive(Params)]
pub struct ChannelStrip {
    #[param(
        name = "Gain",
        range = "linear(-60, 12)",
        default = 0.0,
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,

    #[param(name = "Invert")]
    pub invert: BoolParam,

    #[param(name = "Delay", range = "linear(0, 50)", default = 0.0, unit = "ms")]
    pub delay: FloatParam,
}

#[derive(Params)]
pub struct StereoUtilityParams {
    #[nested]
    pub left: ChannelStrip,
    #[nested]
    pub right: ChannelStrip,
}

/// Stateless descriptor - DSP state lives in [`StereoUtilityProcessor`].
pub struct StereoUtility;

/// Per-instance DSP state: the delay lines and their write heads.
pub struct StereoUtilityProcessor {
    smoothing: Smoothing,
    /// One delay line per channel, sized in `prepare()` for `MAX_DELAY_MS`.
    lines: [Vec<f32>; 2],
    write_pos: [usize; 2],
    line_len: usize,
    sample_rate: f64,
}

/// Delay-line length for `MAX_DELAY_MS` at `sr`, plus one slot so a
/// full-range delay still reads a sample behind the write head.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn delay_line_len(sr: f64) -> usize {
    (MAX_DELAY_MS * sr / 1000.0).ceil() as usize + 1
}

/// Milliseconds to a whole-sample delay, clamped into the line. Reads
/// the raw target (block-rate), so the inner loop holds one offset.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ms_to_samples(ms: f64, sr: f64, line_len: usize) -> usize {
    let s = (ms * sr / 1000.0).round();
    if s <= 0.0 {
        0
    } else {
        (s as usize).min(line_len - 1)
    }
}

/// The processor's ramp for each smoothed parameter. Plain state owned by
/// the audio side: only each parameter's target crosses threads.
#[derive(Clone, Copy, Debug, Default)]
pub struct Smoothing {
    left_gain: ParamSmoother,
    right_gain: ParamSmoother,
}

impl Smoothing {
    /// Jumps every ramp to its target at `sample_rate`. Never allocates.
    fn reset(&mut self, params: &StereoUtilityParams, sample_rate: f64) {
        self.left_gain.reset(&params.left.gain, sample_rate);
        self.right_gain.reset(&params.right.gain, sample_rate);
    }
}

impl Plugin for StereoUtility {
    type Shared = StereoUtilityParams;
    type MainThread = ();
    type Processor = StereoUtilityProcessor;

    fn main_thread(_shared: &Arc<StereoUtilityParams>) {}

    fn activate(
        params: &StereoUtilityParams,
        config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<StereoUtilityProcessor, PreparationError> {
        let mut smoothing = Smoothing::default();
        smoothing.reset(params, config.sample_rate);
        let len = delay_line_len(config.sample_rate);
        Ok(StereoUtilityProcessor {
            smoothing,
            sample_rate: config.sample_rate,
            lines: [vec![0.0; len], vec![0.0; len]],
            write_pos: [0; 2],
            line_len: len,
        })
    }

    fn reset(processor: &mut StereoUtilityProcessor, params: &StereoUtilityParams) {
        processor.smoothing.reset(params, processor.sample_rate);
        for line in &mut processor.lines {
            line.fill(0.0);
        }
        processor.write_pos = [0; 2];
    }

    fn process(
        state: &mut StereoUtilityProcessor,
        params: &StereoUtilityParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let sr = state.sample_rate;
        let len = state.line_len;

        for ch in 0..buffer.channels().min(2) {
            let strip = if ch == 0 { &params.left } else { &params.right };
            // Delay distance and polarity are read per block; gain stays
            // per-sample smoothed so a fader move is click-free.
            let delay_samples = ms_to_samples(f64::from(strip.delay.value()), sr, len);
            let sign = if strip.invert.value() { -1.0 } else { 1.0 };

            let line = &mut state.lines[ch];
            let mut wp = state.write_pos[ch];
            let (inp, out) = buffer.io(ch);
            for i in 0..inp.len() {
                line[wp] = inp[i];
                let read = (wp + len - delay_samples) % len;
                let smoother = if ch == 0 {
                    &mut state.smoothing.left_gain
                } else {
                    &mut state.smoothing.right_gain
                };
                let gain = db_to_linear(smoother.next(&strip.gain));
                out[i] = line[read] * gain * sign;
                wp = (wp + 1) % len;
            }
            state.write_pos[ch] = wp;
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<StereoUtilityParams>) -> Box<dyn Editor> {
        // Reused-group params are addressed by their flattened id, read
        // off each side, so `left` and `right` resolve to distinct
        // controls despite sharing the `ChannelStrip` type.
        GridLayout::build(vec![
            section(
                "LEFT",
                vec![
                    knob(params.left.gain.id(), "Gain"),
                    knob(params.left.delay.id(), "Delay"),
                    toggle(params.left.invert.id(), "Invert"),
                ],
            ),
            section(
                "RIGHT",
                vec![
                    knob(params.right.gain.id(), "Gain"),
                    knob(params.right.delay.id(), "Delay"),
                    toggle(params.right.invert.id(), "Invert"),
                ],
            ),
        ])
        .with_title("STEREO UTILITY")
        .into_editor(&params)
    }
}

moose::plugin! {
    plugin: StereoUtility,
    params: StereoUtilityParams,
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
                    s.set_param(ChannelStripParamId::Gain, 0.9);
                    s.wait_ms(15);
                    s.set_param(ChannelStripParamId::Gain, 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

    #[test]
    fn info_is_valid() {
        moose_test::assert_valid_info::<PluginShell>();
    }

    #[test]
    fn channels_reuse_one_strip_with_distinct_ids() {
        use std::collections::HashSet;
        // The headline: one `ChannelStrip` type in two `#[nested]` slots,
        // each rebased to a disjoint id range so left/right stay separate
        // controls despite sharing the type.
        let p = StereoUtilityParams::new();
        let ids: HashSet<u32> = [
            p.left.gain.id(),
            p.left.invert.id(),
            p.left.delay.id(),
            p.right.gain.id(),
            p.right.invert.id(),
            p.right.delay.id(),
        ]
        .into_iter()
        .collect();
        assert_eq!(ids.len(), 6, "reused strips must not share ids");
        assert_eq!(p.count(), 6);
    }

    #[test]
    fn default_passes_audio_at_unity() {
        use moose_test::{InputSource, assertions, driver};
        use std::time::Duration;
        // Defaults are transparent: 0 dB, 0 ms, non-inverted.
        let result = driver!(PluginShell)
            .duration(Duration::from_millis(12))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        let max = result.output[0]
            .iter()
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        assert!(max > 0.45, "unity passthrough expected, got {max}");
    }

    #[test]
    fn has_editor() {
        moose_test::assert_has_editor::<PluginShell>();
    }

    #[test]
    fn state_round_trips() {
        moose_test::assert_state_round_trip::<PluginShell>();
    }

    // --- AU metadata ---

    #[test]
    fn au_type_codes_ascii() {
        moose_test::assert_au_type_codes_ascii::<PluginShell>();
    }

    #[test]
    fn fourcc_roundtrip() {
        moose_test::assert_fourcc_roundtrip::<PluginShell>();
    }

    #[test]
    fn bus_config_effect() {
        moose_test::assert_bus_config_effect::<PluginShell>();
    }

    // --- GUI lifecycle ---

    #[test]
    fn editor_lifecycle() {
        moose_test::assert_editor_lifecycle::<PluginShell>();
    }

    #[test]
    fn editor_size_consistent() {
        moose_test::assert_editor_size_consistent::<PluginShell>();
    }

    // --- Parameters ---

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
    fn param_count_matches() {
        moose_test::assert_param_count_matches::<PluginShell>();
    }

    #[test]
    fn no_duplicate_param_ids() {
        moose_test::assert_no_duplicate_param_ids::<PluginShell>();
    }

    // --- State resilience ---

    #[test]
    fn corrupt_state_no_crash() {
        moose_test::assert_corrupt_state_no_crash::<PluginShell>();
    }

    #[test]
    fn empty_state_no_crash() {
        moose_test::assert_empty_state_no_crash::<PluginShell>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        moose_test::screenshot!(PluginShell, "screenshots/stereo_utility_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        moose_test::screenshot!(PluginShell, "screenshots/stereo_utility_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        moose_test::screenshot!(
            PluginShell,
            "screenshots/stereo_utility_default_windows.png"
        )
        .pixel_threshold(2)
        .run();
    }
}
