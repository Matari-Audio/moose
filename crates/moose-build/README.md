# moose-build

Build-time schema + target-dir helpers for moose.

## Overview

Plugin crates do not need a `build.rs` - `moose::plugin_info!()` reads
`moose.toml` directly at compile time and tracks it via
`include_bytes!`. This crate exists for two roles:

- **`Config` / `PluginDef` / `VendorConfig`** - the shared deserializer
  for `moose.toml`, used by both `moose-derive` (proc macros) and
  `cargo-moose` (install / build pipeline).
- **`target_dir(root)`** - resolves cargo's effective target directory
  for a workspace root, honoring `CARGO_TARGET_DIR` and
  `[build].target-dir` in `.cargo/config.toml`. Used by runtime callers
  (cargo-moose, moose-test) that need to anchor artifact paths.

Part of [moose](https://github.com/Matari-Audio/moose). [Docs](https://truce.audio/docs/).
