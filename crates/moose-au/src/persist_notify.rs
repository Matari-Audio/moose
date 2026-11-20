//! `AUv2` main-queue dispatch-source wake for the instance's [`HostQueue`].
//!
//! [`HostQueue`]: moose_core::host_queue::HostQueue
use moose_core::editor::SendPtr;
use moose_core::export::PluginExport;
use moose_core::host_queue::HostQueue;
use moose_core::thread::MainThread;
use moose_core::wrapper::run_extern_callback_with;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

unsafe extern "C" {
    static _dispatch_source_type_data_or: c_void;
    static _dispatch_main_q: c_void;
    fn dispatch_source_create(
        kind: *const c_void,
        handle: usize,
        mask: usize,
        queue: *mut c_void,
    ) -> *mut c_void;
    fn dispatch_set_context(source: *mut c_void, context: *mut c_void);
    fn dispatch_source_set_event_handler_f(
        source: *mut c_void,
        callback: unsafe extern "C" fn(*mut c_void),
    );
    fn dispatch_source_set_cancel_handler_f(
        source: *mut c_void,
        callback: unsafe extern "C" fn(*mut c_void),
    );
    fn dispatch_resume(source: *mut c_void);
    fn dispatch_source_merge_data(source: *mut c_void, data: usize);
    fn dispatch_source_cancel(source: *mut c_void);
    fn dispatch_release(source: *mut c_void);
}

struct DrainContext {
    queue: Arc<HostQueue>,
    ctx: SendPtr<c_void>,
}

pub(super) struct Dispatcher {
    source: SendPtr<c_void>,
    activated: AtomicBool,
}

impl Dispatcher {
    /// Create a suspended main-queue dispatch source and its thread-safe
    /// wake closure. Call [`Self::activate`] once the instance has its
    /// stable heap address.
    pub(super) unsafe fn new() -> Option<(Self, impl Fn() + Send + Sync + 'static)> {
        // SAFETY: libdispatch's exported opaque type and queue objects have
        // static process lifetime; the source is resumed once in `activate`
        // (or during `Drop` if creation unwinds early) and cancelled +
        // released in `Drop`.
        unsafe {
            let raw = dispatch_source_create(
                &raw const _dispatch_source_type_data_or,
                0,
                0,
                (&raw const _dispatch_main_q).cast_mut(),
            );
            if raw.is_null() {
                return None;
            }
            let source = SendPtr::new(raw);
            let wake = move || {
                dispatch_source_merge_data(source.as_ptr().cast_mut(), 1);
            };
            Some((
                Self {
                    source,
                    activated: AtomicBool::new(false),
                },
                wake,
            ))
        }
    }

    /// Bind the instance pointer and resume the main-queue source.
    pub(super) unsafe fn activate<P: PluginExport>(&self, queue: Arc<HostQueue>, ctx: *mut c_void) {
        let raw = self.source.as_ptr().cast_mut();
        let drain_ctx = Box::into_raw(Box::new(DrainContext {
            queue,
            // SAFETY: `drain` only dereferences `ctx` inside `queue.with_open`,
            // which `cb_destroy` waits out via `host_queue.close()` before
            // freeing the instance.
            ctx: unsafe { SendPtr::new(ctx.cast_const()) },
        }));
        unsafe {
            dispatch_set_context(raw, drain_ctx.cast());
            dispatch_source_set_event_handler_f(raw, drain::<P>);
            dispatch_source_set_cancel_handler_f(raw, release_context);
            dispatch_resume(raw);
        }
        self.activated.store(true, Ordering::Release);
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        let raw = self.source.as_ptr().cast_mut();
        // SAFETY: libdispatch forbids releasing a suspended source. If
        // creation unwound before `activate`, cancel and resume before
        // releasing so disposal never aborts the host. When activated,
        // `cb_destroy` has already closed `HostQueue` (blocking future
        // drains and waiting out any in-flight drain), and `release_context`
        // frees `DrainContext` once libdispatch finishes cancellation.
        unsafe {
            dispatch_source_cancel(raw);
            if !self.activated.load(Ordering::Acquire) {
                dispatch_resume(raw);
            }
            dispatch_release(raw);
        }
    }
}

unsafe extern "C" fn drain<P: PluginExport>(raw: *mut c_void) {
    run_extern_callback_with::<P, ()>("au", "host_queue_drain", (), || unsafe {
        let context = &*raw.cast::<DrainContext>();
        context.queue.with_open(|| {
            let main = MainThread::assume();
            crate::gui::drain_host_queue::<P>(context.ctx.as_ptr().cast_mut(), &main);
        });
    });
}

unsafe extern "C" fn release_context(raw: *mut c_void) {
    if !raw.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(raw.cast::<DrainContext>()));
        }));
    }
}
