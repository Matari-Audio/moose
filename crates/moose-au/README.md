# moose-au

Audio Unit v2 + v3 format wrapper for the moose audio plugin framework.

## Overview

Bridges a moose `PluginExport` implementation to Apple's Audio Unit API.
The same Rust dylib serves both AU v2 (`.component`, in-process) and AU
v3 (`.appex` inside a container `.app`, sandboxed) - only the bundle
shape and the surrounding shim differ. The framework dylib is the
identical artifact in either case; cargo-moose's install / package
commands wrap it in the right bundle layout.

The Rust side compiles everywhere, but the shim and the exports only
exist on macOS. User plugins opt into AU by adding
`moose-au = { workspace = true, optional = true }` and gating it behind
an `au` Cargo feature (`au = ["dep:moose-au"]`); `moose::plugin!`
exports the AU entry points when that feature is on.

## What it handles

- `AudioComponent` (v2) and `AUAudioUnit` (v3) registration
- Audio render block bridging + sample-rate / block-size lifecycle
- Parameter tree construction from moose parameter metadata
- Plugin state serialization via `moose_core::state`
- GUI view hosting via `NSViewController` (v2) / `AUViewController` (v3)
- Effects (`aufx`), instruments (`aumu`), and MIDI processors (`aumi`)

Optional sidechains keep a fixed AU input element. AU v2 treats a render
callback/connection as active; AU v3 uses `AUAudioUnitBus.isEnabled`. Until the
host connects/enables that element, the plugin receives silence for its
declared channels and no sidechain pull is performed.

`ProcessContext::bus_routing` reports the main/sidechain flattened ranges and
the v2 connection/callback or v3 `isEnabled` snapshot for the current block.
The supported main and sidechain indices are retained even when their current
range is zero. AU supports one independently routable sidechain element;
registration rejects layouts with multiple auxiliary input buses instead of
merging them.

## Architecture

- **v2** uses a hand-written C shim (`shim/au_v2_shim.c`) that exposes an
  `AudioComponentFactory` to the host and forwards every callback into
  Rust via a C ABI function-pointer table.
- **v3** uses a Swift `AUAudioUnit` subclass generated at install time by
  `cargo moose` (so it can stamp in plugin-specific identifiers), with the
  same Rust-side callback table.

AU type codes (`aufx` / `aumu` / `aumi`) are derived from the plugin's
`category` in `moose.toml` by `moose::plugin_info!()` at compile time.

Part of [moose](https://github.com/moose-audio/moose). [Docs](https://moose.audio/docs/).
