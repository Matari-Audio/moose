//! Actual CLAP main-thread completion delivery with no editor created.
use super::host_value_fixture::TestPlugin;
use super::*;
struct HostCalls {
    requests: AtomicU32,
    dirty: AtomicU32,
    main: std::thread::ThreadId,
}
impl Default for HostCalls {
    fn default() -> Self {
        Self {
            requests: AtomicU32::new(0),
            dirty: AtomicU32::new(0),
            main: std::thread::current().id(),
        }
    }
}
unsafe extern "C" fn mark_dirty(host: *const clap_host) {
    let calls = unsafe { &*((*host).host_data.cast::<HostCalls>()) };
    assert_eq!(std::thread::current().id(), calls.main);
    calls.dirty.fetch_add(1, Ordering::Relaxed);
}
unsafe extern "C" fn extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    static STATE: clap_host_state = clap_host_state {
        mark_dirty: Some(mark_dirty),
    };
    if unsafe { CStr::from_ptr(id) } == CLAP_EXT_STATE {
        (&raw const STATE).cast()
    } else {
        std::ptr::null()
    } // deliberately no params extension or flush
}
unsafe extern "C" fn request(host: *const clap_host) {
    // SAFETY: the test retains this host and its counter until destroy returns.
    unsafe { &*((*host).host_data.cast::<HostCalls>()) }
        .requests
        .fetch_add(1, Ordering::Relaxed);
}
#[test]
fn closed_editor_completion_is_main_thread_coalesced_noop_safe_and_teardown_safe() {
    // SAFETY: own both plugins, descriptors and host handles through teardown.
    unsafe {
        let calls = HostCalls::default();
        let mut host: clap_host = std::mem::zeroed();
        host.clap_version = clap_sys::version::CLAP_VERSION;
        host.host_data = (&raw const calls).cast_mut().cast();
        host.request_callback = Some(request);
        host.get_extension = Some(extension);
        let descriptor = DescriptorHolder::new(&TestPlugin::info());
        let plugin =
            create_plugin_instance::<TestPlugin>(&raw const descriptor.descriptor, &raw const host);
        let other =
            create_plugin_instance::<TestPlugin>(&raw const descriptor.descriptor, &raw const host);
        assert!(clap_plugin_init::<TestPlugin>(plugin));
        assert!(clap_plugin_init::<TestPlugin>(other));
        let data = data_from_plugin::<TestPlugin>(plugin);
        let params = data.params_arc.clone();
        let other_params = data_from_plugin::<TestPlugin>(other).params_arc.clone();
        assert!(data.gui.enter().editor.is_none());
        assert!(!data.gui.enter().gui_created);
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    params.completion.complete(5);
                    params.completion.complete(5);
                })
                .join()
                .unwrap();
        });
        assert_eq!(calls.requests.load(Ordering::Relaxed), 1);
        assert_eq!(
            params.get_normalized(1),
            Some(0.5),
            "worker did not edit host parameters"
        );
        clap_plugin_on_main_thread::<TestPlugin>(plugin);
        assert!(matches!(
            data.gui_changes.pop(),
            Some(GuiParamChange::GestureBegin(1))
        ));
        assert!(matches!(
            data.gui_changes.pop(),
            Some(GuiParamChange::Value(1, 0.25))
        ));
        assert!(matches!(
            data.gui_changes.pop(),
            Some(GuiParamChange::GestureEnd(1))
        ));
        assert!(data.gui_changes.pop().is_none());
        assert_eq!(params.get_normalized(1), Some(0.25));
        assert_eq!(other_params.get_normalized(1), Some(0.5));
        assert_eq!(calls.dirty.load(Ordering::Relaxed), 1);
        params.completion.complete(5); // completed identical bytes
        clap_plugin_on_main_thread::<TestPlugin>(plugin);
        assert!(data.gui_changes.pop().is_none());
        assert_eq!(
            calls.dirty.load(Ordering::Relaxed),
            1,
            "no dirty call for identical bytes"
        );
        let signal = params.completion.signal().unwrap();
        clap_plugin_destroy::<TestPlugin>(plugin);
        let requests = calls.requests.load(Ordering::Relaxed);
        params.completion.complete(6);
        signal.notify(); // even a worker-retained signal is closed
        assert_eq!(calls.requests.load(Ordering::Relaxed), requests);
        clap_plugin_destroy::<TestPlugin>(other);
    }
}
