//! The smallest plugin with a MUI editor: a gain knob and a bypass switch
//! bound to moose parameters, and an output meter.
//!
//! A plugin must unwind on panic so a UI panic stays in the editor instead
//! of killing the host: `CARGO_PROFILE_RELEASE_PANIC=unwind cargo moose
//! build --clap --vst3 -p moose-example-gain-mui`.
use moose::mui::MuiEditor;
use moose::mui::mui::prelude::*;
use moose::prelude::*;

#[derive(Params)]
pub struct GainParams {
    #[param(
        id = 0,
        name = "Gain",
        range = "linear(-60, 6)",
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,
    #[param(id = 1, name = "Bypass", default = false)]
    pub bypass: BoolParam,
    #[meter]
    pub level: MeterSlot,
}

use GainParamsParamId as P;

pub struct GainMui;

impl PurePluginLogic for GainMui {
    type Params = GainParams;

    fn process(
        params: &GainParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let bypass = params.bypass.value();
        let mut peak = 0.0f32;
        for i in 0..buffer.num_samples() {
            let gain = if bypass {
                1.0
            } else {
                db_to_linear(params.gain.read())
            };
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
                peak = peak.max(out[i].abs());
            }
        }
        context.set_meter(&params.level, peak.min(1.0));
        ProcessStatus::Normal
    }

    fn editor(params: Arc<GainParams>) -> Box<dyn Editor> {
        let mut ui = Ui::default();
        // Bundled, so this cannot fail short of a corrupt build.
        if let Ok(font) = Font::new(moose_font::JETBRAINS_MONO) {
            ui = ui.font(font);
        }
        MuiEditor::new(params, ui, (300, 200), |ui, bridge| {
            let gain = bridge.bind(ui, P::Gain, |ui, id, v| {
                knob(ui, id, "Gain", v, 0.0..=1.0).size(L)
            });
            let bypass = bridge.bind_bool(ui, P::Bypass, |ui, id, on| toggle(ui, id, "Bypass", on));
            let level = meter(ui, "level", bridge.meter(P::Level)).w(200);
            col![
                row![
                    gain,
                    col![
                        title(bridge.text(P::Gain)),
                        row![caption("Bypass"), bypass].gap(S).center(),
                    ]
                    .gap(S),
                ]
                .gap(L)
                .center(),
                level,
            ]
            .gap(M)
            .pad(L)
            .fill(Role::Surface)
        })
        .resizable((260, 180))
        .into_editor()
    }
}

moose::plugin! {
    logic: GainMui,
    params: GainParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        moose_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() {
        moose_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        moose_test::assert_state_round_trip::<Plugin>();
    }
}
