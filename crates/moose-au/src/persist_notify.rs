//! AUv2 persistent-document completion on the host main queue, without a GUI
//! or a polling worker. Mirrors the VST3 main-queue dispatch-source bridge.
use super::*;
use std::ffi::c_void;

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
    fn dispatch_resume(source: *mut c_void);
    fn dispatch_source_merge_data(source: *mut c_void, data: usize);
    fn dispatch_source_cancel(source: *mut c_void);
    fn dispatch_release(source: *mut c_void);
}
struct Context<P: PluginExport> {
    params: Arc<P::Params>,
    host: SendPtr<c_void>,
}
pub(super) struct Dispatcher<P: PluginExport> {
    signal: Arc<moose_params::PersistChangeSignal>,
    source: SendPtr<c_void>,
    _context: Box<Context<P>>,
}
impl<P: PluginExport> Dispatcher<P> {
    /// Host main thread only, before publishing the instance to the host.
    pub(super) unsafe fn new(params: Arc<P::Params>, host: SendPtr<c_void>) -> Option<Self> {
        if !params.wants_persist_notifications() {
            return None;
        }
        // SAFETY: libdispatch's exported opaque type and queue objects have
        // static process lifetime; the source is resumed once and released by
        // Drop on the same main thread as AU lifecycle/editor callbacks.
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
            let mut context = Box::new(Context::<P> {
                params: params.clone(),
                host,
            });
            dispatch_set_context(raw, (&raw mut *context).cast());
            dispatch_source_set_event_handler_f(raw, drain::<P>);
            dispatch_resume(raw);
            let signal = Arc::new(moose_params::PersistChangeSignal::new(move || {
                dispatch_source_merge_data(source.as_ptr().cast_mut(), 1);
            }));
            params.set_persist_change_signal(Arc::downgrade(&signal));
            Some(Self {
                signal,
                source,
                _context: context,
            })
        }
    }
}
impl<P: PluginExport> Drop for Dispatcher<P> {
    fn drop(&mut self) {
        self.signal.close(); // joins an in-flight worker wake before release
        // SAFETY: AU control callbacks and this source's handler are serialized
        // on the host main thread. No handler is executing concurrently here;
        // cancellation prevents subsequent event-handler calls. No cancel
        // handler is installed, so no plugin code is queued after teardown.
        unsafe {
            dispatch_source_cancel(self.source.as_ptr().cast_mut());
            dispatch_release(self.source.as_ptr().cast_mut());
        }
    }
}
unsafe extern "C" fn drain<P: PluginExport>(raw: *mut c_void) {
    run_extern_callback_with::<P, ()>("au", "persist_drain", (), || unsafe {
        let context = &*raw.cast::<Context<P>>();
        let Some((id, value)) = context.params.take_persist_change() else {
            return;
        };
        if !value.is_finite()
            || !(0.0..=1.0).contains(&value)
            || context.params.get_normalized(id).is_none()
        {
            return;
        }
        let plain = context.params.set_normalized_returning_plain(id, value);
        let host = context.host.as_ptr().cast_mut();
        moose_au_v2_host_begin_param_gesture(host, id);
        // AU wire values are f32, as in the existing editor bridge.
        #[allow(clippy::cast_possible_truncation)]
        moose_au_v2_host_set_param(host, id, plain as f32);
        moose_au_v2_host_end_param_gesture(host, id);
    });
}
