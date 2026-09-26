# moose-gui-types

Lightweight GUI types for the moose audio plugin framework.

## Overview

`moose-gui-types` carries the trait + data surface that GUI
backends build on - `GridLayout`, `RenderBackend` (the trait -
not its impl), `WidgetType`, `WidgetRegion`, `InteractionState`,
`Theme` / `Color`, `ParamSnapshot`, the `grid!` and `layout!`
macros. Crates that only need to *describe* layouts and react
to platform-translated input events depend on this crate; the
heavy machinery (tiny-skia rasterization, baseview windowing,
moose-font / skrifa) stays in the renderer crates (`moose-cpu`,
`moose-gpu`) and `moose-gui`.

## Crate split

```
moose-core          <- AudioBuffer, Editor trait, EventList, ...
   |
moose-gui-types     <- this crate - light data + traits
   |
moose-plugin        <- PluginLogic / PluginLogic64 / PluginLogicCore
   |       \
moose-gui   moose-egui  <- alt GUI backend
(BuiltinEditor,   depends on moose-gui-types but not moose-gui
 baseview)
   |       \
moose-cpu  moose-gpu  <- RenderBackend impls, pulled by moose-gui's
(tiny-skia, (wgpu)       cpu (default) / gpu features
 skrifa)
```

An egui-only plugin's dep tree contains `moose-plugin ->
moose-gui-types -> moose-core` - `tiny-skia`, `baseview`,
`skrifa`, `moose-font` don't appear unless the plugin also
depends on `moose-gui`.

## Key types

- **`GridLayout` / `PluginLayout`** - declarative widget layouts
- **`RenderBackend`** - trait the renderer backends implement (the
  `CpuBackend` in `moose-cpu` is one impl; `moose_gpu::WgpuBackend`
  is another)
- **`WidgetType` / `WidgetKind` / `WidgetRegion`** - widget
  enumeration + hit-test regions
- **`InteractionState` / `InputEvent`** - input dispatch primitives
  (the `BaseviewTranslator` that maps baseview events into these
  lives in `moose-gui`)
- **`Theme` / `Color`** - visual theme (tiny-skia conversions live
  on `ColorExt` in `moose-cpu`)
- **`ParamSnapshot`** - per-frame view of params for widget code

## Macros

- **`layout!`** - declarative DSL for `PluginLayout`
- **`grid!`** - declarative DSL for `GridLayout`

Part of [moose](https://github.com/Matari-Audio/moose). [Docs](https://truce.audio/docs/).
