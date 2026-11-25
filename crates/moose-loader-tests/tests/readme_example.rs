//! Compiles and runs the live `README.md` minimal plugin example verbatim
//! (extracted by `build.rs` from `frameworks/moose/README.md`) so the front
//! page example can never drift out of sync with the `moose::Plugin` API.

#[allow(dead_code, clippy::all, clippy::pedantic)]
mod readme_minimal_example {
    include!(concat!(env!("OUT_DIR"), "/readme_example.rs"));
}

#[test]
fn readme_minimal_example_compiles_and_processes_audio() {
    use moose::Lifecycle;
    use moose::core::AudioConfig;
    use moose::core::buffer::AudioBuffer;
    use moose::core::events::{EventList, TransportInfo};
    use moose::core::plugin::{PluginRuntime, PreparationConfig, PreparationFrameBound};
    use moose::core::process::ProcessContext;
    use moose_loader::static_shell::StaticShell;
    use std::sync::Arc;

    type Logic = Lifecycle<readme_minimal_example::Gain>;
    let params = Arc::new(readme_minimal_example::GainParams::new());
    let mut shell =
        StaticShell::<readme_minimal_example::GainParams, Logic>::from_parts(params, None, None);
    shell
        .prepare(&PreparationConfig::new(
            AudioConfig::new(48_000.0, 64),
            PreparationFrameBound::NegotiatedHost,
        ))
        .expect("README example should activate");

    let input = [0.5_f32; 64];
    let mut output = [0.0_f32; 64];
    let inputs: [&[f32]; 1] = [&input];
    let mut outputs: [&mut [f32]; 1] = [&mut output];
    let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 64) };
    let events = EventList::default();
    let transport = TransportInfo::default();
    let mut out_events = EventList::default();
    let mut ctx = ProcessContext::new(&transport, 48_000.0, 64, &mut out_events);

    shell.process(&mut buffer, &events, &mut ctx);
    assert!(
        output.iter().all(|s| s.is_finite()),
        "README example must produce finite audio"
    );
}
