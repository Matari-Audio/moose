//! Getting work onto the host UI thread.
//!
//! VST3 wants `restartComponent`, edit gestures and `resizeView` on the UI
//! thread, but the audio thread and editor workers produce them. Each OS
//! gets one wake that runs a host-object callback on that thread:
//! - Linux: the host `IRunLoop` (eventfd handlers and a 30 ms timer).
//! - macOS: a main-queue libdispatch source.
//! - Windows: a null-HWND thread timer on the creating (UI) thread.
//!
//! Without a wake, pending work drains on the next host UI-thread call
//! (parameter reads, edit gestures).

#![cfg_attr(not(any(target_os = "linux", target_os = "macos", target_os = "windows")), allow(dead_code))]

/// Poll interval of the Linux run-loop timer and the Windows thread timer.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(super) const RESTART_TIMER_MS: u32 = 30;

#[cfg(target_os = "linux")]
pub(super) mod linux {
    use std::sync::Weak;

    use vst3::Steinberg::Linux::{
        IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler,
        ITimerHandlerTrait,
    };
    use vst3::Steinberg::{FUnknown, kResultOk};
    use vst3::{Class, ComPtr, ComRef, ComWrapper};

    use super::super::Host;

    /// The host run loop of a context object (factory context, `initialize`
    /// context or plug frame), if it offers one.
    pub(in super::super) fn run_loop_of(context: *mut FUnknown) -> Option<ComPtr<IRunLoop>> {
        unsafe { ComRef::from_raw(context) }?.cast::<IRunLoop>()
    }

    struct FdHandler {
        host: Weak<Host>,
        fd: libc::c_int,
        on_fd: fn(&Host),
    }

    impl Class for FdHandler {
        type Interfaces = (IEventHandler,);
    }

    impl IEventHandlerTrait for FdHandler {
        unsafe fn onFDIsSet(&self, _fd: vst3::Steinberg::Linux::FileDescriptor) {
            let mut value = 0u64;
            loop {
                let r = unsafe { libc::read(self.fd, (&raw mut value).cast(), 8) };
                if r >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
            }
            if let Some(host) = self.host.upgrade() {
                (self.on_fd)(&host);
            }
        }
    }

    /// An eventfd registered with a host run loop. Any thread may
    /// `notify`; the run loop then calls `on_fd` on the UI thread.
    pub(in super::super) struct FdWake {
        run_loop: ComPtr<IRunLoop>,
        handler: ComPtr<IEventHandler>,
        fd: libc::c_int,
    }

    impl FdWake {
        pub fn bind(run_loop: ComPtr<IRunLoop>, host: Weak<Host>, on_fd: fn(&Host)) -> Option<Self> {
            let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            if fd < 0 {
                return None;
            }
            let handler = ComWrapper::new(FdHandler { host, fd, on_fd })
                .to_com_ptr::<IEventHandler>()?;
            if unsafe { run_loop.registerEventHandler(handler.as_ptr(), fd) } != kResultOk {
                unsafe { libc::close(fd) };
                return None;
            }
            Some(Self {
                run_loop,
                handler,
                fd,
            })
        }

        /// Wake the UI thread. A full counter (EAGAIN) means a wake is
        /// already pending, which is just as good.
        pub fn notify(&self) {
            Self::notify_fd(self.fd);
        }

        pub fn fd(&self) -> libc::c_int {
            self.fd
        }

