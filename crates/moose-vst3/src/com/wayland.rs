//! Embedding an editor in a Wayland host (`kPlatformTypeWaylandSurfaceID`).
//!
//! The plugin runs on the host's own Wayland connection
//! (`IWaylandHost::openWaylandConnection`) and the host's UI loop: the
//! editor's fd goes to `IRunLoop::registerEventHandler` and a 16 ms
//! `IRunLoop` timer covers events the host read off the shared display.

use std::ptr::NonNull;
use std::sync::Weak;

use vst3::Steinberg::Linux::{
    IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler, ITimerHandlerTrait,
};
use vst3::Steinberg::Vst::{IHostApplication, IHostApplicationTrait};
use vst3::Steinberg::{
    FUnknown, IWaylandHost, IWaylandHost_iid, IWaylandHostTrait, TUID, kResultOk,
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

/// An open `openWaylandConnection`, closed on drop.
pub(super) struct Connection {
    host: ComPtr<IWaylandHost>,
    display: NonNull<wl_display>,
}

impl Connection {
    pub(super) fn open(host: ComPtr<IWaylandHost>) -> Option<Self> {
        // SAFETY: `host` is a live interface.
        let display = NonNull::new(unsafe { host.openWaylandConnection() })?;
        Some(Self { host, display })
    }

    pub(super) fn display(&self) -> *mut wl_display {
        self.display.as_ptr()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: `display` came from this host's `openWaylandConnection`
        // and is closed exactly once.
        unsafe { self.host.closeWaylandConnection(self.display.as_ptr()) };
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
