//! Minimal C-ABI entry helper used by `export_vst3!`'s `GetPluginFactory`.

use std::ffi::c_void;

/// A new reference to the plugin factory. Called by `GetPluginFactory`.
#[must_use]
pub fn moose_vst3_get_factory() -> *mut c_void {
    crate::com::factory()
}
