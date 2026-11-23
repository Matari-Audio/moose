//! Platform parent-window and host-scale helpers for `MuiEditor`.
//!
//! Ported from truce-gui 6.3.0 `src/platform.rs`
//! (<https://github.com/truce-audio/truce>), licensed
//! `LicenseRef-TruceLicense-1.0`.
use moose_core::editor::RawWindowHandle;
use raw_window_handle as rwh;

/// Moose's parent handle as raw-window-handle 0.6: what
/// [`mui_baseview::open`] takes.
pub struct ParentWindow(pub RawWindowHandle);

impl rwh::HasWindowHandle for ParentWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let null = || rwh::HandleError::Unavailable;
        let raw = match self.0 {
            RawWindowHandle::AppKit(ptr) => rwh::RawWindowHandle::AppKit(
                rwh::AppKitWindowHandle::new(std::ptr::NonNull::new(ptr).ok_or_else(null)?),
            ),
            RawWindowHandle::UiKit(ptr) => rwh::RawWindowHandle::UiKit(
                rwh::UiKitWindowHandle::new(std::ptr::NonNull::new(ptr).ok_or_else(null)?),
            ),
            RawWindowHandle::Win32(ptr) => {
                rwh::RawWindowHandle::Win32(rwh::Win32WindowHandle::new(
                    std::num::NonZeroIsize::new(ptr as isize).ok_or_else(null)?,
                ))
            }
            RawWindowHandle::X11(id) => {
                let id = u32::try_from(id)
                    .ok()
                    .and_then(std::num::NonZeroU32::new)
                    .ok_or_else(null)?;
                rwh::RawWindowHandle::Xcb(rwh::XcbWindowHandle::new(id))
            }
            // A host's Wayland surface is embedded by the editor, not parented.
            _ => return Err(rwh::HandleError::NotSupported),
        };
        // SAFETY: the host keeps the parent alive while the editor is open;
        // this only re-types its handle.
        #[expect(unsafe_code, reason = "borrow_raw vouches for the host's handle")]
        let handle = unsafe { rwh::WindowHandle::borrow_raw(raw) };
        Ok(handle)
    }
}

impl ParentWindow {
    /// The host's Win32 parent window `hwnd`, as [`MuiEditor::on_win32_parent`]
    /// handed it out: a native dialog owned by it is modal to the host.
    ///
    /// [`MuiEditor::on_win32_parent`]: crate::MuiEditor::on_win32_parent
    #[must_use]
    pub fn win32(hwnd: isize) -> Self {
        Self(RawWindowHandle::Win32(
            std::ptr::with_exposed_provenance_mut(hwnd.cast_unsigned()),
        ))
    }
}

/// A Win32 parent's display is the desktop; other platforms give none
/// (what a native dialog's `set_parent` asks for besides the window).
impl rwh::HasDisplayHandle for ParentWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        match self.0 {
            RawWindowHandle::Win32(_) => Ok(rwh::DisplayHandle::windows()),
            _ => Err(rwh::HandleError::NotSupported),
        }
    }
}

/// Per-instance host scale for an embedded editor window.
///
/// Feed it `Editor::set_scale_factor`. Kept strictly per-instance (never in a
/// process-global static) so multiple plugin instances on monitors with
/// different DPI scales (e.g. 100% + 200%) never clobber one another; format
/// wrappers replay each instance's stored host scale across editor reopen.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostScale {
    host: Option<f64>,
}

impl HostScale {
    /// The host's content scale; ignored unless finite and positive.
    pub fn set(&mut self, factor: f64) {
        if factor.is_finite() && factor > 0.0 {
            self.host = Some(factor);
        }
    }
    /// This editor instance's host scale, or `None` if unset.
    #[must_use]
    pub fn get(&self) -> Option<f64> {
        self.host
    }
    /// The scale to pin the window to: the host's when it gave one, else
    /// `None` for the OS scale, never the two multiplied. `None` on macOS,
    /// whose `AppKit` coordinates are logical and backing scale is the OS's.
    #[must_use]
    pub fn policy(&self) -> Option<f64> {
        if cfg!(target_os = "macos") {
            None
        } else {
            self.get()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HostScale;

    #[test]
    fn instances_hold_independent_host_scales() {
        let mut monitor_a = HostScale::default();
        let mut monitor_b = HostScale::default();
        let untouched = HostScale::default();

        monitor_a.set(1.0);
        monitor_b.set(2.0);

        assert_eq!(monitor_a.get(), Some(1.0));
        assert_eq!(monitor_b.get(), Some(2.0));
        assert_eq!(untouched.get(), None);

        // Non-finite or non-positive values are ignored without clobbering.
        monitor_a.set(0.0);
        monitor_a.set(-1.5);
        monitor_a.set(f64::NAN);
        monitor_a.set(f64::INFINITY);
        assert_eq!(monitor_a.get(), Some(1.0));
    }
}
