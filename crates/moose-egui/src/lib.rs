//! egui-based GUI backend for moose audio plugins.
//!
//! Provides `EguiEditor`, an implementation of `moose_core::Editor` that
//! renders using egui's immediate-mode UI via egui-wgpu. Gives plugin
//! developers access to egui's full widget library, layout system, and
//! ecosystem while retaining moose's parameter binding and host integration.
//!
//! # Quick Start
//!
//! ```ignore
//! use moose_egui::EguiEditor;
//! use moose_egui::widgets::{param_knob, param_slider};
//! use moose_core::editor::PluginContext;
//!
//! let editor = EguiEditor::new(params, (800, 600), |ui: &mut egui::Ui, state: &PluginContext<MyParams>| {
//!     ui.heading("My Plugin");
//!     param_slider(ui, state, 0u32);
//! });
//! ```

// `editor.rs` is the baseview-driven desktop path; `editor_ios.rs`
// drives the UIView + CADisplayLink + CAMetalLayer host on iOS.
// `renderer.rs` (egui-wgpu wrapper) is shared - it has both a
// baseview-window and a raw-CAMetalLayer constructor.
pub mod dialog;
#[cfg(not(target_os = "ios"))]
pub mod editor;
pub mod font;
pub mod input;
mod lifecycle;
pub mod platform;
#[cfg(target_os = "windows")]
mod render_thread;
pub mod renderer;
#[cfg(not(target_os = "ios"))]
mod screenshot;
pub mod theme;
pub mod widgets;

#[cfg(target_os = "ios")]
mod editor_ios;

#[cfg(not(target_os = "ios"))]
pub use editor::{EditorUi, EguiEditor};

#[cfg(target_os = "ios")]
pub use editor_ios::{EditorUi, EguiEditor};
