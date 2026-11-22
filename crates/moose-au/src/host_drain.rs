//! Event-driven main-queue drain and `MooseAuHostSink` vtable wrapper for AU.
//!
//! Every host-bound event (parameter edits/feedback, `LATENCY`, `TAIL`,
//! `PERSIST`, `PRESENTATION`, `PARAM_RESCAN`, `MARK_DIRTY`, `RESTART`,
//! `HostNotice`) wakes a per-instance `DISPATCH_SOURCE_TYPE_DATA_OR` on
//! `_dispatch_main_q` (`#[cfg(target_vendor = "apple")]`: macOS `AUv2`, macOS
//! `AUv3`, iOS `AUv3`). Zero polling timers, zero frame-rate caps; `HostQueue` and
//! `dispatch_source_merge_data` coalesce bursts into a single main-queue turn.

use crate::ffi::MooseAuHostSink;

// ---------------------------------------------------------------------------
// HostSink: value-copied vtable wrapper with single-run release on Drop
// ---------------------------------------------------------------------------

/// Owned copy of [`MooseAuHostSink`] stored as the **last** field of
/// `AuInstance<P>` so `release_user_data` runs once after `host_queue.close()`
/// and editor teardown complete.
pub(crate) struct HostSink(MooseAuHostSink);

impl HostSink {
    /// No-op sink used by `create(void)` callers and null pointers.
    pub(crate) fn empty() -> Self {
        Self(MooseAuHostSink::default())
    }

    /// Copy only complete fields whose full extent fits inside `(*raw).size`
    /// (preventing partial pointer overwrites for truncated prefixes while
    /// remaining forward-compatible with larger future vtable revisions).
    ///
    /// # Safety
    /// `raw` is either null or points to a readable `MooseAuHostSink` prefix of
    /// at least `(*raw).size` bytes (or `size_of::<MooseAuHostSink>()` bytes).
    pub(crate) unsafe fn from_raw(raw: *const MooseAuHostSink) -> Self {
        if raw.is_null() {
            return Self::empty();
        }
        let declared = unsafe { (*raw).size as usize };
        if declared < std::mem::size_of::<u32>() * 2 {
            return Self::empty();
        }
        let mut copy = MooseAuHostSink::default();
        macro_rules! copy_field {
            ($field:ident) => {{
                let end = std::mem::offset_of!(MooseAuHostSink, $field)
                    + std::mem::size_of_val(&copy.$field);
                if declared >= end {
                    copy.$field = unsafe { std::ptr::addr_of!((*raw).$field).read() };
                }
            }};
        }
        copy_field!(_reserved);
        copy_field!(user_data);
        copy_field!(release_user_data);
        copy_field!(begin_edit);
        copy_field!(set_param);
        copy_field!(end_edit);
        copy_field!(latency_changed);
        copy_field!(tail_changed);
        copy_field!(param_values_changed);
        copy_field!(param_presentation_changed);
        copy_field!(request_resize);
        copy_field!(state_changed);
        copy_field!(audio_ports_changed);
        #[allow(clippy::cast_possible_truncation)]
        {
            copy.size = declared.min(std::mem::size_of::<MooseAuHostSink>()) as u32;
        }
        Self(copy)
    }

    #[must_use]
    pub(crate) fn has_set_param(&self) -> bool {
        self.0.set_param.is_some()
    }

    pub(crate) unsafe fn begin_edit(&self, param_id: u32) {
        if let Some(f) = self.0.begin_edit {
            unsafe { f(self.0.user_data, param_id) };
        }
    }

    pub(crate) unsafe fn set_param(&self, param_id: u32, plain_value: f32) {
        if let Some(f) = self.0.set_param {
            unsafe { f(self.0.user_data, param_id, plain_value) };
        }
    }

    pub(crate) unsafe fn end_edit(&self, param_id: u32) {
        if let Some(f) = self.0.end_edit {
            unsafe { f(self.0.user_data, param_id) };
        }
    }

    #[cfg_attr(
        not(any(target_vendor = "apple", test)),
        allow(
            dead_code,
            reason = "called by drain_host_queue on Apple targets and in tests"
        )
    )]
    pub(crate) unsafe fn latency_changed(&self) {
        if let Some(f) = self.0.latency_changed {
            unsafe { f(self.0.user_data) };
        }
    }

    #[cfg_attr(
        not(any(target_vendor = "apple", test)),
        allow(
            dead_code,
            reason = "called by drain_host_queue on Apple targets and in tests"
        )
    )]
    pub(crate) unsafe fn tail_changed(&self) {
        if let Some(f) = self.0.tail_changed {
            unsafe { f(self.0.user_data) };
        }
    }

    #[cfg_attr(
        not(any(target_vendor = "apple", test)),
        allow(
            dead_code,
            reason = "called by drain_host_queue on Apple targets and in tests"
        )
    )]
    pub(crate) unsafe fn param_values_changed(&self) {
        if let Some(f) = self.0.param_values_changed {
            unsafe { f(self.0.user_data) };
        }
    }

    #[cfg_attr(
        not(any(target_vendor = "apple", test)),
        allow(
            dead_code,
            reason = "called by drain_host_queue on Apple targets and in tests"
        )
    )]
    pub(crate) unsafe fn param_presentation_changed(&self) {
        if let Some(f) = self.0.param_presentation_changed {
            unsafe { f(self.0.user_data) };
        }
    }

    #[cfg_attr(
        not(any(target_vendor = "apple", test)),
        allow(
            dead_code,
            reason = "called by drain_host_queue on Apple targets and in tests"
        )
    )]
    pub(crate) unsafe fn state_changed(&self) {
        if let Some(f) = self.0.state_changed {
            unsafe { f(self.0.user_data) };
        }
    }

    #[cfg_attr(
        not(any(target_vendor = "apple", test)),
        allow(
            dead_code,
            reason = "called by drain_host_queue on Apple targets and in tests"
        )
    )]
    pub(crate) unsafe fn audio_ports_changed(&self) {
        if let Some(f) = self.0.audio_ports_changed {
            unsafe { f(self.0.user_data) };
        }
    }

    pub(crate) unsafe fn request_resize(&self, width: u32, height: u32) -> bool {
        if let Some(f) = self.0.request_resize {
            unsafe { f(self.0.user_data, width, height) };
            true
        } else {
            false
        }
    }
}

