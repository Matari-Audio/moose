//! The two pieces of `moose_gui::platform` the editor uses, ported so the
//! plugin does not link moose-gui, whose wgpu 29 would sit in the graph next
//! to MUI's wgpu 30.
//!
//! Ported from truce-gui 6.3.0 `src/platform.rs`
//! (<https://github.com/truce-audio/truce>), licensed
//! `LicenseRef-TruceLicense-1.0`.
use std::sync::atomic::{AtomicU64, Ordering};

use baseview::WindowScalePolicy;
use moose_core::editor::RawWindowHandle;
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle as Rwh};

/// Moose's parent handle as baseview's raw-window-handle 0.5: what
/// [`mui_baseview::open`] takes.
pub struct ParentWindow(pub RawWindowHandle);

// SAFETY: the handle is the host's live parent window, which the host keeps
// alive for as long as the editor is open; this only re-types it.
#[expect(unsafe_code, reason = "HasRawWindowHandle is an unsafe trait")]
unsafe impl HasRawWindowHandle for ParentWindow {
    fn raw_window_handle(&self) -> Rwh {
        match self.0 {
            RawWindowHandle::AppKit(ptr) => {
                let mut handle = raw_window_handle::AppKitWindowHandle::empty();
                handle.ns_view = ptr;
                Rwh::AppKit(handle)
            }
            RawWindowHandle::UiKit(ptr) => {
                let mut handle = raw_window_handle::UiKitWindowHandle::empty();
                handle.ui_view = ptr;
                Rwh::UiKit(handle)
            }
            RawWindowHandle::Win32(ptr) => {
                let mut handle = raw_window_handle::Win32WindowHandle::empty();
                handle.hwnd = ptr;
                Rwh::Win32(handle)
            }
            RawWindowHandle::X11(window_id) => {
                let mut handle = raw_window_handle::XlibWindowHandle::empty();
                // rwh 0.5's field is c_ulong: u32 on Windows, where an XID
                // never reaches.
                #[cfg_attr(
                    windows,
                    expect(clippy::cast_possible_truncation, reason = "c_ulong is u32")
                )]
                {
                    handle.window = window_id as _;
                }
                Rwh::Xlib(handle)
            }
        }
    }
}

/// The last scale any host passed to [`HostScale::set`], f64 bits, 0 = never.
///
/// moose's CLAP wrapper builds a new editor on every `gui.create` and tells
/// it the host scale only when the host calls `gui.set_scale`, which hosts
/// that set it once per instance do not repeat, so a reopened editor came
/// back at 1.0 inside a 1.5x frame. (moose's VST3 wrapper replays it.)
// ponytail: one scale per process; per-instance if a host ever mixes scales.
static HOST_SCALE: AtomicU64 = AtomicU64::new(0);

/// What moose's editor tells it about scale, as the policy an embedded
/// window opens with. Feed it `Editor::set_scale_factor` and
/// `set_uses_system_scale`; it remembers the last host scale across editors.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostScale {
    host: Option<f64>,
    system: bool,
}

impl HostScale {
    /// The host's content scale; ignored unless finite and positive.
    pub fn set(&mut self, factor: f64) {
        if factor.is_finite() && factor > 0.0 {
            self.host = Some(factor);
            HOST_SCALE.store(factor.to_bits(), Ordering::Relaxed);
        }
    }
    /// The host asks the editor to follow the system scale.
    pub fn set_uses_system(&mut self, yes: bool) {
        self.system = yes;
    }
    /// This editor's host scale, else the last one any editor was given.
    pub fn get(&self) -> Option<f64> {
        self.host.or(match HOST_SCALE.load(Ordering::Relaxed) {
            0 => None,
            bits => Some(f64::from_bits(bits)),
        })
    }
    /// Linux: an embedded editor follows the host's scale, not the
    /// desktop's, which a non-DPI-aware host does not share. Elsewhere the
    /// OS reports a reliable per-window scale.
    #[must_use]
    pub fn policy(&self) -> WindowScalePolicy {
        let host = self.get();
        match editor_window_scale(self.system, host.is_some(), host.unwrap_or(1.0)) {
            Some(s) => WindowScalePolicy::ScaleFactor(s),
            None => WindowScalePolicy::SystemScaleFactor,
        }
    }
}

/// `Some(scale)` to open with `ScaleFactor(scale)`, `None` for the system
/// scale. Linux only: an embedded editor follows the host's content scale
/// (default 1), not `Xft.dpi`, which a non-DPI-aware host does not share.
fn editor_window_scale(
    uses_system_scale: bool,
    host_scale_set: bool,
    host_scale: f64,
) -> Option<f64> {
    if !cfg!(target_os = "linux") || uses_system_scale {
        None
    } else if host_scale_set && host_scale.is_finite() && host_scale > 0.0 {
        Some(host_scale)
    } else {
        Some(1.0)
    }
}
