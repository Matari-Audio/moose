//! Main-queue drains and the AU host sink.
//!
//! Control producers wake a per-instance `DATA_OR` source. Render producers
//! publish atomically and a timer shared by the framework image wakes the
//! dirty instances at approximately 30 Hz.

use crate::ffi::MooseAuHostSink;

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_vendor = "apple")]
use std::{ffi::c_void, sync::atomic::AtomicPtr};

static IMAGE_DIRTY: AtomicBool = AtomicBool::new(false);

#[cfg(not(target_vendor = "apple"))]
thread_local! {
    static IN_RENDER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(target_vendor = "apple")]
fn current_thread() -> *mut c_void {
    unsafe extern "C" {
        fn pthread_self() -> *mut c_void;
    }

    // SAFETY: pthread_self reads the calling thread's initialized pthread token.
    unsafe { pthread_self() }
}

/// Signals routed from this instance's exclusive audio owner to the image timer.
#[derive(Default)]
pub(crate) struct RenderSignals {
    dirty: AtomicBool,
    #[cfg(target_vendor = "apple")]
    thread: AtomicPtr<c_void>,
}

impl RenderSignals {
    pub(crate) fn enter(&self) -> RenderSignalGuard<'_> {
        #[cfg(target_vendor = "apple")]
        self.thread.store(current_thread(), Ordering::Release);
        #[cfg(not(target_vendor = "apple"))]
        let previous = IN_RENDER.with(|render| render.replace(true));
        RenderSignalGuard {
            signals: self,
            #[cfg(not(target_vendor = "apple"))]
            previous,
        }
    }

    pub(crate) fn defer(&self) -> bool {
        #[cfg(target_vendor = "apple")]
        let render = self.thread.load(Ordering::Acquire) == current_thread();
        #[cfg(not(target_vendor = "apple"))]
        let render = IN_RENDER.with(std::cell::Cell::get);
        if render {
            self.mark();
        }
        render
    }

    pub(crate) fn mark(&self) {
        self.dirty.store(true, Ordering::SeqCst);
        IMAGE_DIRTY.store(true, Ordering::SeqCst);
    }
}

pub(crate) struct RenderSignalGuard<'a> {
    signals: &'a RenderSignals,
    #[cfg(not(target_vendor = "apple"))]
    previous: bool,
}

impl Drop for RenderSignalGuard<'_> {
    fn drop(&mut self) {
        #[cfg(target_vendor = "apple")]
        self.signals
            .thread
            .store(std::ptr::null_mut(), Ordering::Release);
        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = self.signals;
            IN_RENDER.with(|render| render.set(self.previous));
        }
    }
}

// ---------------------------------------------------------------------------
// HostSink: value-copied vtable wrapper with single-run release on Drop
// ---------------------------------------------------------------------------

/// Owned copy of [`MooseAuHostSink`] stored as the **last** field of
/// `AuInstance<P>` so `release_user_data` runs once after `host_queue.close()`
/// and editor teardown complete.
pub(crate) struct HostSink(MooseAuHostSink);

impl HostSink {
    /// No-op sink used when the caller passes a null or undersized prefix.
    pub(crate) fn empty() -> Self {
        Self(MooseAuHostSink::default())
    }

