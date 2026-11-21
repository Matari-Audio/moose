//! Getting work onto the host UI thread.
//!
//! VST3 wants `restartComponent`, edit gestures and `resizeView` on the UI
//! thread, but the audio thread and editor workers produce them. Each OS
//! gets an event-driven wake that runs a host-object callback on that thread
//! without polling:
//! - Linux: the host `IRunLoop` with an `eventfd` handler (falling back to a
//!   run-loop timer only if the host refuses `registerEventHandler`).
//! - macOS: a main-queue libdispatch `DATA_OR` source.
//! - Windows: a message-only window (`HWND_MESSAGE`) woken with `PostMessageW`.
//!
//! Without a wake, pending work drains on the next host UI-thread call
//! (parameter reads, edit gestures).

#![cfg_attr(
    not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
    allow(dead_code)
)]

/// Fallback poll interval on Linux only when a host `IRunLoop` refuses
/// `registerEventHandler`.
#[cfg(target_os = "linux")]
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
                if r >= 0
                    || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
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
        pub fn bind(
            run_loop: ComPtr<IRunLoop>,
            host: Weak<Host>,
            on_fd: fn(&Host),
        ) -> Option<Self> {
            let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            if fd < 0 {
                return None;
            }
            let handler =
                ComWrapper::new(FdHandler { host, fd, on_fd }).to_com_ptr::<IEventHandler>()?;
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
                if r >= 0
                    || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
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
        pub fn new(
            context: *mut c_void,
            handler: unsafe extern "C" fn(*mut c_void),
        ) -> Option<Self> {
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
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU8, Ordering};

    const HWND_MESSAGE: *mut c_void = (-3isize) as *mut c_void;
    const WM_CLOSE: u32 = 0x0010;
    const WM_MOOSE_WAKE: u32 = 0x8001; // WM_APP + 1
    const GWLP_USERDATA: i32 = -21;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;
    const DROP_CLOSE_TIMEOUT_MS: u32 = 500;
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;

    const WAKE_IDLE: u8 = 0;
    const WAKE_POSTING: u8 = 1;
    const WAKE_RETRY: u8 = 3;

    type WndProc = unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize;

    #[repr(C)]
    struct WndClassExW {
        cb_size: u32,
        style: u32,
        lpfn_wnd_proc: Option<WndProc>,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: *mut c_void,
        h_icon: *mut c_void,
        h_cursor: *mut c_void,
        hbr_background: *mut c_void,
        lpsz_menu_name: *const u16,
        lpsz_class_name: *const u16,
        h_icon_sm: *mut c_void,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn GetModuleHandleExW(
            flags: u32,
            module_name: *const c_void,
            module: *mut *mut c_void,
        ) -> i32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn UnregisterClassW(name: *const u16, instance: *mut c_void) -> i32;
        fn RegisterClassExW(wc: *const WndClassExW) -> u16;
        fn CreateWindowExW(
            ex_style: u32,
            class_name: *const u16,
            window_name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: *mut c_void,
            menu: *mut c_void,
            instance: *mut c_void,
            param: *mut c_void,
        ) -> *mut c_void;
        fn DestroyWindow(hwnd: *mut c_void) -> i32;
        fn PostMessageW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> i32;
        fn SendMessageTimeoutW(
            hwnd: *mut c_void,
            msg: u32,
            wparam: usize,
            lparam: isize,
            flags: u32,
            timeout: u32,
            result: *mut usize,
        ) -> isize;
        fn DefWindowProcW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize;
        fn SetWindowLongPtrW(hwnd: *mut c_void, index: i32, new_long: isize) -> isize;
        fn GetWindowLongPtrW(hwnd: *mut c_void, index: i32) -> isize;
    }

    static CLASS_NAME: [u16; 19] = [
        b'M' as u16,
        b'o' as u16,
        b'o' as u16,
        b's' as u16,
        b'e' as u16,
        b'V' as u16,
        b's' as u16,
        b't' as u16,
        b'3' as u16,
        b'W' as u16,
        b'a' as u16,
        b'k' as u16,
        b'e' as u16,
        b'W' as u16,
        b'i' as u16,
        b'n' as u16,
        b'd' as u16,
        b'w' as u16,
        0,
    ];

    static HANDLER: OnceLock<fn(*mut c_void)> = OnceLock::new();
    static CLASS_USERS: std::sync::Mutex<usize> = std::sync::Mutex::new(0);

    unsafe extern "system" fn wnd_proc(
        hwnd: *mut c_void,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        if msg == WM_MOOSE_WAKE {
            let ctx = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut c_void;
            if !ctx.is_null()
                && let Some(handler) = HANDLER.get()
            {
                handler(ctx);
            }
            return 0;
        }
        if msg == WM_CLOSE {
            return unsafe { DestroyWindow(hwnd) } as isize;
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn retain_module() -> *mut c_void {
        let mut hmod = std::ptr::null_mut();
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                wnd_proc as *const c_void,
                &raw mut hmod,
            );
        }
        hmod
    }

    fn acquire_class(hinst: *mut c_void) -> bool {
        let mut users = CLASS_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *users == 0 {
            let wc = WndClassExW {
                cb_size: u32::try_from(std::mem::size_of::<WndClassExW>()).unwrap_or(0),
                style: 0,
                lpfn_wnd_proc: Some(wnd_proc),
                cb_cls_extra: 0,
                cb_wnd_extra: 0,
                h_instance: hinst,
                h_icon: std::ptr::null_mut(),
                h_cursor: std::ptr::null_mut(),
                hbr_background: std::ptr::null_mut(),
                lpsz_menu_name: std::ptr::null(),
                lpsz_class_name: CLASS_NAME.as_ptr(),
                h_icon_sm: std::ptr::null_mut(),
            };
            if unsafe { RegisterClassExW(&raw const wc) } == 0 {
                return false;
            }
        }
        *users += 1;
        true
    }

    fn release_class_and_module(hinst: *mut c_void) {
        let mut users = CLASS_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *users -= 1;
        if *users == 0 && unsafe { UnregisterClassW(CLASS_NAME.as_ptr(), hinst) } == 0 {
            // The class still refers to wnd_proc. Pin this registration.
            *users = 1;
            return;
        }
        drop(users);
        unsafe { FreeLibrary(hinst) };
    }

    /// Event-driven Win32 message-only window (`HWND_MESSAGE`) wake.
    ///
    /// Replaces periodic `SetTimer` polling: `signal` coalesces bursts via an
    /// atomic flag and posts a single `WM_MOOSE_WAKE` directly to the host UI
    /// thread's message queue (zero polling, allocation-free).
    pub(in super::super) struct MessageWake {
        hwnd: *mut c_void,
        module: *mut c_void,
        pub thread: std::thread::ThreadId,
        state: AtomicU8,
    }

    // SAFETY: `PostMessageW` and `SendMessageTimeoutW` are thread-safe per
    // Win32; `DestroyWindow` runs on the owning thread (directly when dropped
    // on it, or synchronously via `SendMessageTimeoutW(..., WM_CLOSE, ...)`).
    unsafe impl Send for MessageWake {}
    unsafe impl Sync for MessageWake {}

    impl MessageWake {
        pub fn new(ctx: *mut c_void, handler: fn(*mut c_void)) -> Option<Self> {
            let _ = HANDLER.set(handler);
            let hinst = retain_module();
            if hinst.is_null() {
                return None;
            }
            if !acquire_class(hinst) {
                unsafe { FreeLibrary(hinst) };
                return None;
            }
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    CLASS_NAME.as_ptr(),
                    std::ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    std::ptr::null_mut(),
                    hinst,
                    std::ptr::null_mut(),
                )
            };
            if hwnd.is_null() {
                release_class_and_module(hinst);
                return None;
            }
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx as isize) };
            Some(Self {
                hwnd,
                module: hinst,
                thread: std::thread::current().id(),
                state: AtomicU8::new(WAKE_IDLE),
            })
        }

        pub fn signal(&self) {
            loop {
                match self.state.load(Ordering::Acquire) {
                    WAKE_IDLE => {
                        if self
                            .state
                            .compare_exchange(
                                WAKE_IDLE,
                                WAKE_POSTING,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_err()
                        {
                            continue;
                        }
                        let posted = unsafe { PostMessageW(self.hwnd, WM_MOOSE_WAKE, 0, 0) } != 0;
                        if posted {
                            // The UI may already have cleared this attempt and started
                            // another. Only the UI may clear a successful post.
                            return;
                        }
                        // `PostMessageW` failed (queue full / closing): restore
                        // `WAKE_IDLE` so future signals can retry instead of
                        // wedging on `pending=true`.
                        let prev = self.state.swap(WAKE_IDLE, Ordering::AcqRel);
                        if prev != WAKE_RETRY {
                            return;
                        }
                    }
                    WAKE_POSTING => {
                        if self
                            .state
                            .compare_exchange(
                                WAKE_POSTING,
                                WAKE_RETRY,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_err()
                        {
                            continue;
                        }
                        return;
                    }
                    _ => return,
                }
            }
        }

        pub fn clear_pending(&self) {
            self.state.store(WAKE_IDLE, Ordering::Release);
        }
    }

    impl Drop for MessageWake {
        fn drop(&mut self) {
            unsafe { SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0) };
            let destroyed = if std::thread::current().id() == self.thread {
                unsafe { DestroyWindow(self.hwnd) != 0 }
            } else {
                // Synchronously dispatch `WM_CLOSE` to the owner thread (bounded
                // by `DROP_CLOSE_TIMEOUT_MS`). Check both dispatch and destruction.
                let mut result = 0usize;
                unsafe {
                    SendMessageTimeoutW(
                        self.hwnd,
                        WM_CLOSE,
                        0,
                        0,
                        SMTO_ABORTIFHUNG,
                        DROP_CLOSE_TIMEOUT_MS,
                        &raw mut result,
                    ) != 0
                        && result != 0
                }
            };
            // On timeout or destruction failure a window may still dispatch
            // wnd_proc. Keep its module reference for the process lifetime.
            if destroyed {
                release_class_and_module(self.module);
            }
        }
    }
}
