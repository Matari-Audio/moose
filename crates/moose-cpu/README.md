# moose-cpu

CPU rendering backend for the moose built-in GUI.

## Overview

Provides `CpuBackend`, a software implementation of
`moose_gui_types::RenderBackend` built on tiny-skia, plus the skrifa
glyph cache (`font`) and the `ColorExt` conversions the rasterizer
needs. This is the default renderer: it works on every platform without
a usable GPU, and its output is deterministic, which makes it the
reliable path for screenshot tests.

`moose-cpu` is an implementation detail of
[`moose-gui`](../moose-gui): `BuiltinEditor` rasterizes the widget tree
into a tiny-skia pixmap through this backend and blits it to a wgpu
surface for compositing. Plugins don't depend on `moose-cpu` directly -
it's pulled in by `moose-gui`'s default `cpu` feature. Its peer
[`moose-gpu`](../moose-gpu) provides the wgpu backend behind the `gpu`
feature.

## Key types

- **`CpuBackend`** -- tiny-skia `moose_gui_types::RenderBackend` implementation
- **`ColorExt`** -- extension trait adding `to_skia()` / `to_premultiplied()` to the light `moose_gui_types::theme::Color` type, so that type stays tiny-skia-free
- **`font`** -- glyph cache + text measurement (skrifa outlines rasterized with tiny-skia), fed by the bundled JetBrains Mono from [`moose-font`](../moose-font)

## Usage

`moose-cpu` is selected automatically - the default `cpu` feature on
`moose-gui` enables it, and `editor()` renders through it:

```rust
fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
    GridLayout::build(vec![widgets(vec![
        knob(P::Gain, "Gain"),
    ])])
    .into_editor(&params)
}
```

Part of [moose](https://github.com/Matari-Audio/moose). [Docs](https://truce.audio/docs/).
