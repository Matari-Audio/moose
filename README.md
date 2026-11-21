# moose

A Rust audio plugin framework. Build **CLAP**, **VST3**, **AU (v2 & v3 on macOS)**, and **standalone** plugins from one Rust codebase.

## Highlights

- **One codebase, native format parity** — CLAP 1.2+ (polyphonic modulation, ambisonics, host thread pool, nested Wayland), Steinberg VST3 3.7+/3.8+ (pure-Rust COM, `IParameterFinder`, `IKeyswitchController`, `IUnitInfo`/`IProgramListData`, `IPluginCompatibility`, `IWaylandFrame`), first-class macOS Audio Unit v2 & v3 (`AUParameterTree`, sample-accurate ramps, `auval` validated), and Standalone.
- **Real-time safe & event-driven** — zero audio-thread heap allocations (enforced via `--features rt-paranoid`), lock-free host handoffs verified with `loom`, and event-driven OS wakes with zero UI-thread polling (`eventfd`/`IRunLoop` on Linux, libdispatch `DATA_OR` on macOS, `HWND_MESSAGE` on Windows).
- **Fast local loop & out-of-the-box CI** — `cargo moose new` initializes a single plugin or multi-plugin workspace in milliseconds with fast-compile dev profiles, hot-reload `--shell` builds, headless PNG screenshots, and a cross-platform GitHub Actions CI workflow ready on day one.

## Quick Start

```sh
# Install the CLI (one-time)
cargo install --locked --git https://github.com/Matari-Audio/moose cargo-moose

# Scaffold a new plugin (or multi-plugin workspace with --workspace)
cargo moose new my-plugin && cd my-plugin

# Fast local iteration
cargo moose run --debug           # standalone, fast dev profile
cargo moose install --clap        # build + install CLAP
cargo moose install --vst3        # build + install VST3
cargo moose install --au2         # build + install AU v2 (macOS)
```

Every `cargo moose` build/install command compiles in **release** mode by default so DAW testing runs at full DSP speed; pass `--debug` for rapid iteration (scaffolded projects enable line-table debug info and lightly optimized dependencies so `--debug` compiles quickly and still runs audio smoothly).

```sh
cargo moose build                 # bundle every enabled format into target/bundles/
cargo moose build --clap --vst3   # build a subset of formats
cargo moose build --shell         # hot-reload shell + logic dylib build
cargo moose screenshot --out screenshots/main.png
cargo moose validate              # clap-validator + pluginval + auval
cargo moose package               # signed .pkg (macOS), .exe (Windows), or .tar.gz (Linux)
cargo moose doctor                # verify toolchain, SDKs, and install paths
```

Scaffolded plugins default to **CLAP + VST3 + AU + standalone** (AU is skipped with a note off macOS) and include a `.github/workflows/ci.yml` covering formatting, Clippy, cross-OS tests, `rt-paranoid` zero-allocation checks, and release builds (pass `--no-ci` to opt out).

To scaffold a multi-plugin suite in one workspace:

```sh
cargo moose new my-suite --workspace gain synth arp \
  --type:synth=instrument --type:arp=midi
```

## Minimal Example

```rust
use moose::prelude::*;
use moose_gui_types::layout::{GridLayout, knob, widgets};

#[derive(Params)]
pub struct GainParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}

use GainParamsParamId as P;

pub struct Gain;

// The processor owns DSP state; only parameter targets are shared.
pub struct GainProcessor {
    gain: ParamSmoother,
    sample_rate: f64,
    // Per-sample gain for one block, allocated once in `activate`.
    gains: Vec<f32>,
}

impl Plugin for Gain {
    type Shared = GainParams;
    type MainThread = ();
    type Processor = GainProcessor;

    fn main_thread(_shared: &Arc<GainParams>) {}

    fn activate(
        shared: &GainParams,
        config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<GainProcessor, PreparationError> {
        let mut gain = ParamSmoother::default();
        gain.reset(&shared.gain, config.sample_rate);
        Ok(GainProcessor {
            gain,
            sample_rate: config.sample_rate,
            gains: vec![0.0; config.max_block_size],
        })
    }

    fn reset(processor: &mut GainProcessor, shared: &GainParams) {
        processor.gain.reset(&shared.gain, processor.sample_rate);
    }

    fn process(
        processor: &mut GainProcessor,
        params: &GainParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        // Smooth the gain once per block, then run each channel as a
        // plain slice loop the compiler can vectorize.
        let gains = &mut processor.gains[..buffer.num_samples()];
        processor.gain.next_into(&params.gain, gains);
        for g in gains.iter_mut() {
            *g = db_to_linear(*g);
        }
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            for ((o, i), g) in out.iter_mut().zip(inp).zip(gains.iter()) {
                *o = i * g;
            }
        }
        ProcessStatus::Normal
    }

    fn editor(params: Arc<GainParams>) -> Box<dyn Editor> {
        moose_gui::default_editor(
            params,
            GridLayout::build(vec![widgets(vec![knob(P::Gain, "Gain")])]),
        )
    }
}

moose::plugin! { plugin: Gain, params: GainParams }
```

