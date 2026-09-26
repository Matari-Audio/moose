//! GPU rendering primitives for moose plugins.
//!
//! Provides [`WgpuBackend`], the wgpu+lyon+skrifa implementation of
//! [`moose_gui_types::RenderBackend`]. Used by the user-facing
//! editor wrappers (`moose_gui::GpuEditor`, `moose_egui::EguiEditor`,
//! `moose_iced::IcedEditor`, `moose_slint::SlintEditor`) and as the
//! GPU pipeline backing `moose_gui::default_editor`.
//!
//! Plugin authors don't depend on this crate directly - they call
//! `moose_gui::default_editor(...)` from their `PluginLogic::editor`
//! impl, which pulls `WgpuBackend` transitively.

mod backend;
pub mod platform;
#[cfg(not(target_os = "ios"))]
pub mod pump;

pub use backend::WgpuBackend;
