# moose-vst3

VST3 format wrapper for the moose audio plugin framework.

## Overview

Bridges a moose `PluginExport` implementation to the Steinberg VST3 plugin API
using a pure-Rust COM layer (`vst3` crate, SDK 3.8.0) with no C++ shim or
C-vtable callback indirection.

User plugins typically take a direct optional dep on this crate
(`moose-vst3 = { workspace = true, optional = true }`) gated behind a
`vst3` Cargo feature; `cargo moose build --vst3` / `install --vst3` /
`package --formats vst3` selects it at the CLI.

## What it handles

- Pure-Rust COM class factory (`GetPluginFactory`) and platform module entry points
- `IComponent` / `IAudioProcessor` -- bus negotiation, activation, and real-time audio processing
- `IEditController` / `IParameterFinder` / `IKeyswitchController` / `IUnitInfo` / `IProgramListData` -- parameter editing, automation, and GUI coordination
- `IPlugView` / `IWaylandFrame` -- platform-native editor window embedding (Cocoa, Win32, X11, Wayland)
- `IPluginCompatibility` -- migration metadata
- Streaming state persistence via `IBStream` with zero intermediate C heap copies

## Architecture

The pure-Rust COM objects (`com/`) invoke the generic plugin instance
(`Vst3Instance<P>`) directly through the typed Rust `Vst3Backend` and
`Vst3FactoryBackend` traits. Audio callbacks run wait-free with zero audio-thread
allocations or locks, and UI-thread notifications use event-driven OS wakes
(`eventfd`/`IRunLoop` on Linux, libdispatch `DATA_OR` on macOS, `HWND_MESSAGE`
on Windows).

Part of [moose](https://github.com/Matari-Audio/moose).