> Switch the import to `moose::prelude64::*` to write `f64` DSP instead — step smoothers with `smoother.next_into_f64(&param, ..)` / `next_f64`, change `GainProcessor::gains` to `Vec<Sample>` (a `Vec<f64>` with this prelude), implement `Plugin<f64>`, and let the format wrapper widen/narrow at the block boundary.

## Formats & GUI Backends

| Format               | macOS | Windows | Linux |
|----------------------|-------|---------|-------|
| CLAP (`.clap`)       | Yes   | Yes     | Yes   |
| VST3 (`.vst3`)       | Yes   | Yes     | Yes   |
| AU v2 (`.component`) | Yes   | —       | —     |
| AU v3 (`.appex`)     | Yes   | —       | —     |
| Standalone           | Yes   | Yes     | Yes   |

**GUI backends:**
- Built-in widget set (`moose-gui`, CPU or wgpu rendering)
- egui (`moose-egui`)
- [MUI](https://github.com/Matari-Audio/MUI) (`moose-mui`, enabled via `moose`'s `mui` feature)
- Any custom renderer via a raw window handle (`Editor` trait, supporting Cocoa, Win32, X11, and Wayland)

## Presets

Place `.preset` TOML files in a `presets/` directory next to your crate and `cargo moose install` / `package` ships them automatically as CLAP preset-discovery entries, `.vstpreset` files, and AU factory presets / `.aupreset` files. Use `cargo moose preset list | init | pull | convert | import | export` to author and round-trip presets with your DAW.

## Workspace Crates

- **Core & DSP:** `moose`, `moose-core`, `moose-params`, `moose-derive`, `moose-dsp`, `moose-simd`, `moose-utils`
- **Formats & Runtime:** `moose-clap`, `moose-vst3`, `moose-au`, `moose-standalone`, `moose-plugin`, `moose-loader`, `moose-shim-types`
- **GUI:** `moose-gui`, `moose-gui-types`, `moose-gui-utils`, `moose-cpu`, `moose-gpu`, `moose-font`, `moose-egui`, `moose-mui`
- **Tooling & Testing:** `cargo-moose`, `moose-build`, `moose-driver`, `moose-test`

## Requirements

- **Rust:** 1.92+
- **macOS:** Xcode command-line tools (full Xcode for AU v3 `.appex` builds)
- **Windows:** MSVC build tools (`x86_64-pc-windows-msvc`); run `cargo moose install --system` from an Administrator prompt for system-wide directories
- **Linux:** Wayland / X11 and Vulkan development headers, ALSA / JACK (or PipeWire's JACK shim)

## Acknowledgements

moose is maintained by Matari Audio and is forked from [**truce**](https://github.com/truce-audio/truce) v6.3.0; see [`NOTICE`](NOTICE). truce drew on [**nih-plug**](https://github.com/robbert-vdh/nih-plug) by Robbert van der Helm.

## License

moose, like truce, is licensed under **The Truce License, Version 1.0** ([`LICENSE`](LICENSE), SPDX `LicenseRef-TruceLicense-1.0`), a dual [Apache-2.0](LICENSE-APACHE) / [MIT](LICENSE-MIT) permissive grant with one narrow rider.

**For plug-in authors it is effectively just MIT / Apache-2.0.** Build, ship, and sell plug-ins, plug-in suites, and internal SDKs under either license — no fees, no splash screen, no revenue cap, no email needed. Most users never need to read past this paragraph.

Contributions are inbound = outbound under the Truce License unless you explicitly state otherwise.

### The one rider — commercial frameworks and services

You need a Framework License, granted by permission, only to redistribute moose **as a commercial framework** to other developers, or to run it **as a commercial service** that provides its framework capabilities to other developers — anything sold, subscription-gated, dual-licensed commercially, or bundled into a paid offering. Free, OSI-licensed framework projects on top of moose are exempt. Plug-in authors and internal-SDK use are unaffected.

See [`LICENSE`](LICENSE) Section 2 for the precise boundary, the exemption criteria, and the request procedure.
