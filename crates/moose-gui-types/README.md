# moose-gui-types

Lightweight GUI types for the moose audio plugin framework.

## Overview

`moose-gui-types` carries lightweight layout, theme, widget-region, and
interaction data types (`GridLayout`, `RenderBackend`, `WidgetType`,
`WidgetRegion`, `InteractionState`, `Theme` / `Color`, `ParamSnapshot`,
and the `grid!` / `layout!` macros) without pulling in any windowing or
rendering backend.

For native plugin GUIs, use `moose-mui` (`moose` with the `mui` feature).

## Key types

- **`GridLayout` / `PluginLayout`** - declarative widget layouts
- **`RenderBackend`** - renderer-agnostic drawing trait
- **`WidgetType` / `WidgetKind` / `WidgetRegion`** - widget enumeration + hit-test regions
- **`InteractionState` / `InputEvent`** - input dispatch primitives
- **`Theme` / `Color`** - visual theme data
- **`ParamSnapshot`** - per-frame view of params for widget code

Part of [moose](https://github.com/Matari-Audio/moose).
