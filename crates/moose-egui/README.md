# moose-egui

egui GUI backend for moose audio plugins.

## Overview

Provides `EguiEditor`, an implementation of `moose_core::Editor` that renders
using [egui](https://github.com/emilk/egui)'s immediate-mode UI via
egui-wgpu. This gives plugin developers full access to egui's widget library,
layout system, and ecosystem while retaining moose's parameter binding and
host integration. Supports custom fonts and themes.

Use this backend when you want fine-grained control over your plugin's UI
using egui's immediate-mode paradigm.

## Key types

- **`EguiEditor`** -- the `Editor` implementation
- **`EditorUi`** -- trait you implement to define your plugin's UI
- **`PluginContext`** -- parameter bridge for reading/writing moose
  params from egui widgets (re-exported from `moose-core`)

## Usage

```rust
struct MyUi;

impl<P: Params> EditorUi<P> for MyUi {
    fn ui(&mut self, ui: &mut egui::Ui, state: &PluginContext<P>) {
        // bind widgets via the PluginContext
    }
}
```

Part of [moose](https://github.com/Matari-Audio/moose). [Docs](https://truce.audio/docs/).