impl Drop for HostSink {
    fn drop(&mut self) {
        if let Some(release) = self.0.release_user_data.take() {
            let ud = self.0.user_data;
            self.0.user_data = std::ptr::null_mut();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                release(ud);
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Apple libdispatch per-instance event-driven DATA_OR wake (macOS + iOS)
// ---------------------------------------------------------------------------

#[cfg(target_vendor = "apple")]
mod apple {
    use crate::instance::AuCtx;
    use moose_core::export::PluginExport;
    use moose_core::host_queue::HostQueue;
    use moose_core::wrapper::run_extern_callback_with;
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Owned `dispatch_source_t` handle safe to wake from any thread via
    /// `dispatch_source_merge_data` until `Dispatcher::drop` cancels and
    /// releases it after `HostQueue::close`.
    #[derive(Clone, Copy)]
    struct DispatchSource(*mut c_void);

    // SAFETY: libdispatch `dispatch_source_t` objects are thread-safe for
    // `dispatch_source_merge_data`, and cancellation/release is serialized by
    // `HostQueue::close` before `Dispatcher::drop`.
    unsafe impl Send for DispatchSource {}
    unsafe impl Sync for DispatchSource {}

    impl DispatchSource {
        #[inline]
        fn as_ptr(self) -> *mut c_void {
            self.0
        }
    }

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
        ctx: AuCtx,
    }

    pub(crate) struct Dispatcher {
        source: DispatchSource,
        activated: AtomicBool,
    }

    impl Dispatcher {
        /// Create a suspended main-queue `DATA_OR` dispatch source and its
        /// thread-safe, allocation-free wake closure.
        pub(crate) unsafe fn new() -> Option<(Self, impl Fn() + Send + Sync + 'static)> {
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
                let source = DispatchSource(raw);
                let wake = move || {
                    dispatch_source_merge_data(source.as_ptr(), 1);
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
        pub(crate) unsafe fn activate<P: PluginExport>(
            &self,
            queue: Arc<HostQueue>,
            ctx: *mut c_void,
        ) {
            let raw = self.source.0;
            let drain_ctx = Box::into_raw(Box::new(DrainContext {
                queue,
                ctx: AuCtx(ctx),
            }));
            unsafe {
                dispatch_set_context(raw, drain_ctx.cast());
                dispatch_source_set_event_handler_f(raw, drain::<P>);
                dispatch_source_set_cancel_handler_f(raw, release_context);
                dispatch_resume(raw);
            }
            self.activated.store(true, Ordering::Release);
        }

        /// Coalesced event-driven wake of the main-queue drain (safe from any
        /// thread, allocation-free).
        pub(crate) fn trigger_now(&self) {
            if self.activated.load(Ordering::Acquire) {
                unsafe {
                    dispatch_source_merge_data(self.source.0, 1);
                }
            }
        }
    }

    impl Drop for Dispatcher {
        fn drop(&mut self) {
            let raw = self.source.0;
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
            super::run_host_drain::<P>(&context.queue, context.ctx.as_ptr());
        });
    }

    unsafe extern "C" fn release_context(raw: *mut c_void) {
        if !raw.is_null() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                drop(Box::from_raw(raw.cast::<DrainContext>()));
            }));
        }
    }
}

#[cfg(not(target_vendor = "apple"))]
mod non_apple {
    use moose_core::export::PluginExport;
    use moose_core::host_queue::HostQueue;
    use std::ffi::c_void;
    use std::sync::Arc;

    pub(crate) struct Dispatcher;

    impl Dispatcher {
        #[allow(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            reason = "matches the Apple Dispatcher::new signature"
        )]
        pub(crate) unsafe fn new() -> Option<(Self, impl Fn() + Send + Sync + 'static)> {
            Some((Self, || {}))
        }

        #[allow(clippy::unused_self)]
        pub(crate) unsafe fn activate<P: PluginExport>(
            &self,
            _queue: Arc<HostQueue>,
            _ctx: *mut c_void,
        ) {
        }

        #[allow(clippy::unused_self)]
        pub(crate) fn trigger_now(&self) {}
    }
}

/// Execute one main-queue drain under `HostQueue::with_open`, finalizing any
/// re-entrant `cb_destroy` requested by a host callback only AFTER `with_open`
/// unregisters so `HostQueue::close` cannot deadlock waiting on itself.
///
/// # Safety
/// `ctx` must point to a live `AuInstance<P>` associated with `queue`.
pub(crate) unsafe fn run_host_drain<P: moose_core::export::PluginExport>(
    queue: &moose_core::host_queue::HostQueue,
    ctx: *mut std::ffi::c_void,
) {
    unsafe {
        crate::gui::with_host_open::<P, _>(queue, ctx, || {
            let main = moose_core::thread::MainThread::assume();
            crate::gui::drain_host_queue::<P>(ctx, &main);
        });
    }
}

#[cfg(target_vendor = "apple")]
pub(crate) use apple::Dispatcher;
#[cfg(not(target_vendor = "apple"))]
pub(crate) use non_apple::Dispatcher;
