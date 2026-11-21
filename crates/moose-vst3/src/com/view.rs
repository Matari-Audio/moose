//! `IPlugView` + `IPlugViewContentScaleSupport`.
//!
//! The view keeps its instance alive (an `Arc<Host>`), like the SDK's
//! `EditorView`, so editor callbacks stay valid after the host drops its
//! component interfaces.

use std::ffi::{CStr, c_void};
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use vst3::Steinberg::IPlugViewContentScaleSupport_::ScaleFactor;
use vst3::Steinberg::Vst::{IParameterFinder, IParameterFinderTrait, ParamID};
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};

use super::component::Host;
use super::retain;

pub(super) struct View {
    host: Arc<Host>,
    /// This view's own interface pointer, for identity and `resizeView`.
    id: AtomicUsize,
    frame: Mutex<Option<ComPtr<IPlugFrame>>>,
    /// Set while the editor is embedded in a Wayland surface.
    #[cfg(target_os = "linux")]
    wayland: Mutex<WaylandState>,
    /// Lock-free view of whether a Wayland attachment is active, so re-entrant
    /// `onSize` / `setContentScaleFactor` callbacks triggered during `attached`
    /// never reacquire `wayland`.
    #[cfg(target_os = "linux")]
    wayland_attached: AtomicBool,
}

#[cfg(target_os = "linux")]
enum WaylandState {
    Detached,
    Attaching,
    Attached(WaylandEmbed),
}

/// What a Wayland `attached` holds until `removed`.
#[cfg(target_os = "linux")]
struct WaylandEmbed {
    hooks: super::wayland::LoopHooks,
    wayland_frame: Option<ComPtr<IWaylandFrame>>,
    connection: super::wayland::Connection,
}

#[cfg(target_os = "linux")]
impl WaylandEmbed {
    #[allow(dead_code, reason = "used by Wayland popup parenting and unit tests")]
    fn parent_surface(&self) -> Option<*mut c_void> {
        super::wayland::parent_xdg_surface(self.wayland_frame.as_ref(), self.connection.display())
    }

    #[allow(dead_code, reason = "used by Wayland popup parenting and unit tests")]
    fn parent_toplevel(&self) -> Option<*mut c_void> {
        super::wayland::parent_xdg_toplevel(self.wayland_frame.as_ref(), self.connection.display())
    }
}

impl Class for View {
    type Interfaces = (IPlugView, IPlugViewContentScaleSupport, IParameterFinder);
}

/// The native platform type this build embeds into.
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

/// A platform type `attached` can take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    /// [`platform_type`].
    Native,
    /// `kPlatformTypeWaylandSurfaceID`: `parent` is a `wl_surface*`.
    #[cfg(target_os = "linux")]
    Wayland,
}

