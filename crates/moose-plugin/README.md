# moose-plugin

User-facing plugin trait for the moose audio plugin framework.

## Overview

`moose-plugin` defines [`Plugin`], the single trait plugin authors implement.
It splits a plugin into three ownership-clean types:

- `Shared`: lock-free parameters and atomics (`Arc<Self::Shared>`)
- `MainThread`: host-main-thread hooks (track info, context menus, MIDI learn)
- `Processor`: audio-thread DSP state created in `activate` and dropped in `deactivate`

The crate is intentionally light: it depends on `moose-core`, `moose-gui-types`
(data + render-trait surface only), and `moose-params`. No windowing, no
renderer, no fonts. Custom editors (`moose-mui` or raw window handles) stay
opt-in per plugin.

## Wrapper Bridge

`Lifecycle<P>` adapts any `P: Plugin<S>` (`S = f32` default, or `f64`) into
`PluginLogicCore<S>` and `PluginEditor<S>` so `StaticShell`, `HotShell`, and
every format wrapper (`moose-clap`, `moose-vst3`, `moose-au`) drive it through
one unified path via `moose::plugin!`.

Part of [moose](https://github.com/Matari-Audio/moose).