        pub fn notify_fd(fd: libc::c_int) {
            let one = 1u64;
            loop {
                let r = unsafe { libc::write(fd, (&raw const one).cast(), 8) };
                if r >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }

    impl Drop for FdWake {
        fn drop(&mut self) {
            unsafe {
                self.run_loop.unregisterEventHandler(self.handler.as_ptr());
                libc::close(self.fd);
            }
        }
    }

    struct TimerHandler {
        host: Weak<Host>,
    }

    impl Class for TimerHandler {
        type Interfaces = (ITimerHandler,);
    }

    impl ITimerHandlerTrait for TimerHandler {
        unsafe fn onTimer(&self) {
            if let Some(host) = self.host.upgrade() {
                host.on_ui_tick();
            }
        }
    }

    /// The UI-thread timer, registered while a plug frame is bound.
    pub(in super::super) struct TimerWake {
        run_loop: ComPtr<IRunLoop>,
        handler: ComPtr<ITimerHandler>,
    }

    impl TimerWake {
        pub fn bind(run_loop: ComPtr<IRunLoop>, host: Weak<Host>) -> Option<Self> {
            let handler = ComWrapper::new(TimerHandler { host }).to_com_ptr::<ITimerHandler>()?;
            let ms = u64::from(super::RESTART_TIMER_MS);
            if unsafe { run_loop.registerTimer(handler.as_ptr(), ms) } != kResultOk {
                return None;
            }
            Some(Self { run_loop, handler })
        }
    }

    impl Drop for TimerWake {
        fn drop(&mut self) {
            unsafe { self.run_loop.unregisterTimer(self.handler.as_ptr()) };
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) mod macos {
    use std::ffi::{c_int, c_void};

    #[repr(C)]
    struct Opaque([u8; 0]);

    unsafe extern "C" {
        static _dispatch_source_type_data_or: Opaque;
        static _dispatch_main_q: Opaque;
        fn dispatch_source_create(
            kind: *const Opaque,
            handle: usize,
            mask: usize,
            queue: *const Opaque,
        ) -> *mut c_void;
        fn dispatch_set_context(object: *mut c_void, context: *mut c_void);
        fn dispatch_source_set_event_handler_f(
            source: *mut c_void,
            handler: unsafe extern "C" fn(*mut c_void),
        );
        fn dispatch_resume(object: *mut c_void);
        fn dispatch_source_merge_data(source: *mut c_void, value: usize);
        fn dispatch_source_cancel(source: *mut c_void);
        fn dispatch_release(object: *mut c_void);
        fn pthread_main_np() -> c_int;
    }

    /// A main-queue `DATA_OR` source. `signal` is lock- and allocation-free,
    /// so the audio thread may call it.
    pub(in super::super) struct MainQueueWake(*mut c_void);

    // SAFETY: libdispatch objects are thread-safe.
    unsafe impl Send for MainQueueWake {}
    unsafe impl Sync for MainQueueWake {}

    impl MainQueueWake {
        /// `handler(context)` runs on the main queue after each `signal`.
        pub fn new(context: *mut c_void, handler: unsafe extern "C" fn(*mut c_void)) -> Option<Self> {
            unsafe {
                let source = dispatch_source_create(
                    &raw const _dispatch_source_type_data_or,
                    0,
                    0,
                    &raw const _dispatch_main_q,
                );
                if source.is_null() {
                    return None;
                }
                dispatch_set_context(source, context);
                dispatch_source_set_event_handler_f(source, handler);
                dispatch_resume(source);
                Some(Self(source))
            }
        }

        pub fn signal(&self) {
            unsafe { dispatch_source_merge_data(self.0, 1) };
        }
    }

    impl Drop for MainQueueWake {
        fn drop(&mut self) {
            // A handler already running keeps the source alive; it resolves
            // its instance through the registry, which no longer has it.
            unsafe {
                dispatch_source_cancel(self.0);
                dispatch_release(self.0);
            }
        }
    }

    pub(in super::super) fn on_main_thread() -> bool {
        unsafe { pthread_main_np() != 0 }
    }
}

#[cfg(target_os = "windows")]
pub(super) mod windows {
    use std::ffi::c_void;

    type TimerProc = unsafe extern "system" fn(*mut c_void, u32, usize, u32);

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetTimer(hwnd: *mut c_void, id: usize, ms: u32, proc_: Option<TimerProc>) -> usize;
        fn KillTimer(hwnd: *mut c_void, id: usize) -> i32;
    }

    /// A thread timer on the creating thread's message queue. The host UI
    /// thread creates instances, so `proc_` runs there for the instance's
    /// whole life; a scan thread without a pump simply never fires it.
    pub(in super::super) struct ThreadTimer {
        pub id: usize,
        pub thread: std::thread::ThreadId,
    }

    impl ThreadTimer {
        pub fn new(proc_: TimerProc) -> Option<Self> {
            let id = unsafe { SetTimer(std::ptr::null_mut(), 0, super::RESTART_TIMER_MS, Some(proc_)) };
            (id != 0).then(|| Self {
                id,
                thread: std::thread::current().id(),
            })
        }
    }

    impl Drop for ThreadTimer {
        fn drop(&mut self) {
            // Fails off the creating thread; the orphaned proc then finds
            // no instance with this id and does nothing.
            unsafe { KillTimer(std::ptr::null_mut(), self.id) };
        }
    }
}