fn platform(kind: FIDString) -> Option<Platform> {
    if kind.is_null() {
        return None;
    }
    // SAFETY: hosts pass a NUL-terminated platform type string.
    let kind = unsafe { CStr::from_ptr(kind) };
    if platform_type().is_some_and(|t| kind == t) {
        return Some(Platform::Native);
    }
    // SAFETY: an SDK string constant.
    #[cfg(target_os = "linux")]
    if kind == unsafe { CStr::from_ptr(kPlatformTypeWaylandSurfaceID) } {
        return Some(Platform::Wayland);
    }
    None
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
            #[cfg(target_os = "linux")]
            wayland: Mutex::new(WaylandState::Detached),
            #[cfg(target_os = "linux")]
            wayland_attached: AtomicBool::new(false),
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

    /// The editor accepts a Wayland parent surface.
    #[cfg(target_os = "linux")]
    fn supports_wayland(&self) -> bool {
        // SAFETY: `ctx` is the live instance; host UI thread.
        unsafe { (self.host.reg.cb.gui_supports_wayland)(self.host.ctx) != 0 }
    }

    /// `attached(parent, "WaylandSurfaceID")`: open the editor on the host's
    /// connection, resolve the parent `wl_surface*` (preferring
    /// `IWaylandFrame::getWaylandSurface` per VST3 3.8, falling back to
    /// `parent`), then drive it from the plug frame's run loop.
    #[cfg(target_os = "linux")]
    fn attach_wayland(&self, parent: *mut c_void) -> bool {
        use super::wayland::{Connection, LoopHooks, resolve_parent_surface, wayland_frame_of};

        struct AttachGuard<'a> {
            view: &'a View,
            committed: bool,
        }
        impl Drop for AttachGuard<'_> {
            fn drop(&mut self) {
                if !self.committed {
                    self.view.wayland_attached.store(false, Ordering::Release);
                    let mut state = self
                        .view
                        .wayland
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if matches!(*state, WaylandState::Attaching) {
                        *state = WaylandState::Detached;
                    }
                }
            }
        }

        // Reserve the attachment slot without holding the mutex across any
        // external call: VST3 permits synchronous `onSize` / `resizeView`
        // callbacks while `attached` is running.
        {
            let mut state = self.wayland.lock().unwrap_or_else(PoisonError::into_inner);
            if !matches!(*state, WaylandState::Detached) {
                return false;
            }
            *state = WaylandState::Attaching;
        }
        let mut guard = AttachGuard {
            view: self,
            committed: false,
        };

        if !self.supports_wayland() {
            return false;
        }
        // Everything that can fail without side effects comes first: the
        // editor needs both the host connection and the run loop.
        let Some(host_context) = self.host.host_context() else {
            return false;
        };
        let (run_loop, wayland_frame) = {
            let guard = self.frame.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(frame) = guard.as_ref() else {
                return false;
            };
            (
                super::wake::linux::run_loop_of(frame.as_ptr().cast()),
                wayland_frame_of(frame),
            )
        };
        let Some(run_loop) = run_loop else {
            return false;
        };
        if wayland_frame.is_none() && parent.is_null() {
            return false;
        }
        let Some(connection) = Connection::open(&host_context) else {
            return false;
        };
        let Some(surface) =
            resolve_parent_surface(wayland_frame.as_ref(), connection.display(), parent)
        else {
            return false;
        };
        let (ctx, cb) = (self.host.ctx, self.host.reg.cb);
        self.host.attach_view(self.id());
        self.wayland_attached.store(true, Ordering::Release);
        // SAFETY: `ctx` is live; `display` and `surface` stay valid until
        // `gui_close`, which runs before `connection` drops.
        if unsafe { (cb.gui_open_wayland)(ctx, connection.display().cast(), surface) } == 0 {
            self.host.detach_view(self.id());
            return false;
        }
        // SAFETY: `ctx` is live and its editor open.
        let fd = unsafe { (cb.gui_event_fd)(ctx) };
        let Some(hooks) = LoopHooks::register(
            run_loop,
            Arc::downgrade(&self.host),
            (fd >= 0).then_some(fd),
        ) else {
            self.host.detach_view(self.id());
            // SAFETY: as above; the connection closes after this.
            unsafe { (cb.gui_close)(ctx) };
            return false;
        };
        let mut state = self.wayland.lock().unwrap_or_else(PoisonError::into_inner);
        if !matches!(*state, WaylandState::Attaching) {
            // `removed()` ran re-entrantly during `gui_open_wayland`.
            drop(state);
            drop(hooks);
            if self.host.detach_view(self.id()) {
                // SAFETY: `ctx` is still live until `connection` drops.
                unsafe { (cb.gui_close)(ctx) };
            }
            drop(connection);
            return false;
        }
        *state = WaylandState::Attached(WaylandEmbed {
            hooks,
            wayland_frame,
            connection,
        });
        guard.committed = true;
        true
    }

    #[cfg(target_os = "linux")]
    fn is_wayland_attached(&self) -> bool {
        self.wayland_attached.load(Ordering::Acquire)
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
        let ok = match platform(kind) {
            Some(Platform::Native) => true,
            #[cfg(target_os = "linux")]
            Some(Platform::Wayland) => self.supports_wayland(),
            None => false,
        };
        if ok { kResultOk } else { kResultFalse }
    }

    unsafe fn attached(&self, parent: *mut c_void, kind: FIDString) -> tresult {
        // The type says how to read `parent`; check it before Rust does.
        let Some(platform) = platform(kind) else {
            return kResultFalse;
        };
        match platform {
            Platform::Native => {
                if parent.is_null() {
                    return kResultFalse;
                }
            }
            #[cfg(target_os = "linux")]
            Platform::Wayland => {
                // VST3 3.8 allows `parent == nullptr` when `IWaylandFrame::getWaylandSurface`
                // provides the surface for the host's Wayland display.
                return if self.attach_wayland(parent) {
                    kResultOk
                } else {
                    kResultFalse
                };
            }
        }
        self.host.attach_view(self.id());
        if unsafe { (self.host.reg.cb.gui_open)(self.host.ctx, parent) } == 0 {
            // Not open: detach so a later attached on this view retries.
            self.host.detach_view(self.id());
            return kResultFalse;
        }
        kResultOk
    }

    /// Wayland order: run-loop hooks off, editor closed, then the host
    /// connection closed (the editor's proxies live on it).
    unsafe fn removed(&self) -> tresult {
        #[cfg(target_os = "linux")]
        let (hooks, connection) = {
            self.wayland_attached.store(false, Ordering::Release);
            let prev = std::mem::replace(
                &mut *self.wayland.lock().unwrap_or_else(PoisonError::into_inner),
                WaylandState::Detached,
            );
            match prev {
                WaylandState::Attached(e) => (Some(e.hooks), Some(e.connection)),
                WaylandState::Detached | WaylandState::Attaching => (None, None),
            }
        };
        #[cfg(target_os = "linux")]
        drop(hooks);
        if self.host.detach_view(self.id()) {
            unsafe { (self.host.reg.cb.gui_close)(self.host.ctx) };
        }
        #[cfg(target_os = "linux")]
        drop(connection);
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
    ///
    /// On Wayland (`WaylandSurfaceID`), bracket the resize with `set_sync` /
    /// `set_desync` and flush the surface so the resized buffer commits
    /// atomically with the host's parent commit (VST3 3.8 sync-on-resize).
    unsafe fn onSize(&self, size: *mut ViewRect) -> tresult {
        if let Some(r) = unsafe { size.as_ref() } {
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            if w > 0 && h > 0 {
                #[cfg(target_os = "linux")]
                let wayland = self.is_wayland_attached();
                #[cfg(not(target_os = "linux"))]
                let wayland = false;
                if wayland {
                    unsafe { (self.host.reg.cb.gui_wayland_set_sync)(self.host.ctx, 1) };
                }
                unsafe { (self.host.reg.cb.gui_set_size)(self.host.ctx, w as u32, h as u32) };
                if wayland {
                    unsafe {
                        (self.host.reg.cb.gui_on_fd)(self.host.ctx);
                        (self.host.reg.cb.gui_wayland_set_sync)(self.host.ctx, 0);
                    }
                }
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
    /// macOS scales through `AppKit`; Wayland scales through the compositor's
    /// `wp_fractional_scale_v1` / `preferred_buffer_scale`.
    unsafe fn setContentScaleFactor(&self, factor: ScaleFactor) -> tresult {
        if cfg!(target_os = "macos") {
            return kResultFalse;
        }
        #[cfg(target_os = "linux")]
        if self.is_wayland_attached() {
            return kResultFalse;
        }
        unsafe { (self.host.reg.cb.gui_set_content_scale)(self.host.ctx, f64::from(factor)) };
        kResultOk
    }
}

impl IParameterFinderTrait for View {
    unsafe fn findParameter(
        &self,
        x_pos: int32,
        y_pos: int32,
        result_tag: *mut ParamID,
    ) -> tresult {
        let Some(out) = (unsafe { result_tag.as_mut() }) else {
            return kInvalidArgument;
        };
        let mut id = 0u32;
        if unsafe { (self.host.reg.cb.gui_param_at)(self.host.ctx, x_pos, y_pos, &raw mut id) } != 0
        {
            *out = id;
            kResultOk
        } else {
            kResultFalse
        }
    }
}
