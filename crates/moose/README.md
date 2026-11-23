# moose

Main entry point for the moose audio plugin framework.

## Overview

`moose` is the only core dependency most plugin authors need. It re-exports
`moose-core` (traits and types), `moose-params` (parameter system),
`moose-plugin` (`Plugin`, `BackgroundTask`), and the proc macros from
`moose-derive` (`Params`, `ParamEnum`, `State` at the crate root, plus
`plugin_info!()` via the preludes).

## Key re-exports

- `Plugin` and `BackgroundTask` from `moose-plugin`
- `PluginExport`, `Editor`, `NoEditor`, `EventList`, `ProcessContext`, `ProcessStatus`
  and the core runtime types from `moose-core` (`AudioBuffer` is a per-prelude
  precision-pinned type alias)
- `FloatParam`, `IntParam`, `BoolParam`, `EnumParam`, `ParamSmoother` from `moose-params`
- `#[derive(Params)]`, `#[derive(ParamEnum)]`, `#[derive(State)]` from `moose-derive`
- `moose::mui` (`MuiEditor`, `Bridge`, and the `mui` crate) when the `mui` feature is enabled
- `moose::plugin!` generates all format-export glue from one declaration

## Preludes

| Prelude | Audio buffer | DSP math in | When to pick |
|---|---|---|---|
| `prelude` / `prelude32` | `f32` | `f32` | Default — host wire is `f32` everywhere |
| `prelude64m` | `f32` | `f64` | Stable `f64` intermediate math, narrow on buffer write |
| `prelude64` | `f64` | `f64` | Wrapper widens host `f32` → plugin `f64` once per block |

## Features

| Feature | Description |
|---------|-------------|
| `clap` (default) | Enable CLAP format export |
| `vst3` | Enable VST3 format export |
| `au` | Enable Audio Unit (v2 `.component` / v3 `.appex`) export; macOS only |
| `aax` | Enable Pro Tools AAX format export |
| `mui` | Re-export `moose-mui` as `moose::mui` |
| `shell` | Build a dynamic shell that dlopens a hot-reloadable logic dylib |
| `rt-paranoid` | Enable real-time allocation checking via `moose::enable_rt_paranoid!()` |
| `hot-debug` | Verbose hot-reload diagnostics |

## Usage

```rust
use moose::mui::MuiEditor;
use moose::mui::mui::prelude::*;
use moose::prelude::*;

#[derive(Params)]
pub struct MyParams {
    #[param(name = "Gain", range = "linear(-60, 6)", unit = "dB")]
    pub gain: FloatParam,
}

use MyParamsParamId as P;

pub struct MyPlugin;

impl Plugin for MyPlugin {
    type Shared = MyParams;
    type MainThread = ();
    type Processor = ();

    fn main_thread(_shared: &Arc<MyParams>) {}

    fn activate(
        _shared: &MyParams,
        _config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<(), PreparationError> {
        Ok(())
    }

    fn process(
        _processor: &mut (),
        params: &MyParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let gain = db_to_linear(params.gain.value());
        for i in 0..buffer.num_samples() {
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }
        }
        ProcessStatus::Normal
    }

    fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
        let ui = Ui::default();
        MuiEditor::new(params, ui, (260, 180), |ui, bridge| {
            let gain = bridge.bind(ui, P::Gain, |ui, id, v| {
                knob(ui, id, "Gain", v, 0.0..=1.0).size(L)
            });
            col![gain].pad(L).center().fill(Role::Surface)
        })
        .into_editor()
    }
}

moose::plugin! { plugin: MyPlugin, params: MyParams }
```

Part of [moose](https://github.com/Matari-Audio/moose).
