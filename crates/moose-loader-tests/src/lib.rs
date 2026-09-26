//! Integration-test host for `moose-loader`.
//!
//! This crate has no library surface. It exists only so the tests
//! under `tests/` can depend on the umbrella `moose` crate (needed
//! by `#[derive(Params)]` / `#[derive(State)]` expansion, which
//! emits `::moose::params::*` and `::moose::core::*` paths) without
//! the back-edge into `moose-loader` becoming a `moose <->
//! moose-loader` cycle in cargo metadata.
//!
//! See `crates/moose-loader/Cargo.toml` for the rationale.
