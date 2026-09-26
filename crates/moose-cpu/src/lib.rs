// `draw_text` and the tiny-skia path take several
// independent geometry / color arguments; bundling them into a
// struct would obscure the call sites without simplifying any.
#![allow(clippy::too_many_arguments)]

//! CPU rendering backend for moose plugins (tiny-skia + skrifa).
//!
//! Parallels [`moose-gpu`](../moose_gpu/) at the same level of the
//! crate graph: both implement [`moose_gui_types::RenderBackend`] on
//! their respective primitives ([`CpuBackend`] uses tiny-skia
//! software rasterization; `moose_gpu::WgpuBackend` uses wgpu). The
//! built-in editor (`moose_gui::BuiltinEditor`) holds an internal
//! `CpuBackend` for its iOS / pre-blit rendering path; `GpuEditor`
//! routes through `WgpuBackend` for non-iOS.
//!
//! Plugin authors don't usually depend on this crate directly -
//! `moose-gui::default_editor` pulls it in. Custom-editor plugins
//! (egui / iced / slint) that want a software `RenderBackend` for
//! testing without a GPU can opt in here.

mod backend;
pub mod font;

pub use backend::CpuBackend;

/// Extension trait giving [`moose_gui_types::theme::Color`] the
/// `to_skia` / `to_premultiplied` methods. Lives here (next to the
/// tiny-skia rasterizer that consumes them) so `moose-gui-types`
/// stays rasterizer-free.
pub trait ColorExt {
    fn to_skia(&self) -> tiny_skia::Color;
    fn to_premultiplied(&self) -> tiny_skia::PremultipliedColorU8;
}

impl ColorExt for moose_gui_types::theme::Color {
    fn to_skia(&self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba(self.r, self.g, self.b, self.a)
            .unwrap_or(tiny_skia::Color::BLACK)
    }

    fn to_premultiplied(&self) -> tiny_skia::PremultipliedColorU8 {
        self.to_skia().premultiply().to_color_u8()
    }
}