    /// Copy only complete fields whose full extent fits inside `(*raw).size`
    /// (preventing partial pointer overwrites for truncated prefixes while
    /// remaining forward-compatible with larger future vtable revisions).
    ///
    /// # Safety
    /// `raw` is null or aligned with a readable size word and the first
    /// `min(size, size_of::<MooseAuHostSink>())` bytes initialized.
    pub(crate) unsafe fn from_raw(raw: *const MooseAuHostSink) -> Self {
        if raw.is_null() {
            return Self::empty();
        }
        // SAFETY: the caller guarantees the size word is readable even for
        // an undersized prefix.
        let declared = unsafe { raw.cast::<u32>().read() as usize };
        if declared < std::mem::size_of::<u32>() * 2 {
            return Self::empty();
        }
        let mut copy = MooseAuHostSink::default();
        macro_rules! copy_field {
            ($field:ident) => {{
                let end = std::mem::offset_of!(MooseAuHostSink, $field)
                    + std::mem::size_of_val(&copy.$field);
                if declared >= end {
                    // SAFETY: `declared >= end` proves this field lies entirely inside the caller's
                    // initialized prefix, and `addr_of!` reads it without forming a reference to
                    // the (possibly shorter) whole struct.
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
            // SAFETY: `f` and `user_data` come from the adapter's sink, which keeps `user_data`
            // valid until `release_user_data` runs in `Drop`; `&self` proves that has not happened
            // yet.
            unsafe { f(self.0.user_data, param_id) };
        }
    }

    pub(crate) unsafe fn set_param(&self, param_id: u32, plain_value: f32) {
        if let Some(f) = self.0.set_param {
            // SAFETY: Adapter-supplied callback and `user_data`, valid until `release_user_data`
            // runs in `Drop` (not yet, since `&self` is live).
            unsafe { f(self.0.user_data, param_id, plain_value) };
        }
    }

    pub(crate) unsafe fn end_edit(&self, param_id: u32) {
        if let Some(f) = self.0.end_edit {
            // SAFETY: Adapter-supplied callback and `user_data`, valid until `release_user_data`
            // runs in `Drop` (not yet, since `&self` is live).
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
            // SAFETY: Adapter-supplied callback and `user_data`, valid until `release_user_data`
            // runs in `Drop` (not yet, since `&self` is live).
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
            // SAFETY: Adapter-supplied callback and `user_data`, valid until `release_user_data`
            // runs in `Drop` (not yet, since `&self` is live).
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
            // SAFETY: Adapter-supplied callback and `user_data`, valid until `release_user_data`
            // runs in `Drop` (not yet, since `&self` is live).
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
            // SAFETY: Adapter-supplied callback and `user_data`, valid until `release_user_data`
            // runs in `Drop` (not yet, since `&self` is live).
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
            // SAFETY: Adapter-supplied callback and `user_data`, valid until `release_user_data`
            // runs in `Drop` (not yet, since `&self` is live).
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
            // SAFETY: Adapter-supplied callback and `user_data`, valid until `release_user_data`
            // runs in `Drop` (not yet, since `&self` is live).
            unsafe { f(self.0.user_data) };
        }
    }

    pub(crate) unsafe fn request_resize(&self, width: u32, height: u32) -> bool {
        if let Some(f) = self.0.request_resize {
            // SAFETY: Adapter-supplied callback and `user_data`, valid until `release_user_data`
            // runs in `Drop` (not yet, since `&self` is live).
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
            // SAFETY: `release` is called exactly once: it was `take()`n, and `user_data` was
            // nulled so no later call can reach it. `HostSink` is the last `AuInstance` field, so
            // every other user of `user_data` has already dropped.
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
    use super::{IMAGE_DIRTY, RenderSignals};
    use crate::instance::AuCtx;
    use moose_core::export::PluginExport;
    use moose_core::host_queue::HostQueue;
    use moose_core::wrapper::run_extern_callback_with;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    /// Owned `dispatch_source_t` handle safe to wake from any thread via
    /// `dispatch_source_merge_data` until `Dispatcher::drop` cancels and
    /// releases it after `HostQueue::close`.
    #[derive(Clone, Copy)]
    struct DispatchSource(*mut c_void);

    // SAFETY: libdispatch `dispatch_source_t` objects are thread-safe for
    // `dispatch_source_merge_data`, and cancellation/release is serialized by
    // `HostQueue::close` before `Dispatcher::drop`.
    unsafe impl Send for DispatchSource {}
    // SAFETY: Same invariant as `Send`: shared access only reaches `dispatch_source_merge_data`,
    // which libdispatch documents as thread-safe, and the pointer is never mutated.
    unsafe impl Sync for DispatchSource {}

    impl DispatchSource {
        #[inline]
        fn as_ptr(self) -> *mut c_void {
            self.0
        }
    }

    unsafe extern "C" {
        static _dispatch_source_type_data_or: c_void;
        static _dispatch_source_type_timer: c_void;
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
        fn dispatch_source_set_timer(source: *mut c_void, start: u64, interval: u64, leeway: u64);
        fn dispatch_resume(source: *mut c_void);
        fn dispatch_source_merge_data(source: *mut c_void, data: usize);
        fn dispatch_source_cancel(source: *mut c_void);
        fn dispatch_release(source: *mut c_void);
    }

    static TIMER: Mutex<RenderTimer> = Mutex::new(RenderTimer {
        source: None,
        reservations: 0,
        active: Vec::new(),
    });

    struct RenderTimer {
        source: Option<DispatchSource>,
        reservations: usize,
        active: Vec<(DispatchSource, Arc<RenderSignals>)>,
    }

    // Reserve a working source during fallible instantiation; the v14
    // activation callback has no way to report a timer allocation failure.
    struct TimerReservation;

    impl TimerReservation {
        fn new() -> Option<Self> {
            let mut timer = TIMER.lock().unwrap_or_else(PoisonError::into_inner);
            if timer.source.is_none() {
                // SAFETY: both the source type and queue are libdispatch globals.
                let raw = unsafe {
                    dispatch_source_create(
                        &raw const _dispatch_source_type_timer,
                        0,
                        0,
                        (&raw const _dispatch_main_q).cast_mut(),
                    )
                };
                if raw.is_null() {
                    return None;
                }
                // SAFETY: initialize and resume the newly created timer once.
                unsafe {
                    dispatch_source_set_event_handler_f(raw, timer_tick);
                    dispatch_source_set_timer(raw, u64::MAX, 0, 0);
                    dispatch_resume(raw);
                }
                timer.source = Some(DispatchSource(raw));
            }
            timer.reservations += 1;
            Some(Self)
        }
    }

    impl Drop for TimerReservation {
        fn drop(&mut self) {
            let mut timer = TIMER.lock().unwrap_or_else(PoisonError::into_inner);
            timer.reservations -= 1;
            if timer.reservations == 0
                && let Some(source) = timer.source.take()
            {
                // The last dispatcher has already removed its registration.
                // SAFETY: `reservations` reached zero under `TIMER`, so no `Dispatcher` is left to
                // set or tick this timer; `active` is empty because each dispatcher unregisters in
                // `Drop` before releasing its reservation. Cancel then release is the libdispatch
                // teardown order for a resumed source.
                unsafe {
                    dispatch_source_cancel(source.0);
                    dispatch_release(source.0);
                }
            }
        }
    }

    unsafe extern "C" fn timer_tick(_: *mut c_void) {
        if !IMAGE_DIRTY.swap(false, Ordering::SeqCst) {
            return;
        }
        let timer = TIMER.lock().unwrap_or_else(PoisonError::into_inner);
        for (source, signals) in &timer.active {
            if signals.dirty.swap(false, Ordering::SeqCst) {
                // SAFETY: registrations keep the source live under TIMER.
                unsafe { dispatch_source_merge_data(source.0, 1) };
            }
        }
    }

    struct DrainContext {
        queue: Arc<HostQueue>,
        ctx: AuCtx,
    }

    pub(crate) struct Dispatcher {
        source: DispatchSource,
        activated: AtomicBool,
        _timer: TimerReservation,
    }

    impl Dispatcher {
        /// Reserve the render timer and create a suspended main-queue
        /// `DATA_OR` source with a thread-safe, allocation-free wake closure.
        pub(crate) unsafe fn new() -> Option<(Self, impl Fn() + Send + Sync + 'static)> {
            let timer = TimerReservation::new()?;
            // SAFETY: `_dispatch_source_type_data_or` and `_dispatch_main_q` are libdispatch
            // globals valid for the process lifetime. A null result is handled, and the new source
            // stays suspended until `activate`, so `wake` only merges data into a live source
            // (`HostQueue` runs it only while open, and the source is released after `close`).
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
                        _timer: timer,
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
            // SAFETY: `raw` is this dispatcher's suspended source, configured once before
            // `dispatch_resume`. `drain_ctx` is a leaked box that `release_context` frees exactly
            // once when the source is cancelled in `Drop`.
            unsafe {
                dispatch_set_context(raw, drain_ctx.cast());
                dispatch_source_set_event_handler_f(raw, drain::<P>);
                dispatch_source_set_cancel_handler_f(raw, release_context);
                dispatch_resume(raw);
            }
            self.activated.store(true, Ordering::Release);
        }

        pub(crate) fn start_render_polling(&self, signals: &Arc<RenderSignals>) {
            let mut timer = TIMER.lock().unwrap_or_else(PoisonError::into_inner);
            if timer.active.is_empty()
                && let Some(source) = timer.source
            {
                // The dispatcher's reservation keeps the resumed timer live.
                // SAFETY: `source` is the shared timer, kept alive by this dispatcher's
                // `TimerReservation`, and `TIMER` is locked.
                unsafe { dispatch_source_set_timer(source.0, 0, 33_333_333, 3_000_000) };
            }
            timer.active.push((self.source, Arc::clone(signals)));
            if signals.dirty.load(Ordering::SeqCst) {
                IMAGE_DIRTY.store(true, Ordering::SeqCst);
            }
        }

        pub(crate) fn stop_render_polling(&self) {
            let mut timer = TIMER.lock().unwrap_or_else(PoisonError::into_inner);
            timer.active.retain(|(source, _)| source.0 != self.source.0);
            if timer.active.is_empty()
                && let Some(source) = timer.source
            {
                // DISPATCH_TIME_FOREVER keeps the reserved source idle.
                // SAFETY: `source` is the shared timer, kept alive by this dispatcher's
                // `TimerReservation`, and `TIMER` is locked.
                unsafe { dispatch_source_set_timer(source.0, u64::MAX, 0, 0) };
            }
        }

        /// Coalesced event-driven wake of the main-queue drain (safe from any
        /// thread, allocation-free).
        pub(crate) fn trigger_now(&self) {
            if self.activated.load(Ordering::Acquire) {
                // SAFETY: `self.source` stays live until `Drop`, and merging data is thread-safe in
                // libdispatch; the `activated` check ensures the source has been resumed and has a
                // context.
                unsafe {
                    dispatch_source_merge_data(self.source.0, 1);
                }
            }
        }
    }

    impl Drop for Dispatcher {
        fn drop(&mut self) {
            self.stop_render_polling();
            let raw = self.source.0;
            // SAFETY: Last use of this source: it was unregistered from the timer above. A source
            // that was never resumed must be resumed before its final release (libdispatch traps on
            // releasing a suspended source), and cancellation runs `release_context` for an
            // activated one.
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
        // SAFETY: `raw` is the `DrainContext` set in `activate`. libdispatch never runs the event
        // handler after the cancel handler frees it, and `run_host_drain` only dereferences the
        // instance inside `HostQueue::with_open`, which fails once `finalize_destroy` closed the
        // queue.
        run_extern_callback_with::<P, ()>("au", "host_queue_drain", (), || unsafe {
            let context = &*raw.cast::<DrainContext>();
            super::run_host_drain::<P>(&context.queue, context.ctx.as_ptr());
        });
    }

    unsafe extern "C" fn release_context(raw: *mut c_void) {
        if !raw.is_null() {
            // SAFETY: `raw` is the `Box<DrainContext>` leaked in `activate`; libdispatch calls the
            // cancel handler exactly once.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                drop(Box::from_raw(raw.cast::<DrainContext>()));
            }));
        }
    }
}

#[cfg(not(target_vendor = "apple"))]
mod non_apple {
    use super::RenderSignals;
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

        #[allow(clippy::unused_self)]
        pub(crate) fn start_render_polling(&self, _signals: &Arc<RenderSignals>) {}

        #[allow(clippy::unused_self)]
        pub(crate) fn stop_render_polling(&self) {}
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
    // SAFETY: Forwarded contract: the caller guarantees `ctx` is the live `AuInstance<P>` that owns
    // `queue`, and `with_host_open` only runs the drain while the queue is open.
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
