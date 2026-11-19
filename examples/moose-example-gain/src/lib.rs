use moose::prelude::*;
use moose_gui::IntoLayoutEditor;
use moose_gui_types::layout::{GridLayout, knob, meter, widgets, xy_pad};

// --- Parameters ---

use GainParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct GainParams {
    #[param(
        name = "Gain",
        range = "linear(-60, 6)",
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,

    #[param(name = "Pan", range = "linear(-1, 1)", unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

// --- Plugin ---

/// The plugin descriptor; its DSP state is the parameter smoothing.
pub struct Gain;

/// The processor's ramp for each smoothed parameter. Plain state owned by
/// the audio side: only each parameter's target crosses threads.
#[derive(Clone, Copy, Debug, Default)]
pub struct Smoothing {
    gain: ParamSmoother,
    pan: ParamSmoother,
    /// Rate the processor was activated at, for re-seating the ramps in `reset`.
    sample_rate: f64,
}

impl Smoothing {
    /// Jumps every ramp to its target at `sample_rate`. Never allocates.
    fn reset(&mut self, params: &GainParams, sample_rate: f64) {
        self.gain.reset(&params.gain, sample_rate);
        self.pan.reset(&params.pan, sample_rate);
    }
}

impl Plugin for Gain {
    type Shared = GainParams;
    type MainThread = ();
    type Processor = Smoothing;

    fn main_thread(_shared: &Arc<GainParams>) {}

    fn activate(
        shared: &GainParams,
        config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<Smoothing, PreparationError> {
        let mut smoothing = Smoothing {
            sample_rate: config.sample_rate,
            ..Smoothing::default()
        };
        smoothing.reset(shared, config.sample_rate);
        Ok(smoothing)
    }

    fn reset(state: &mut Smoothing, params: &GainParams) {
        state.reset(params, state.sample_rate);
    }

    fn process(
        state: &mut Smoothing,
        params: &GainParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let gain_db = state.gain.next(&params.gain);
            let pan = state.pan.next(&params.pan);
            let gain_linear = db_to_linear(gain_db);

            let gain_l = gain_linear * (1.0 - pan.max(0.0));
            let gain_r = gain_linear * (1.0 + pan.min(0.0));

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                let g = if ch == 0 { gain_l } else { gain_r };
                out[i] = inp[i] * g;
            }
        }

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterLeft, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterRight, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<GainParams>) -> Box<dyn Editor> {
        // Knob row at top-left, XY pad below them, meter pinned to
        // column 2 spanning three rows. With `.resizable(true)`,
        // host-driven resize snaps to a whole-cell width and
        // reflows the grid via `GridLayout::refit_cols`: auto-flow
        // widgets repack into the new column count and the height
        // re-computes from the new layout. The explicit `.at(...)`
        // placements stay put, so `min_cols(3)` keeps the meter's
        // column reachable at any host-allowed size.
        GridLayout::build(vec![widgets(vec![
            knob(P::Gain, "Gain").at(0, 0),
            knob(P::Pan, "Pan").at(1, 0),
            xy_pad(P::Pan, P::Gain, "XY").at(0, 1),
            meter(&[P::MeterLeft, P::MeterRight], "Level")
                .at(2, 0)
                .rows(3),
        ])])
        .with_title("GAIN")
        // `build()` derives `cols` from the widest section's widget
        // count (4 for this layout: knob + knob + xy_pad + meter),
        // which is wider than the rightmost widget edge (col 2 -
        // span 1 = 3 cols). Pin to 3 explicitly so the natural
        // size is tight to the actual content and `refit_cols`
        // snaps from there.
        .with_cols(3)
        .resizable(true)
        // Cell-count bounds on (cols, rows). Call shape matches the
        // other backends' `min_size` / `max_size` builders; the unit
        // is cells because the grid snaps to whole cells anyway.
        // 3 cols / 3 rows is the floor (the meter at col 2 spans 3
        // rows, so dropping below 3 would clip it); 8 / 6 caps the
        // stretch at a size the layout still reads as tight.
        .min_size((3, 3))
        .max_size((8, 6))
        .into_editor(&params)
    }
}

