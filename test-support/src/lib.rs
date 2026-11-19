//! Test plugins shared by the CLAP, VST3 and AU wrappers' unit tests, which
//! drive them through each format's real FFI callbacks. A crate rather than
//! `#[path]` modules, so every tool (rustc, rust-analyzer, blaze) resolves it.
pub mod host_value_fixture;
pub mod large_state_fixture;

// The derive names `::moose::{core, params}`; resolve those here.
extern crate self as moose;
use moose_core as core;
use moose_params as params;
