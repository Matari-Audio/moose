# MOOSE

**Matari's Own Open Sound Engine.** A Rust audio-plugin framework for
Matari Audio, forked from [truce](https://github.com/truce-audio/truce)
at v6.3.0 (`25791270`). Build CLAP, VST3 and standalone plugins from
one Rust codebase.

## Quick Start

```sh
# Install the CLI (one-time), from this repo
cargo install --git https://github.com/Matari-Audio/moose cargo-moose

cargo moose new my-plugin && cd my-plugin
cargo moose run                  # standalone, no DAW needed
cargo moose install --clap       # build + install CLAP
cargo moose install --vst3       # build + install VST3
```

Every `cargo moose` command builds in **release** mode by default; pass
`--debug` for fast-compile iteration.

```sh
cargo moose build                # bundle every enabled format into target/bundles/
cargo moose build --clap --vst3  # a subset
cargo moose build --shell        # hot-reload shell build
cargo moose screenshot --out screenshots/main.png
cargo moose validate             # pluginval + clap-validator on installed plugins
cargo moose package              # signed .pkg (macOS) or Inno Setup .exe (Windows)
cargo moose doctor               # environment health
```

Scaffolded plugins default to **CLAP + VST3 + standalone**. On Windows,
`cargo moose install` must run from an Administrator prompt (plugin
directories are system-wide).

## Presets

Put a `presets/` directory of `.preset` TOML files next to your crate and
`cargo moose install` ships them as CLAP preset-discovery entries and
`.vstpreset` files. `cargo moose preset list | pull | convert | init` is
the authoring toolbox.

## Minimal Example

```rust
use moose::prelude::*;
use moose_gui::IntoLayoutEditor;
use moose_gui_types::layout::{knob, widgets, GridLayout};

#[derive(Params)]
pub struct GainParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}

use GainParamsParamId as P;

pub struct Gain;

impl PurePluginLogic for Gain {
    type Params = GainParams;

    fn process(params: &GainParams, buffer: &mut AudioBuffer,
               _events: &EventList, _ctx: &mut ProcessContext) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let gain = db_to_linear(params.gain.read());
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }
        }
        ProcessStatus::Normal
    }

    fn editor(params: Arc<GainParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![knob(P::Gain, "Gain")])])
            .into_editor(&params)
    }
}

moose::plugin! { logic: Gain, params: GainParams }
```

> Switch the import to `moose::prelude64::*` to write `f64` DSP
> instead — `param.read()` returns `f64`, the audio buffer is
> `f64`, and the format wrapper widens/narrows at the block
> boundary. Same `impl PluginLogic` header on both precisions.

## Formats and GUI backends

| Format     | macOS | Windows | Linux |
|------------|-------|---------|-------|
| CLAP       | Yes   | Yes     | Yes   |
| VST3       | Yes   | Yes     | Yes   |
| Standalone | Yes   | Yes     | Yes   |

GUI: the built-in widget set (`moose-gui`, CPU or GPU rendering), egui
(`moose-egui`), [MUI](https://github.com/Matari-Audio/MUI) (`moose-mui`,
the facade's `mui` feature), or a raw window handle.

Workspace crates: `moose`, `moose-core`, `moose-params`,
`moose-derive`, `moose-utils`, `moose-simd`,
`moose-build`, `moose-plugin`, `moose-clap`, `moose-vst3`,
`moose-standalone`, `moose-loader`, `moose-test`, `moose-driver`,
`moose-gui`, `moose-gui-types`, `moose-gui-utils`, `moose-gpu`,
`moose-egui`, `moose-mui`, `moose-font`, `moose-cpu` and the `cargo-moose` CLI.

## Differences from truce

MOOSE is a hard fork. It does not track truce releases.

### Removed

- **Formats:** AU v2, AU v3 (macOS and iOS), AAX (with `aax-bridge`),
  VST2 and LV2, with their wrapper crates, `cargo moose` flags
  (`--au2`, `--au3`, `--ios`, `--ios-device`, `--aax`, `--vst2`,
  `--lv2`), packaging paths, validators (auval, AAX validator) and
  scaffold templates.
- **GUI backends:** `truce-iced`, `truce-vizia`, `truce-slint` and
  `truce-gpu-examples`, plus the examples built on them (`gain-iced`,
  `gain-vizia`, `gain-slint`, `gui-zoo-iced`, `gui-zoo-slint`,
  `midi-inspector`).
- iOS CI, simulator tooling and iOS screenshot baselines.
- Leftover metadata and codecs for those formats: `PluginInfo`'s
  fourcc, AU type/manufacturer, AAX category, per-format display names
  and legacy IDs; the LV2 TTL emitter; `.aupreset` and LV2 preset
  import/export in `cargo moose preset`. Their `moose.toml` keys
  (`au_manufacturer`, `fourcc`, `au_type`, `au_subtype`, `aax_category`,
  `vst2_name`, `au_name`, `au3_name`, `aax_name`, `lv2_name`,
  `legacy_state`) are ignored if present.

### Fixes carried on top of truce 6.3.0

IDs refer to Matari's internal fork inventory.

- **Fork line (DerpcatMusic/truce, 7.0.0):** A01 explicit VST3 class
  IDs; A02 full 31-bit parameter IDs; A03 bounded lossless core event
  lane; A04 exact CLAP MIDI/note round-trip; A05 lossless VST3 native
  events; A08 optional buses and stepped-parameter contracts; A09
  configurable CLAP discovery metadata; A10 VST3 `activateBus`; A12
  egui/egui-wgpu 0.35; A13 output-event delivery status; A14 CLAP
  fractional fixed-step params; A15/A16 managed background tasks under
  hot reload and RT-safe continuations; A17 `ProcessContext::bus_routing`;
  A20 egui key capture, native file drop and Linux file dialog, with
  baseview pinned to `DerpcatMusic/baseview@15cf1fe` (X01, X11
  autorepeat normalization; since replaced by the in-tree
  `crates/moose-baseview`, upstream baseview 0.3.4 plus the ports listed
  in its README). The AU/AAX/LV2 halves of A06, A07, A11,
  A18 and A19 went away with those formats; A21/A22 (vizia) went away
  with vizia.
- **B01** CLAP requests `CLAP_PARAM_RESCAN_VALUES` after state and preset
  loads (clap-validator state reproducibility, upstream #232).
- **B02** correct VST3 IIDs for `IUnitInfo` and
  `IEditControllerHostEditing`.
- **B03** correct VST3 `IProcessContextRequirements` IID.
- **B04** CLAP replays the host GUI scale into every newly created
  editor.
- `moose-core` worker-pool test no longer depends on test ordering.
- **From KURV's vendored truce:**
  - K01 bounded host state I/O: CLAP and VST3 cap state reads at
    32 MiB and reject negative or oversized reads; VST3 `getState`
    loops on partial writes, and `setState` fails on an empty stream.
  - K02 Windows VST3 DLLs don't import VCRUNTIME140 or libstdc++-6:
    the shim matches the Rust CRT (`/MT` under `+crt-static`, which
    `cargo moose` sets) and MinGW links libstdc++ statically.
  - K04 runtime parameter names, groups and visibility
    (`#[params(presentation = "…", presentation_revision = "…")]`),
    announced by CLAP `CLAP_PARAM_RESCAN_INFO` and VST3
    `kParamTitlesChanged`.
  - K05 VST3 MIDI CC proxy parameters parse host-entered text.
  - K06 CLAP keeps parameter events flushed while the transport is
    stopped and replays them at the start of the next block.
  - K07 CLAP sets the main-port flag by bus kind; outputs after the
    first are auxiliary buses.
  - K10 configurable preset container extension
    (`[plugin.presets] extension`).
  - K11 saved `#[persist]` state is validated before anything is
    restored (`#[params(validate_persist = "…")]`), plus
    `restore_state`, `post_load`, `pre_save` and `#[persist_missing]`
    hooks.
  - K12 parameters borrow their `ParamInfo` from a static table and
    unsmoothed floats carry no smoother, shrinking every param.
  - K13 host text entry works for every parameter: without a
    `parse_fn` the derive round-trips the text through the param's
    formatter.
  - K14 hot-reload temp copies include the PID (no SIGBUS across
    processes); the static shell boxes the DSP state.
- **From asymmetry-rider's vendored truce:** S02 `default = FRAC_1_SQRT_2`
  and other `std::f64::consts` values are emitted as paths (no
  `approx_constant` lint in plugin crates), and `flags = "none"`.
  S01 (a second host-text parser) was not taken: K13 covers it and
  also round-trips enum names and custom formatters.

### Naming

| truce | moose |
|-------|-------|
| `truce`, `truce-*` crates | `moose`, `moose-*` |
| `cargo truce` | `cargo moose` |
| `truce::plugin!` | `moose::plugin!` |
| `truce.toml` | `moose.toml` (`truce.toml` still read, with a deprecation warning) |
| `TRUCE_*` env vars | `MOOSE_*` (`TRUCE_*` still read, with a deprecation warning) |
| `TRUCE_NOTARY` keychain profile | `MOOSE_NOTARY` |

Kept on purpose, so existing users' data keeps loading: the
`.trucepreset` extension (the default; see K10), `truce-preset://` URIs, the `truce/` user preset
folder, the state and preset blob magics, and the plugin ID derivation.

### Migrating a truce 6.3 plugin

1. In `Cargo.toml`, replace each `truce*` dependency with its `moose*`
   counterpart at 7.0 (git: `https://github.com/Matari-Audio/moose`).
   Drop the `au`, `aax`, `vst2` and `lv2` features.
2. Replace `truce::` / `truce_*::` paths with `moose::` / `moose_*::`,
   including `truce::plugin!`.
3. Rename `truce.toml` to `moose.toml` and remove the AU/AAX/iOS keys
   (`au_type`, `au_subtype`, `au3_subtype`, `au_tag`, `aax_category`,
   `ios_*`) and any removed format in `[packaging] formats`, which no
   longer parses.
4. Rename `TRUCE_*` variables in `.cargo/config.toml` and CI to
   `MOOSE_*`. Recreate or rename the notary keychain profile to
   `MOOSE_NOTARY`, or set `MOOSE_NOTARY_PROFILE`.
5. If your editor used egui, move your direct `egui` dependency to 0.35.
6. Rebuild with `cargo moose build --clap --vst3`. Saved sessions and
   presets load unchanged.

## Requirements

- Rust 1.92+.
- **macOS:** Xcode command-line tools.
- **Windows:** MSVC build tools, `x86_64-pc-windows-msvc` toolchain.
- **Linux:** X11 and Vulkan development headers, JACK (or PipeWire's
  shim).

## Acknowledgements

MOOSE is built on [**truce**](https://github.com/truce-audio/truce) by
the truce authors; see [`NOTICE`](NOTICE). truce drew on
[**nih-plug**](https://github.com/robbert-vdh/nih-plug) by Robbert van
der Helm.

## License

MOOSE, like truce, is licensed under **The Truce License, Version 1.0**
([`LICENSE`](LICENSE), SPDX `LicenseRef-TruceLicense-1.0`), a dual
[Apache-2.0](LICENSE-APACHE) / [MIT](LICENSE-MIT) permissive grant
with one narrow rider.

**For plug-in authors it is effectively just MIT / Apache-2.0.**
Build, ship, and sell plug-ins, plug-in suites, and internal SDKs
under either license - no fees, no splash screen, no revenue cap, no
email needed. Most users never need to read past this paragraph.

Contributions are inbound = outbound under the Truce License unless
you explicitly state otherwise.

### The one rider — commercial frameworks and services

You need a Framework License, granted by permission, only to
redistribute moose **as a commercial framework** to other developers,
or to run it **as a commercial service** that provides its framework
capabilities to other developers - anything sold, subscription-gated,
dual-licensed commercially, or bundled into a paid offering. Free,
OSI-licensed framework projects on top of moose are exempt. Plug-in
authors and internal-SDK use are unaffected.

See [`LICENSE`](LICENSE) Section 2 for the precise boundary, the
exemption criteria, and the request procedure.

