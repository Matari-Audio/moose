//! `IPlugView` + `IPlugViewContentScaleSupport`.
//!
//! The view keeps its instance alive (an `Arc<Host>`), like the SDK's
//! `EditorView`, so editor callbacks stay valid after the host drops its
//! component interfaces.

use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use vst3::Steinberg::IPlugViewContentScaleSupport_::ScaleFactor;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};

use super::component::Host;
use super::retain;

pub(super) struct View {
    host: Arc<Host>,
    /// This view's own interface pointer, for identity and `resizeView`.
    id: AtomicUsize,
    frame: Mutex<Option<ComPtr<IPlugFrame>>>,
}

impl Class for View {
    type Interfaces = (IPlugView, IPlugViewContentScaleSupport);
}

/// The platform type this build embeds into.
fn platform_type() -> Option<&'static CStr> {
    let t = if cfg!(target_os = "macos") {
        kPlatformTypeNSView
    } else if cfg!(target_os = "windows") {
        kPlatformTypeHWND
    } else if cfg!(target_os = "linux") {
        kPlatformTypeX11EmbedWindowID
    } else {
        return None;
    };
    Some(unsafe { CStr::from_ptr(t) })
}

fn supported(kind: FIDString) -> bool {
    !kind.is_null() && platform_type().is_some_and(|t| unsafe { CStr::from_ptr(kind) } == t)
}

impl View {
    /// A new view for `host`, or null when the plugin has no editor.
    pub(super) fn create(host: &Arc<Host>) -> *mut IPlugView {
        if unsafe { (host.reg.cb.gui_has_editor)(host.ctx) } == 0 {
            return std::ptr::null_mut();
        }
        let wrapper = ComWrapper::new(View {
            host: host.clone(),
            id: AtomicUsize::new(0),
            frame: Mutex::new(None),
        });
        let Some(ptr) = wrapper.to_com_ptr::<IPlugView>() else {
            return std::ptr::null_mut();
        };
        wrapper.id.store(ptr.as_ptr() as usize, Ordering::Relaxed);
        // Hosts use one editor at a time; resize requests go to the newest.
        host.set_current_view(ptr.as_ptr() as usize, ptr.as_ptr());
        #[cfg(target_os = "linux")]
        host.update_run_loop_registration(None);
        ptr.into_raw()
    }

    fn id(&self) -> usize {
        self.id.load(Ordering::Relaxed)
    }

    fn set_frame(&self, frame: Option<ComPtr<IPlugFrame>>) {
        let mut slot = self.frame.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.as_ref().map(ComPtr::as_ptr) == frame.as_ref().map(ComPtr::as_ptr) {
            return;
        }
        slot.clone_from(&frame);
        drop(slot);
        if self.host.is_current_view(self.id()) {
            // The frame first: edits drained while rebinding resize through it.
            #[cfg(target_os = "linux")]
            let unknown = frame.as_ref().map(|f| f.as_ptr().cast::<FUnknown>());
            self.host.set_current_frame(self.id(), frame);
            // Only the current view binds the instance's UI-thread wake.
            #[cfg(target_os = "linux")]
            self.host.update_run_loop_registration(unknown);
        }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        unsafe { self.removed() };
        self.set_frame(None);
        self.host.forget_view(self.id());
    }
}

impl IPlugViewTrait for View {
    unsafe fn isPlatformTypeSupported(&self, kind: FIDString) -> tresult {
        if supported(kind) {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn attached(&self, parent: *mut c_void, kind: FIDString) -> tresult {
        // The type says how to read `parent`; check it before Rust does.
        if parent.is_null() || !supported(kind) {
            return kResultFalse;
        }
        self.host.attach_view(self.id());
        if unsafe { (self.host.reg.cb.gui_open)(self.host.ctx, parent) } == 0 {
            // Not open: detach so a later attached on this view retries.
            self.host.detach_view(self.id());
            return kResultFalse;
        }
        kResultOk
    }

    unsafe fn removed(&self) -> tresult {
        if self.host.detach_view(self.id()) {
            unsafe { (self.host.reg.cb.gui_close)(self.host.ctx) };
        }
        kResultOk
    }

    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyDown(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyUp(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }

    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        let Some(r) = (unsafe { size.as_mut() }) else {
            return kResultFalse;
        };
        let (mut w, mut h) = (0u32, 0u32);
        unsafe { (self.host.reg.cb.gui_get_size)(self.host.ctx, &raw mut w, &raw mut h) };
        *r = ViewRect {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        kResultOk
    }

    /// The host has committed a size; there is no refusing it here.
    unsafe fn onSize(&self, size: *mut ViewRect) -> tresult {
        if let Some(r) = unsafe { size.as_ref() } {
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            if w > 0 && h > 0 {
                unsafe { (self.host.reg.cb.gui_set_size)(self.host.ctx, w as u32, h as u32) };
            }
        }
        kResultOk
    }

    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
        self.set_frame(unsafe { retain(frame) });
        kResultOk
    }

    unsafe fn canResize(&self) -> tresult {
        if unsafe { (self.host.reg.cb.gui_can_resize)(self.host.ctx) } != 0 {
            kResultOk
        } else {
            kResultFalse
        }
    }

    /// Clamps to the editor's constraints and always succeeds: kResultFalse
    /// means "unrecoverable", and Live asks even when canResize is false
    /// (a fixed editor then snaps to its size).
    unsafe fn checkSizeConstraint(&self, rect: *mut ViewRect) -> tresult {
        if let Some(r) = unsafe { rect.as_mut() } {
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            if w > 0 && h > 0 {
                let (mut w, mut h) = (w as u32, h as u32);
                unsafe {
                    (self.host.reg.cb.gui_check_size_constraint)(
                        self.host.ctx,
                        &raw mut w,
                        &raw mut h,
                    );
                };
                r.right = r.left + w as i32;
                r.bottom = r.top + h as i32;
            }
        }
        kResultOk
    }
}

impl IPlugViewContentScaleSupportTrait for View {
    /// macOS scales through `AppKit`; the host factor is not used there.
    unsafe fn setContentScaleFactor(&self, factor: ScaleFactor) -> tresult {
        if cfg!(target_os = "macos") {
            return kResultFalse;
        }
        unsafe { (self.host.reg.cb.gui_set_content_scale)(self.host.ctx, f64::from(factor)) };
        kResultOk
    }
}
