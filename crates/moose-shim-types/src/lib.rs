#![forbid(unsafe_code)]

//! Shared C header types for moose format shims.
//!
//! Provides the `au_shim_types.h` header as an embedded constant so
//! both `moose-au` (build.rs, via `include_dir()` for `cc::Build`) and
//! `cargo-moose` (AU v3 template, via `AU_SHIM_TYPES_H`) read from a
//! single source of truth.
//!
//! The crate is small on purpose. It exists to break a dependency
//! cycle: `cargo-moose` is a build tool that emits AU v3 project
//! scaffolding at install-time and cannot reach into `moose-au`'s
//! source tree; `moose-au` is a runtime/build-time library that
//! cannot pull in the cargo-moose tool. Both need the *same* header
//! string, in two different forms (a filesystem path for `cc::Build`
//! vs. an embedded `&str` for templating). Splitting that into a tiny
//! standalone crate is the workspace-idiomatic way to share it.
//!
//! Kept separate from both consumers: collapsing into `moose-au`
//! reintroduces the `cargo-moose` ↔ `moose-au` dependency cycle, and
//! collapsing into `cargo-moose` would make the AU build-time C compile
//! step depend on a build tool.

/// The `au_shim_types.h` C header, embedded at compile time.
pub const AU_SHIM_TYPES_H: &str = include_str!("../include/au_shim_types.h");

/// Returns the path to the `include/` directory within this crate.
///
/// Useful for `moose-au`'s build.rs to set as a C include path.
#[must_use]
pub fn include_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include")
}
