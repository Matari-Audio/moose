//! Embedding an editor in a Wayland host (`kPlatformTypeWaylandSurfaceID`).
//!
//! The plugin runs on the host's own Wayland connection
//! (`IWaylandHost::openWaylandConnection`) and the host's UI loop: the
//! editor's fd goes to `IRunLoop::registerEventHandler` and a 16 ms
//! `IRunLoop` timer covers events the host read off the shared display.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock, PoisonError, Weak};

use vst3::Steinberg::Linux::{
    IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler, ITimerHandlerTrait,
};
use vst3::Steinberg::Vst::{IHostApplication, IHostApplicationTrait};
use vst3::Steinberg::{
    FUnknown, IPlugFrame, IWaylandFrame, IWaylandFrameTrait, IWaylandHost, IWaylandHost_iid,
    IWaylandHostTrait, TUID, kResultOk,
};
use vst3::{Class, ComPtr, ComWrapper, wl_display};

use super::component::Host;

/// `Editor::on_timer` period. The editor contract asks for ~16 ms (one
/// 60 Hz frame).
const TIMER_MS: u64 = 16;

/// The host's `IWaylandHost`: the context itself, or one made through its
/// `IHostApplication::createInstance`.
pub(super) fn wayland_host_of(context: &ComPtr<FUnknown>) -> Option<ComPtr<IWaylandHost>> {
    if let Some(host) = context.cast::<IWaylandHost>() {
        return Some(host);
    }
    let app = context.cast::<IHostApplication>()?;
    let mut cid: TUID = IWaylandHost_iid;
    let mut iid: TUID = IWaylandHost_iid;
    let mut obj = std::ptr::null_mut();
    // SAFETY: `app` is a live interface; the TUIDs and out-pointer are
    // valid for the call. On success `obj` carries one reference to an
    // `IWaylandHost` (the iid asked for), which `from_raw` adopts.
    unsafe {
        if app.createInstance(&raw mut cid, &raw mut iid, &raw mut obj) != kResultOk {
            return None;
        }
        ComPtr::from_raw(obj.cast::<IWaylandHost>())
    }
}

/// The plug frame's `IWaylandFrame`, when the host exposes VST3 3.8 Wayland
/// frame queries (`getWaylandSurface`, `getParentSurface`, `getParentToplevel`).
pub(super) fn wayland_frame_of(frame: &ComPtr<IPlugFrame>) -> Option<ComPtr<IWaylandFrame>> {
    frame.cast::<IWaylandFrame>()
}

/// Resolve the parent `wl_surface*` on `display` for `attached(parent, "WaylandSurfaceID")`.
///
/// Per VST3 3.8 `IWaylandFrame`, when the host exposes `IWaylandFrame`,
/// `getWaylandSurface(display)` returns the parent surface for that display
/// (even if `attached` passed a null `parent`). Otherwise fall back to the
/// `parent` pointer passed to `attached`.
pub(super) fn resolve_parent_surface(
    frame: Option<&ComPtr<IWaylandFrame>>,
    display: *mut wl_display,
    parent: *mut c_void,
) -> Option<*mut c_void> {
    if let Some(wf) = frame {
        // SAFETY: `wf` and `display` are live host Wayland interfaces on the UI thread.
        let surface = unsafe { wf.getWaylandSurface(display) };
        if !surface.is_null() {
            return Some(surface.cast());
        }
    }
    (!parent.is_null()).then_some(parent)
}

/// Resolve the host's parent `xdg_surface*` on `display` for popup positioning
/// via VST3 3.8 `IWaylandFrame::getParentSurface`.
#[allow(dead_code, reason = "queried by Wayland popup helpers and unit tests")]
pub(super) fn parent_xdg_surface(
    frame: Option<&ComPtr<IWaylandFrame>>,
    display: *mut wl_display,
) -> Option<*mut c_void> {
    let wf = frame?;
    let mut size: vst3::Steinberg::ViewRect = unsafe { std::mem::zeroed() };
    // SAFETY: `wf` and `display` are live host Wayland interfaces on the UI thread.
    let surface = unsafe { wf.getParentSurface(&raw mut size, display) };
    (!surface.is_null()).then_some(surface.cast())
}

/// Resolve the host's parent `xdg_toplevel*` on `display` for dialogs/popups
/// via VST3 3.8 `IWaylandFrame::getParentToplevel`.
#[allow(dead_code, reason = "queried by Wayland popup helpers and unit tests")]
pub(super) fn parent_xdg_toplevel(
    frame: Option<&ComPtr<IWaylandFrame>>,
    display: *mut wl_display,
) -> Option<*mut c_void> {
    let wf = frame?;
    // SAFETY: `wf` and `display` are live host Wayland interfaces on the UI thread.
    let toplevel = unsafe { wf.getParentToplevel(display) };
    (!toplevel.is_null()).then_some(toplevel.cast())
}

/// One shared `IWaylandHost::openWaylandConnection` entry keyed by the host
/// context pointer, so multiple open plugin views inside the same host share
/// one Wayland connection (matching VSTGUI's `useCount`) and close it only
/// when the last view detaches.
struct SharedEntry {
    context_key: usize,
    host: ComPtr<IWaylandHost>,
    display: NonNull<wl_display>,
    refs: usize,
}

// SAFETY: `open` and `drop` only run on the host's UI thread (attached /
// removed); the mutex protects the process-wide table across instances.
unsafe impl Send for SharedEntry {}

