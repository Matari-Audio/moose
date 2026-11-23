use moose::mui::MuiEditor;
use moose::mui::mui::prelude::*;
use moose::prelude::*;

{params_struct | unescaped}

use {struct_name}ParamsParamId as P;

{descriptor_block | unescaped}{struct_decl | unescaped}

impl Plugin for {struct_name} \{
    type Shared = {struct_name}Params;
    type MainThread = ();
    type Processor = {processor_type | unescaped};

    fn main_thread(_shared: &Arc<{struct_name}Params>) \{}

{activate_method | unescaped}{reset_method | unescaped}{bus_layouts_method | unescaped}{process_body | unescaped}

    fn editor(params: Arc<{struct_name}Params>) -> Box<dyn Editor> \{
        let mut ui = Ui::default();
        if let Ok(font) = Font::new(moose_font::JETBRAINS_MONO) \{
            ui = ui.font(font);
        }
        MuiEditor::new(params, ui, (260, 180), |ui, bridge| \{
            let control = {layout_knob | unescaped};
            col![control].gap(M).pad(L).center().fill(Role::Surface)
        })
        .into_editor()
    }
}

{plugin_macro | unescaped}

// Installs the real-time allocation checker under `--features rt-paranoid`
// (a no-op otherwise). Wrap a driver run in `assert_no_audio_alloc` to
// fail a test if `process` ever allocates. See the audio-testing guide.
moose::enable_rt_paranoid!();