moose::plugin! {
    plugin: Gain,
    params: GainParams,
}

moose::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;

    /// Enable dealloc flagging around a wrapper-glue smoke check so its
    /// count covers frees as well as allocations, then restore the setting.
    /// Gated like its only callers, the per-format wrapper-glue tests
    /// below.
    #[cfg(any(feature = "clap", feature = "vst3", feature = "au"))]
    fn wrapper_glue_violations(smoke: impl FnOnce() -> u32) -> u32 {
        let prev = moose::rt::check_dealloc();
        moose::rt::set_check_dealloc(true);
        let n = smoke();
        moose::rt::set_check_dealloc(prev);
        n
    }

    #[test]
    fn process_is_realtime_clean() {
        use moose_test::{InputSource, assert_realtime_clean, driver};
        use std::time::Duration;
        assert_realtime_clean(|| {
            driver!(PluginShell)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .script(|s| {
                    s.set_param(P::Gain, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Gain, 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

    /// The CLAP wrapper's per-block glue (host-event conversion,
    /// transport, `process`, output narrow, snapshot publish) is
    /// allocation-free on the audio thread. Drives the real `process`
    /// vtable callback under the checker, not just the plugin.
    /// Compiled only with `clap` on (a default feature).
    #[cfg(feature = "clap")]
    #[test]
    fn clap_wrapper_glue_is_allocation_free() {
        assert_eq!(
            wrapper_glue_violations(moose_clap::rt_paranoid_smoke::<PluginShell>),
            0,
            "the CLAP wrapper's per-block glue must not allocate or free on the audio thread"
        );
    }

    #[cfg(feature = "vst3")]
    #[test]
    fn vst3_wrapper_glue_is_allocation_free() {
        assert_eq!(
            wrapper_glue_violations(moose_vst3::rt_paranoid_smoke::<PluginShell>),
            0,
            "the VST3 wrapper's per-block glue must not allocate or free on the audio thread"
        );
    }

    /// The AU wrapper's per-block glue is allocation-free on the audio
    /// thread. Drives the real render callback under the checker.
    #[cfg(feature = "au")]
    #[test]
    fn au_wrapper_glue_is_allocation_free() {
        assert_eq!(
            wrapper_glue_violations(moose_au::rt_paranoid_smoke::<PluginShell>),
            0,
            "the AU wrapper's per-block glue must not allocate or free on the audio thread"
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
            .input(InputSource::Constant(0.5))
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

    // State-load routing without a `migrate_state` override (this
    // plugin keeps the default): a matching envelope loads; foreign
    // bytes, a future envelope version, and a renamed plugin's
    // envelope all fail the load instead of resetting to defaults or
    // eating garbage.
    #[test]
    fn state_load_routing_defaults() {
        use moose_core::plugin::PluginRuntime;
        use moose_core::state::{
            PluginFormat, parse_or_migrate, serialize_state, shared_plugin_state_hash,
        };
        let hash = shared_plugin_state_hash(&PluginShell::info());

        let envelope = serialize_state(hash, &[0], &[-6.0], &[], &[]);
        let loaded = parse_or_migrate::<PluginShell>(&envelope, hash, PluginFormat::Clap, None)
            .expect("own envelope must load");
        assert_eq!(loaded.params, vec![(0, -6.0)]);

        // Foreign bytes must fail without a hook.
        moose_test::assert_state_migration_rejected::<PluginShell>(
            PluginFormat::Clap,
            None,
            b"{\"legacy\":true}",
        );

        // A future envelope version must fail, never reach the hook.
        let mut future = serialize_state(hash, &[], &[], &[], &[]);
        future[4..8].copy_from_slice(&99u32.to_le_bytes());
        moose_test::assert_state_migration_rejected::<PluginShell>(
            PluginFormat::Clap,
            None,
            &future,
        );

        // A wrong-plugin envelope must fail without a hook.
        let renamed = serialize_state(hash ^ 1, &[0], &[-6.0], &[], &[]);
        moose_test::assert_state_migration_rejected::<PluginShell>(
            PluginFormat::Clap,
            None,
            &renamed,
        );
    }

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

    #[test]
    fn editor_lifecycle() {
        moose_test::assert_editor_lifecycle::<PluginShell>();
    }

    #[test]
    fn editor_size_consistent() {
        moose_test::assert_editor_size_consistent::<PluginShell>();
    }

    #[test]
    fn param_defaults_match() {
        moose_test::assert_param_defaults_match::<PluginShell>();
    }

    /// Sample-accurate chunking: a `set_param` scheduled mid-block
    /// shouldn't show up in the output until its sample offset.
    ///
    /// We script a step from +6 dB (gain=1.0) to -60 dB (gain~=0.001)
    /// at sample 4800 of a 9600-sample (200 ms) run @ 48 kHz, with a
    /// constant unit-DC input. With the chunker doing its job, the
    /// first ~100 samples should hold near 6 dB (~2.0) and the last
    /// samples should be near -60 dB (~0.001) - the smoother needs
    /// time to ramp, but it should start ramping AT the event sample,
    /// not at sample 0 of the block the event lands in. The pre-event
    /// segment is therefore solidly +6 dB; the eager-apply behaviour
    /// would have shoved it toward -60 dB starting from sample 0.
    #[test]
    fn set_param_chunks_at_event_sample() {
        use moose_test::{InputSource, driver};
        use std::time::Duration;

        let sr = 48_000.0;
        let result = driver!(PluginShell)
            .sample_rate(sr)
            .duration(Duration::from_millis(200))
            .input(InputSource::Constant(1.0))
            // Pre-load +6 dB so the run starts already snapped to the
            // max of the gain range (avoids the first-block ramp from
            // the default into the test's starting target). The
            // `PluginDriver::set_param` setter goes through
            // `Params::set_normalized` (normalized [0, 1]); the
            // script-side `Script::set_param` below pushes a
            // `ParamChange` event whose value the chunker delivers
            // to `Params::set_plain` (plain units - matches the CLAP
            // / VST3 wire convention).
            .set_param(P::Gain, 1.0)
            // Schedule a step to -60 dB at sample 4800.
            .script(|s| {
                s.wait_samples(4800);
                s.set_param(P::Gain, -60.0);
            })
            .run();

        // Pre-event tail (samples 4700..4800): the smoother has had
        // an exp(5ms) tail to settle to +6 dB, so output sits near
        // 2.0. With eager apply this region would already be ramping
        // *toward* -60 dB and the mean magnitude would be noticeably
        // below 2.0.
        let pre = &result.output[0][4700..4800];
        #[allow(clippy::cast_precision_loss)]
        let pre_mean = pre.iter().map(|s| s.abs()).sum::<f32>() / pre.len() as f32;
        assert!(
            pre_mean > 1.5,
            "pre-event region should sit near +6 dB (~2.0); got mean={pre_mean}"
        );

        // Post-event tail (last 100 samples): the smoother has had
        // ~100 ms after the event to ramp toward -60 dB; output should
        // be far below the pre-event level.
        let post = &result.output[0][result.output[0].len() - 100..];
        #[allow(clippy::cast_precision_loss)]
        let post_mean = post.iter().map(|s| s.abs()).sum::<f32>() / post.len() as f32;
        assert!(
            post_mean < 0.05,
            "post-event tail should ramp toward -60 dB (~0.001); got mean={post_mean}"
        );
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
        moose_test::screenshot!(PluginShell, "screenshots/gain_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        moose_test::screenshot!(PluginShell, "screenshots/gain_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        moose_test::screenshot!(PluginShell, "screenshots/gain_default_windows.png")
            .pixel_threshold(2)
            .run();
    }

    /// End-to-end check of [`moose_test::PluginDriver`] on an
    /// effect: feed a block of non-zero input, assert the output is
    /// non-silent and not clipping or `NaNing`. The canonical smoke
    /// test for the driver pipeline.
    #[test]
    fn driver_passthrough() {
        use moose_test::{InputSource, assertions, driver};
        use std::time::Duration;

        let result = driver!(PluginShell)
            .sample_rate(44_100.0)
            .channels(2)
            .block_size(256)
            .duration(Duration::from_secs(1))
            .input(InputSource::Constant(0.5))
            .run();

        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
        assertions::assert_peak_below(&result, 1.0);
    }
}
