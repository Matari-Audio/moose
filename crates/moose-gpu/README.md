# moose-gpu

wgpu rendering backend for the moose built-in GUI.

## Overview

Provides `WgpuBackend`, a hardware-accelerated implementation of
`moose_gui_types::RenderBackend` via wgpu (Metal on macOS, DX12 on
Windows, Vulkan on Linux), plus the wgpu surface / swapchain glue. Uses
lyon for path tessellation and skrifa for glyph atlas generation.
Widgets render identically to the CPU path but with better performance
on complex UIs.

`moose-gpu` is an implementation detail of
[`moose-gui`](../moose-gui): the `GpuEditor` there wraps the built-in
widget runtime and renders through this backend. Plugins don't depend on
`moose-gpu` directly - they enable GPU rendering through `moose-gui`'s
`gpu` feature.

## Key types

- **`WgpuBackend`** -- implements `moose_gui_types::RenderBackend` using wgpu

## Usage

Opt into GPU rendering by enabling the `gpu` feature on `moose-gui`:

```toml
[dependencies]
moose     = { version = "7.0", features = ["clap"] }
moose-gui = { version = "7.0", features = ["gpu"] }
```

`moose-gui` pulls in `moose-gpu` transitively and routes the built-in
editor through `WgpuBackend`. No direct dependency and no per-editor
code: the same `editor()` impl works for both renderers.

```rust
fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
    GridLayout::build(vec![widgets(vec![
        knob(P::Gain, "Gain"),
    ])])
    .into_editor(&params)
}
```

Part of [moose](https://github.com/Matari-Audio/moose). [Docs](https://truce.audio/docs/).
