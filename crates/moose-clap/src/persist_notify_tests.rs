//! Actual CLAP main-thread completion delivery with no editor created.
use super::host_value_fixture::TestPlugin;
use super::*;
#[derive(Default)]
struct HostCalls {
    requests: AtomicU32,
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
        params.completion.complete(5); // completed identical bytes
        clap_plugin_on_main_thread::<TestPlugin>(plugin);
        assert!(data.gui_changes.pop().is_none());
        let signal = params.completion.signal().unwrap();
        clap_plugin_destroy::<TestPlugin>(plugin);
        let requests = calls.requests.load(Ordering::Relaxed);
        params.completion.complete(6);
        signal.notify(); // even a worker-retained signal is closed
        assert_eq!(calls.requests.load(Ordering::Relaxed), requests);
        clap_plugin_destroy::<TestPlugin>(other);
    }
}