fn shared_connections() -> &'static Mutex<Vec<SharedEntry>> {
    static TABLE: OnceLock<Mutex<Vec<SharedEntry>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(Vec::new()))
}

/// A refcounted handle to the host's `openWaylandConnection`, closed once the
/// last view using the same host context drops.
pub(super) struct Connection {
    context_key: usize,
    display: NonNull<wl_display>,
}

impl Connection {
    pub(super) fn open(context: &ComPtr<FUnknown>) -> Option<Self> {
        let context_key = context.as_ptr() as usize;
        {
            let mut table = shared_connections()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(entry) = table.iter_mut().find(|e| e.context_key == context_key) {
                entry.refs += 1;
                return Some(Self {
                    context_key,
                    display: entry.display,
                });
            }
        }
        let host = wayland_host_of(context)?;
        // SAFETY: `host` is a live interface on the UI thread.
        let display = NonNull::new(unsafe { host.openWaylandConnection() })?;
        let mut table = shared_connections()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = table.iter_mut().find(|e| e.context_key == context_key) {
            // Another view raced us (should not happen on one UI thread, but handle cleanly).
            // SAFETY: close the redundant connection we just opened.
            unsafe { host.closeWaylandConnection(display.as_ptr()) };
            entry.refs += 1;
            return Some(Self {
                context_key,
                display: entry.display,
            });
        }
        table.push(SharedEntry {
            context_key,
            host,
            display,
            refs: 1,
        });
        Some(Self {
            context_key,
            display,
        })
    }

    pub(super) fn display(&self) -> *mut wl_display {
        self.display.as_ptr()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let closing = {
            let mut table = shared_connections()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            table
                .iter()
                .position(|e| e.context_key == self.context_key)
                .and_then(|idx| {
                    table[idx].refs -= 1;
                    if table[idx].refs == 0 {
                        Some(table.swap_remove(idx))
                    } else {
                        None
                    }
                })
        };
        if let Some(entry) = closing {
            // SAFETY: `display` came from this host's `openWaylandConnection`
            // and is closed exactly once after the last view's surfaces are gone.
            unsafe { entry.host.closeWaylandConnection(entry.display.as_ptr()) };
        }
    }
}

// SAFETY: the display is only used on the host UI thread (attached /
// removed); the struct just carries it between those calls.
unsafe impl Send for Connection {}

/// Calls `on` with the instance from a run-loop callback, if it still lives.
struct Hook {
    host: Weak<Host>,
    on: fn(&Host),
}

impl Hook {
    fn fire(&self) {
        if let Some(host) = self.host.upgrade() {
            (self.on)(&host);
        }
    }
}

impl Class for Hook {
    type Interfaces = (IEventHandler, ITimerHandler);
}

impl IEventHandlerTrait for Hook {
    unsafe fn onFDIsSet(&self, _fd: vst3::Steinberg::Linux::FileDescriptor) {
        self.fire();
    }
}

impl ITimerHandlerTrait for Hook {
    unsafe fn onTimer(&self) {
        self.fire();
    }
}

fn on_fd(host: &Host) {
    // SAFETY: `ctx` is the live plugin instance; run loops call on the UI
    // thread.
    unsafe { (host.reg.cb.gui_on_fd)(host.ctx) };
}

fn on_timer(host: &Host) {
    // SAFETY: as in `on_fd`.
    unsafe { (host.reg.cb.gui_on_timer)(host.ctx) };
}

/// The editor's fd handler (when it has an fd) and timer on a run loop,
/// unregistered on drop.
pub(super) struct LoopHooks {
    run_loop: ComPtr<IRunLoop>,
    fd: Option<ComPtr<IEventHandler>>,
    timer: ComPtr<ITimerHandler>,
}

impl LoopHooks {
    /// Registers the timer, and the fd handler when `fd` is `Some`. `None`
    /// when the run loop refuses the timer (the editor can't run without it).
    pub(super) fn register(
        run_loop: ComPtr<IRunLoop>,
        host: Weak<Host>,
        fd: Option<libc::c_int>,
    ) -> Option<Self> {
        let timer = ComWrapper::new(Hook {
            host: host.clone(),
            on: on_timer,
        })
        .to_com_ptr::<ITimerHandler>()?;
        // SAFETY: `run_loop` and `timer` are live interfaces.
        if unsafe { run_loop.registerTimer(timer.as_ptr(), TIMER_MS) } != kResultOk {
            return None;
        }
        let mut hooks = Self {
            run_loop,
            fd: None,
            timer,
        };
        if let Some(fd) = fd {
            let handler =
                ComWrapper::new(Hook { host, on: on_fd }).to_com_ptr::<IEventHandler>()?;
            // SAFETY: `run_loop` and `handler` are live; the editor keeps
            // `fd` open until `gui_close`, which runs after this unregisters.
            if unsafe { hooks.run_loop.registerEventHandler(handler.as_ptr(), fd) } != kResultOk {
                return None;
            }
            hooks.fd = Some(handler);
        }
        Some(hooks)
    }
}

impl Drop for LoopHooks {
    fn drop(&mut self) {
        // SAFETY: both handlers were registered with this run loop.
        unsafe {
            if let Some(fd) = &self.fd {
                self.run_loop.unregisterEventHandler(fd.as_ptr());
            }
            self.run_loop.unregisterTimer(self.timer.as_ptr());
        }
    }
}
