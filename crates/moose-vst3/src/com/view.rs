//! `IPlugView` + `IPlugViewContentScaleSupport` + `IParameterFinder`.
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
    #[cfg(target_os = "linux")]
    wayland: Mutex<WaylandState>,
    #[cfg(target_os = "linux")]
    wayland_attached: AtomicBool,
}

#[cfg(target_os = "linux")]
enum WaylandState {
    Detached,
    Attaching,
    Attached(WaylandEmbed),
}

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
    // SAFETY: The `kPlatformType*` constants are static NUL-terminated strings from the SDK
    // bindings.
    Some(unsafe { CStr::from_ptr(t) })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Native,
    #[cfg(target_os = "linux")]
    Wayland,
}

fn platform(kind: FIDString) -> Option<Platform> {
    if kind.is_null() {
        return None;
    }
    // SAFETY: `kind` is non-null (checked above) and, per the VST3 contract, a NUL-terminated
    // `FIDString` live for this call. The borrow does not outlive it.
    let kind = unsafe { CStr::from_ptr(kind) };
    if platform_type().is_some_and(|t| kind == t) {
        return Some(Platform::Native);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: `kPlatformTypeWaylandSurfaceID` is a static NUL-terminated string from the SDK
    // bindings.
    if kind == unsafe { CStr::from_ptr(kPlatformTypeWaylandSurfaceID) } {
        return Some(Platform::Wayland);
    }
    None
}

impl View {
    /// A new view for `host`, or null when the plugin has no editor.
    pub(super) fn create(host: &Arc<Host>) -> *mut IPlugView {
        if !host.backend().gui_has_editor() {
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
        host.set_current_view(ptr.as_ptr() as usize, ptr.as_ptr());
        // SAFETY: `None` carries no pointer.
        #[cfg(target_os = "linux")]
        unsafe {
            host.update_run_loop_registration(None);
        }
        ptr.into_raw()
    }

    fn id(&self) -> usize {
        self.id.load(Ordering::Relaxed)
    }

    #[cfg(target_os = "linux")]
    fn supports_wayland(&self) -> bool {
        self.host.backend().gui_supports_wayland()
    }

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
        let Some(host_context) = self.host.host_context() else {
            return false;
        };
        let (run_loop, wayland_frame) = {
            let guard = self.frame.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(frame) = guard.as_ref() else {
                return false;
            };
            (
                // SAFETY: `frame` is a retained `IPlugFrame` held by the
                // locked slot, so it is live for this call.
                unsafe { super::wake::linux::run_loop_of(frame.as_ptr().cast()) },
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
        let backend = self.host.backend();
        self.host.attach_view(self.id());
        self.wayland_attached.store(true, Ordering::Release);
        // SAFETY: `connection` keeps the display open and `surface` is the host's parent surface.
        // Both are stored in `WaylandState::Attached` and torn down in `removed` only after
        // `gui_close`, which `gui_open_wayland` requires.
        if !unsafe { backend.gui_open_wayland(connection.display().cast(), surface) } {
            self.host.detach_view(self.id());
            return false;
        }
        let fd = backend.gui_event_fd();
        let Some(hooks) = LoopHooks::register(run_loop, Arc::downgrade(&self.host), fd) else {
            self.host.detach_view(self.id());
            backend.gui_close();
            return false;
        };
        let mut state = self.wayland.lock().unwrap_or_else(PoisonError::into_inner);
        if !matches!(*state, WaylandState::Attaching) {
            drop(state);
            drop(hooks);
            if self.host.detach_view(self.id()) {
                backend.gui_close();
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
            #[cfg(target_os = "linux")]
            let unknown = frame.as_ref().map(|f| f.as_ptr().cast::<FUnknown>());
            self.host.set_current_frame(self.id(), frame);
            // SAFETY: `unknown` points at the frame just stored by
            // `set_current_frame`, which keeps it retained across this call.
            #[cfg(target_os = "linux")]
            unsafe {
                self.host.update_run_loop_registration(unknown);
            }
        }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: `removed` has no preconditions beyond `&self`; it detaches whatever is attached,
        // and does nothing if the view is already detached.
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
                return if self.attach_wayland(parent) {
                    kResultOk
                } else {
                    kResultFalse
                };
            }
        }
        self.host.attach_view(self.id());
        // SAFETY: `parent` is non-null and is the host's native window for this platform, which the
        // host keeps alive until `removed`, i.e. for the editor session `gui_open` requires.
        if !unsafe { self.host.backend().gui_open(parent) } {
            self.host.detach_view(self.id());
            return kResultFalse;
        }
        kResultOk
    }

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
            self.host.backend().gui_close();
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
        // SAFETY: The host passes `size` as null or a writable `ViewRect`; `as_mut` maps null to
        // `None`.
        let Some(r) = (unsafe { size.as_mut() }) else {
            return kResultFalse;
        };
        let (w, h) = self.host.backend().gui_get_size();
        *r = ViewRect {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        kResultOk
    }

    unsafe fn onSize(&self, size: *mut ViewRect) -> tresult {
        // SAFETY: The host passes `size` as null or a readable `ViewRect`; `as_ref` maps null to
        // `None`.
        if let Some(r) = unsafe { size.as_ref() } {
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            if w > 0 && h > 0 {
                let backend = self.host.backend();
                #[cfg(target_os = "linux")]
                let wayland = self.is_wayland_attached();
                #[cfg(not(target_os = "linux"))]
                let wayland = false;
                if wayland {
                    backend.gui_wayland_set_sync(true);
                }
                backend.gui_set_size(w as u32, h as u32);
                if wayland {
                    backend.gui_on_fd();
                    backend.gui_wayland_set_sync(false);
                }
            }
        }
        kResultOk
    }

    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
        // SAFETY: The host passes `frame` as null or a live `IPlugFrame`; `retain` takes our own
        // reference.
        self.set_frame(unsafe { retain(frame) });
        kResultOk
    }

    unsafe fn canResize(&self) -> tresult {
        if self.host.backend().gui_can_resize() {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn checkSizeConstraint(&self, rect: *mut ViewRect) -> tresult {
        // SAFETY: The host passes `rect` as null or a read-write `ViewRect`; `as_mut` maps null to
        // `None`.
        if let Some(r) = unsafe { rect.as_mut() } {
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            if w > 0 && h > 0 {
                let (w, h) = self
                    .host
                    .backend()
                    .gui_check_size_constraint(w as u32, h as u32);
                r.right = r.left + w as i32;
                r.bottom = r.top + h as i32;
            }
        }
        kResultOk
    }
}

impl IPlugViewContentScaleSupportTrait for View {
    unsafe fn setContentScaleFactor(&self, factor: ScaleFactor) -> tresult {
        if cfg!(target_os = "macos") {
            return kResultFalse;
        }
        #[cfg(target_os = "linux")]
        if self.is_wayland_attached() {
            return kResultFalse;
        }
        self.host.backend().gui_set_content_scale(f64::from(factor));
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
        // SAFETY: The host passes `result_tag` as null or a writable `ParamID`; `as_mut` maps null
        // to `None`.
        let Some(out) = (unsafe { result_tag.as_mut() }) else {
            return kInvalidArgument;
        };
        if let Some(id) = self.host.backend().gui_param_at(x_pos, y_pos) {
            *out = id;
            kResultOk
        } else {
            kResultFalse
        }
    }
}
