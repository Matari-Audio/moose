# moose-shim-types

Shared C header for the moose AU shim.

## Overview

A tiny "data carrier" crate that publishes the
`include/au_shim_types.h` header (defining `AuTransportSnapshot` and
the other structs that bridge the AU shim's C/Objective-C/Swift code
to moose-au's Rust FFI) as both an embedded `&'static str` constant
and an on-disk path that `cc-rs` can use as an include directory.

```rust
// the bytes, for embedding into a generated build tree
pub const AU_SHIM_TYPES_H: &str;

// the directory, for `cc::Build::include()` / clang `-I<dir>`
pub fn include_dir() -> std::path::PathBuf;
```

## Why a separate crate

Three consumers need the **exact same bytes** of the header:

1. **`moose-au/build.rs`** - passes `include_dir()` to `cc-rs` so the
   shim sources (`au_shim_common.c`, `au_v2_shim.c`) can
   `#include "au_shim_types.h"` during compile.
2. **`moose-au/src/ffi.rs`** - defines `AuTransportSnapshot` whose
   Rust layout has to match the C struct in the header.
3. **`cargo-moose/src/templates.rs`** - embeds `AU_SHIM_TYPES_H` into
   the AU v3 Xcode template that `cargo moose install --au3` writes
   into the user's build tree, where the Swift `BridgingHeader.h`
   then `#import`s it.

Merging into either consumer breaks the other:

- **Into `moose-au`**: `cargo-moose` would have to depend on
  `moose-au`, dragging the AU NSView Objective-C compile cone into
  `cargo install cargo-moose`. Wrong shape - cargo-moose is meant to
  be lean.
- **Into `cargo-moose/templates/`**: works for the embedding case
  but `moose-au/build.rs` loses the published path it can pin to
  once moose-au gets published. Workspace-relative paths break
  post-publish, which is exactly when stable cross-crate sharing
  matters.

So this crate exists to be the single, published, version-pinnable
source of the header bytes. Same shape of rationale as `moose-font`
(a data-only crate with multiple consumers needing identical bytes).

Part of [moose](https://github.com/moose-audio/moose). [Docs](https://moose.audio/docs/).
