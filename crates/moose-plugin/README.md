# moose-plugin

User-facing plugin trait for the moose audio plugin framework.

## Overview

`moose-plugin` defines [`Plugin`], the single trait plugin authors implement.
It splits a plugin into three ownership-clean types:

- `Shared`: lock-free parameters and atomics (`Arc<Self::Shared>`)
- `MainThread`: host-main-thread hooks (track info, context menus, MIDI learn)
- `Processor`: audio-thread DSP state created in `activate` and dropped in `deactivate`

The crate is intentionally light: it depends on `moose-core` and `moose-params`.
No windowing, no renderer, no fonts. Custom editors (`moose-mui` or raw window
handles) stay opt-in per plugin.

## Wrapper Bridge

`Lifecycle<P>` adapts any `P: Plugin<S>` (`S = f32` default, or `f64`) into
`PluginLogicCore<S>` and `PluginEditor<S>` so `StaticShell`, `HotShell`, and
every format wrapper (`moose-clap`, `moose-vst3`, `moose-au`, `moose-aax`) drive
it through one unified path via `moose::plugin!`. Plugins with heap-backed
custom state can optionally override `decode_state` and `apply_decoded_state`
with `DecodedState` to parse off-audio-thread and swap into `Processor` with
zero audio-thread allocations or deallocations.

Part of [moose](https://github.com/Matari-Audio/moose).
