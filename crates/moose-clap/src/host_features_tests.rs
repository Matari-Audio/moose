//! The runtime catalog and the main-thread extensions through the real CLAP
//! callbacks, against a recording host and recording `MainThreadHooks`.
// The asserted values (0.0, 0.5, 1.0, 100.0) are exact in f32 and f64.
#![allow(clippy::float_cmp)]

use super::*;
use clap_sys::color::clap_color;
use clap_sys::ext::context_menu::{
    CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY, CLAP_CONTEXT_MENU_ITEM_ENTRY,
    CLAP_CONTEXT_MENU_ITEM_SEPARATOR, CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL,
    CLAP_CONTEXT_MENU_TARGET_KIND_PARAM, clap_context_menu_builder, clap_context_menu_check_entry,
    clap_context_menu_entry, clap_context_menu_item_kind, clap_context_menu_target,
    clap_host_context_menu,
};
use clap_sys::ext::note_name::clap_host_note_name;
use clap_sys::ext::note_name::clap_note_name;
use clap_sys::ext::param_indication::CLAP_PARAM_INDICATION_AUTOMATION_RECORDING;
use clap_sys::ext::params::CLAP_PARAM_CLEAR_ALL;
use clap_sys::ext::track_info::{
    CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL, CLAP_TRACK_INFO_HAS_TRACK_COLOR,
    CLAP_TRACK_INFO_HAS_TRACK_NAME, CLAP_TRACK_INFO_IS_FOR_BUS, clap_track_info,
};
use clap_sys::ext::voice_info::clap_host_voice_info;
use clap_sys::ext::voice_info::{CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES, clap_voice_info};
use moose_core::main_thread::{
    AutomationState, HostColor, HostMapping, HostNotice, HostNotifier, MenuItem, MenuTarget,
    ParamIndication, TrackInfo, TrackKind, VoiceInfo,
};
use moose_core::process::ProcessContext;
use moose_params::{DynamicParams, FloatParam, SlotDef, SlotRange};
use std::cell::RefCell;

#[derive(moose_derive::Params)]
#[params(id_scheme = "ordinal")]
struct Modular {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = 0.5)]
    gain: FloatParam,
    #[nested(base = 1)]
    slots: DynamicParams<4>,
}

/// What the hooks saw, per test thread.
#[derive(Debug, PartialEq)]
enum Seen {
    Track(TrackInfo),
    Indication(u32, ParamIndication),
    Perform(MenuTarget, u32),
}

thread_local! {
    static CAN_POPUP: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    static NOTIFIER: RefCell<Option<HostNotifier>> = const { RefCell::new(None) };
}

fn seen() -> Vec<Seen> {
    SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

struct Recorder;

impl MainThreadHooks for Recorder {
    fn connect(&mut self, notifier: HostNotifier) {
        NOTIFIER.with(|n| *n.borrow_mut() = Some(notifier));
    }
    fn track_info_changed(&mut self, _: &MainThread, info: &TrackInfo) {
        SEEN.with(|s| s.borrow_mut().push(Seen::Track(info.clone())));
    }
    fn note_names(&self, _: &MainThread) -> Vec<NoteName> {
        vec![
            NoteName {
                name: "Kick".into(),
                port: None,
                key: Some(36),
                channel: None,
            },
            NoteName {
                name: "Snare".into(),
                port: Some(0),
                key: Some(38),
                channel: Some(9),
            },
        ]
    }
    fn voice_info(&self, _: &MainThread) -> Option<VoiceInfo> {
        Some(VoiceInfo {
            count: 4,
            capacity: 16,
            overlapping_notes: true,
        })
    }
    fn param_indication(&mut self, _: &MainThread, id: u32, indication: ParamIndication) {
        SEEN.with(|s| s.borrow_mut().push(Seen::Indication(id, indication)));
    }
    fn menu_items(&self, _: &MainThread, target: MenuTarget) -> Vec<MenuItem> {
        let mut items = vec![MenuItem::Entry {
            label: "Reset".into(),
            action: 1,
            enabled: true,
        }];
        if let MenuTarget::Param(_) = target {
            items.push(MenuItem::Separator);
            items.push(MenuItem::Check {
                label: "Learn".into(),
                action: 2,
                enabled: true,
                checked: false,
            });
        }
        items
    }
    fn perform_menu(&mut self, _: &MainThread, target: MenuTarget, action: u32) -> bool {
        SEEN.with(|s| s.borrow_mut().push(Seen::Perform(target, action)));
        action <= 2
    }
}

struct ModularPlugin {
    params: Arc<Modular>,
    meters: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
}

impl PluginExport for ModularPlugin {
    type Params = Modular;
    fn create() -> Self {
        Self {
            params: Arc::new(Modular::new()),
            meters: MeterStore::new(),
            snapshot: SnapshotSlot::new(),
        }
    }
    fn main_thread(&self) -> Box<dyn MainThreadHooks> {
        Box::new(Recorder)
    }
    fn params(&self) -> &Modular {
        &self.params
    }
    fn params_arc(&self) -> Arc<Modular> {
        Arc::clone(&self.params)
    }
    fn meter_store(&self) -> Arc<MeterStore> {
        Arc::clone(&self.meters)
    }
    fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
        Arc::clone(&self.snapshot)
    }
}

impl PluginRuntime for ModularPlugin {
    type Sample = f32;
    fn info() -> PluginInfo {
        super::host_value_fixture::TestPlugin::info()
    }
    fn reset(&mut self, _: &AudioConfig) {}
    fn process(
        &mut self,
        _: &mut AudioBuffer,
        _: &EventList,
        _: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }
}

/// Everything the fake host was asked.
#[derive(Default)]
struct HostLog {
    callbacks: AtomicU32,
    restarts: AtomicU32,
    rescans: Mutex<Vec<u32>>,
    clears: Mutex<Vec<(u32, u32)>>,
    note_names_changed: AtomicU32,
    voice_info_changed: AtomicU32,
    port_rescans: Mutex<Vec<u32>>,
    popups: Mutex<Vec<(u32, clap_id, i32, i32)>>,
}

impl HostLog {
    fn rescans(&self) -> Vec<u32> {
        std::mem::take(&mut *self.rescans.lock().expect("rescans"))
    }
    fn clears(&self) -> Vec<(u32, u32)> {
        std::mem::take(&mut *self.clears.lock().expect("clears"))
    }
}

/// # Safety
/// `host` is the test host whose `host_data` points to a live `HostLog`.
unsafe fn log<'a>(host: *const clap_host) -> &'a HostLog {
    // SAFETY: caller contract.
    unsafe { &*(*host).host_data.cast::<HostLog>() }
}

unsafe extern "C" fn request_callback(host: *const clap_host) {
    // SAFETY: called by the plugin with the test host.
    unsafe { log(host) }
        .callbacks
        .fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn request_restart(host: *const clap_host) {
    // SAFETY: called by the plugin with the test host.
    unsafe { log(host) }
        .restarts
        .fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn rescan(host: *const clap_host, flags: u32) {
    // SAFETY: called by the plugin with the test host.
    let log = unsafe { log(host) };
    log.rescans.lock().expect("rescans").push(flags);
}

unsafe extern "C" fn clear(host: *const clap_host, id: clap_id, flags: u32) {
    // SAFETY: called by the plugin with the test host.
    let log = unsafe { log(host) };
    log.clears.lock().expect("clears").push((id, flags));
}

unsafe extern "C" fn note_names_changed(host: *const clap_host) {
    // SAFETY: called by the plugin with the test host.
    unsafe { log(host) }
        .note_names_changed
        .fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn voice_info_changed(host: *const clap_host) {
    // SAFETY: called by the plugin with the test host.
    unsafe { log(host) }
        .voice_info_changed
        .fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn audio_ports_rescan(host: *const clap_host, flags: u32) {
    // SAFETY: called by the plugin with the test host.
    let log = unsafe { log(host) };
    log.port_rescans.lock().expect("port rescans").push(flags);
}

unsafe extern "C" fn menu_can_popup(_: *const clap_host) -> bool {
    CAN_POPUP.with(std::cell::Cell::get)
}

unsafe extern "C" fn menu_popup(
    host: *const clap_host,
    target: *const clap_context_menu_target,
    _screen: i32,
    x: i32,
    y: i32,
) -> bool {
    // SAFETY: called by the plugin with the test host and a live target.
    let (log, target) = unsafe { (log(host), &*target) };
    log.popups
        .lock()
        .expect("popups")
        .push((target.kind, target.id, x, y));
    true
}

unsafe extern "C" fn track_get(_: *const clap_host, info: *mut clap_track_info) -> bool {
    // SAFETY: the plugin passes a writable info.
    let info = unsafe { &mut *info };
    info.flags = CLAP_TRACK_INFO_HAS_TRACK_NAME
        | CLAP_TRACK_INFO_HAS_TRACK_COLOR
        | CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL
        | CLAP_TRACK_INFO_IS_FOR_BUS;
    copy_str_to_buf(&mut info.name, "Drums");
    info.color = clap_color {
        alpha: 255,
        red: 10,
        green: 20,
        blue: 30,
    };
    info.audio_channel_count = 2;
    true
}

unsafe extern "C" fn host_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    static PARAMS: clap_host_params = clap_host_params {
        rescan: Some(rescan),
        clear: Some(clear),
        request_flush: None,
    };
    static TRACK: clap_host_track_info = clap_host_track_info {
        get: Some(track_get),
    };
    static NOTE_NAME: clap_host_note_name = clap_host_note_name {
        changed: Some(note_names_changed),
    };
    static VOICE_INFO: clap_host_voice_info = clap_host_voice_info {
        changed: Some(voice_info_changed),
    };
    static AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
        is_rescan_flag_supported: None,
        rescan: Some(audio_ports_rescan),
    };
    static MENU: clap_host_context_menu = clap_host_context_menu {
        populate: None,
        perform: None,
        can_popup: Some(menu_can_popup),
        popup: Some(menu_popup),
    };
    // SAFETY: the plugin passes a NUL-terminated id.
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_CONTEXT_MENU {
        (&raw const MENU).cast()
    } else if id == CLAP_EXT_PARAMS {
        (&raw const PARAMS).cast()
    } else if id == CLAP_EXT_TRACK_INFO {
        (&raw const TRACK).cast()
    } else if id == CLAP_EXT_NOTE_NAME {
        (&raw const NOTE_NAME).cast()
    } else if id == CLAP_EXT_VOICE_INFO {
        (&raw const VOICE_INFO).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        (&raw const AUDIO_PORTS).cast()
    } else {
        ptr::null()
    }
}

type Plugin = ModularPlugin;

/// Run `test` on an initialized instance with the recording host.
fn with_modular(test: impl FnOnce(*const clap_plugin, &HostLog, &Modular)) {
    let log = HostLog::default();
    // SAFETY: host, log and descriptor outlive the serial calls and destroy.
    unsafe {
        let mut host: clap_host = std::mem::zeroed();
        host.clap_version = clap_sys::version::CLAP_VERSION;
        host.host_data = (&raw const log).cast_mut().cast();
        host.get_extension = Some(host_extension);
        host.request_callback = Some(request_callback);
        host.request_restart = Some(request_restart);
        let descriptor = DescriptorHolder::new(&Plugin::info());
        let plugin =
            create_plugin_instance::<Plugin>(&raw const descriptor.descriptor, &raw const host);
        assert!(!plugin.is_null());
        assert!((*plugin).init.unwrap()(plugin));
        let params = Arc::clone(&data_from_plugin::<Plugin>(plugin).params_arc);
        test(plugin, &log, &params);
        (*plugin).destroy.unwrap()(plugin);
    }
}

fn linear(max: f64) -> SlotDef {
    SlotDef::new("Cutoff", SlotRange::Linear { min: 0.0, max }).with_default(max / 2.0)
}

/// # Safety
/// `plugin` is a live instance of `Plugin`.
unsafe fn count(plugin: *const clap_plugin) -> u32 {
    // SAFETY: caller contract.
    unsafe { params_count::<Plugin>(plugin) }
}

/// The host-visible info for `id`, if it is exposed.
///
/// # Safety
/// `plugin` is a live instance of `Plugin`.
unsafe fn info_for(plugin: *const clap_plugin, id: u32) -> Option<clap_param_info> {
    // SAFETY: caller contract; `info` is writable.
    unsafe {
        (0..count(plugin)).find_map(|i| {
            let mut info: clap_param_info = std::mem::zeroed();
            (params_get_info::<Plugin>(plugin, i, &raw mut info) && info.id == id).then_some(info)
        })
    }
}

#[test]
fn catalog_wakes_coalesce_and_publish_structure_at_once_while_inactive() {
    with_modular(|plugin, log, params| unsafe {
        assert_eq!(count(plugin), 1, "free slots are not listed");
        let slot = params.slots.slot(0).expect("slot 0");
        let id = params.slots.id(slot);
        params.slots.edit(|tx| tx.bind(slot, linear(1.0)));
        params.slots.edit(|tx| tx.rename(slot, "Freq"));
        assert_eq!(log.callbacks.load(Ordering::Relaxed), 1, "one callback");

        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(log.restarts.load(Ordering::Relaxed), 0);
        let rescans = log.rescans();
        assert_eq!(rescans.len(), 1);
        assert_ne!(rescans[0] & CLAP_PARAM_RESCAN_ALL, 0, "{rescans:?}");
        assert_ne!(rescans[0] & CLAP_PARAM_RESCAN_INFO, 0, "{rescans:?}");
        assert_eq!(log.clears(), [], "nothing was bound before");
        assert_eq!(count(plugin), 2);
        assert_eq!(info_for(plugin, id).expect("bound slot").max_value, 1.0);

        // The wake flag is cleared, so the next change wakes again.
        params.slots.edit(|tx| tx.rename(slot, "Pitch"));
        assert_eq!(log.callbacks.load(Ordering::Relaxed), 2);
        (*plugin).on_main_thread.unwrap()(plugin);
        let rescans = log.rescans();
        assert_eq!(
            rescans,
            [CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_TEXT],
            "a rename is info only"
        );
    });
}

#[test]
fn structure_while_active_restarts_then_publishes_at_deactivate() {
    with_modular(|plugin, log, params| unsafe {
        let slot = params.slots.slot(1).expect("slot 1");
        let id = params.slots.id(slot);
        params.slots.edit(|tx| tx.bind(slot, linear(1.0)));
        (*plugin).on_main_thread.unwrap()(plugin);
        let _ = log.rescans();

        assert!((*plugin).activate.unwrap()(plugin, 48_000.0, 1, 256));
        params.slots.edit(|tx| tx.bind(slot, linear(100.0)));
        (*plugin).on_main_thread.unwrap()(plugin);
        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(log.restarts.load(Ordering::Relaxed), 1, "one restart");
        assert!(
            log.rescans()
                .iter()
                .all(|flags| flags & CLAP_PARAM_RESCAN_ALL == 0),
            "no RESCAN_ALL while active"
        );
        assert_eq!(log.clears(), []);
        let data = data_from_plugin::<Plugin>(plugin);
        assert_eq!(info_for(plugin, id).expect("listed").max_value, 1.0);

        (*plugin).deactivate.unwrap()(plugin);
        assert_eq!(log.clears(), [(id, CLAP_PARAM_CLEAR_ALL)]);
        let rescans = log.rescans();
        assert_ne!(
            rescans.last().copied().unwrap_or(0) & CLAP_PARAM_RESCAN_ALL,
            0,
            "{rescans:?}"
        );
        assert_eq!(info_for(plugin, id).expect("listed").max_value, 100.0);
        let audio_max = data
            .audio
            .enter()
            .catalog
            .by_id(id)
            .expect("in catalog")
            .range
            .max();
        assert_eq!(audio_max, 100.0, "the audio catalog was swapped too");

        // Freeing the slot while inactive publishes at once.
        params.slots.edit(|tx| tx.unbind(slot));
        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(log.clears(), [(id, CLAP_PARAM_CLEAR_ALL)]);
        assert!(info_for(plugin, id).is_none());
        assert_eq!(log.restarts.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn track_info_reaches_the_hooks_at_init_and_on_change() {
    with_modular(|plugin, _, _| unsafe {
        let mut want = TrackInfo::new();
        want.name = Some("Drums".into());
        want.color = Some(HostColor {
            r: 10,
            g: 20,
            b: 30,
            a: 255,
        });
        want.audio_channels = Some(2);
        want.kind = TrackKind::Bus;
        assert_eq!(seen(), [Seen::Track(want.clone())], "read at init");
        let ext = &*(*plugin).get_extension.unwrap()(plugin, CLAP_EXT_TRACK_INFO.as_ptr())
            .cast::<clap_plugin_track_info>();
        ext.changed.unwrap()(plugin);
        assert_eq!(seen(), [Seen::Track(want)]);
    });
}

#[test]
fn note_names_and_voice_info_come_from_the_hooks() {
    with_modular(|plugin, _, _| unsafe {
        let get = (*plugin).get_extension.unwrap();
        let names = &*get(plugin, CLAP_EXT_NOTE_NAME.as_ptr()).cast::<clap_plugin_note_name>();
        assert_eq!(names.count.unwrap()(plugin), 2);
        let mut name: clap_note_name = std::mem::zeroed();
        assert!(names.get.unwrap()(plugin, 1, &raw mut name));
        assert_eq!(
            CStr::from_ptr(name.name.as_ptr()).to_str().expect("utf8"),
            "Snare"
        );
        assert_eq!((name.port, name.key, name.channel), (0, 38, 9));
        assert!(names.get.unwrap()(plugin, 0, &raw mut name));
        assert_eq!((name.port, name.key, name.channel), (-1, 36, -1));
        assert!(!names.get.unwrap()(plugin, 2, &raw mut name));

        let voices = get(plugin, CLAP_EXT_VOICE_INFO.as_ptr()).cast::<clap_plugin_voice_info>();
        assert!(!voices.is_null(), "offered: the hooks report voice info");
        let mut info: clap_voice_info = std::mem::zeroed();
        assert!((*voices).get.unwrap()(plugin, &raw mut info));
        assert_eq!((info.voice_count, info.voice_capacity), (4, 16));
        assert_eq!(info.flags, CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES);
    });
}

#[test]
fn host_notices_reach_the_host_on_the_main_thread() {
    with_modular(|plugin, log, _| unsafe {
        let notifier = NOTIFIER
            .with(|n| n.borrow().clone())
            .expect("connected at init");
        let callbacks = log.callbacks.load(Ordering::Relaxed);
        notifier.notify(HostNotice::NOTE_NAMES);
        notifier.notify(HostNotice::VOICE_INFO);
        assert_eq!(
            log.callbacks.load(Ordering::Relaxed),
            callbacks + 1,
            "one main-thread callback for both"
        );
        assert_eq!(log.note_names_changed.load(Ordering::Relaxed), 0);

        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(log.note_names_changed.load(Ordering::Relaxed), 1);
        assert_eq!(log.voice_info_changed.load(Ordering::Relaxed), 1);

        // The editor's context sends through the same notifier.
        editor_context::<Plugin>(plugin).notify_host(HostNotice::VOICE_INFO);
        assert_eq!(log.callbacks.load(Ordering::Relaxed), callbacks + 2);
        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(log.note_names_changed.load(Ordering::Relaxed), 1);
        assert_eq!(log.voice_info_changed.load(Ordering::Relaxed), 2);
    });
}

#[cfg(target_os = "linux")]
#[test]
fn embedded_window_creation_refreshes_menu_capability() {
    use moose_core::editor::{Editor, PluginContext, RawWindowHandle};

    struct WindowEditor(bool);
    impl Editor for WindowEditor {
        fn size(&self) -> (u32, u32) {
            (100, 100)
        }
        fn open(&mut self, _: RawWindowHandle, _: PluginContext) {
            self.0 = true;
            CAN_POPUP.with(|value| value.set(true));
        }
        fn is_open(&self) -> bool {
            self.0
        }
        fn close(&mut self) {}
    }

    with_modular(|plugin, log, _| unsafe {
        let data = crate::data_from_plugin::<Plugin>(plugin);
        data.gui.enter().editor = Some(Box::new(WindowEditor(false)));
        CAN_POPUP.with(|value| value.set(false));
        assert!(gui_create::<Plugin>(
            plugin,
            CLAP_WINDOW_API_X11.as_ptr(),
            false
        ));
        let context = editor_context::<Plugin>(plugin);
        assert!(!context.context_menu(MenuTarget::Global, 1, 2));
        let mut window: clap_window = std::mem::zeroed();
        window.api = CLAP_WINDOW_API_X11.as_ptr();
        window.specific.ptr = ptr::dangling_mut::<c_void>();
        assert!(gui_set_parent::<Plugin>(plugin, &raw const window));
        assert!(context.context_menu(MenuTarget::Global, 1, 2));
        assert!(log.popups.lock().unwrap().is_empty());
        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(log.popups.lock().unwrap().len(), 1);
    });
}

#[test]
fn menu_falls_back_for_unsupported_and_floating_windows() {
    with_modular(|plugin, log, _| unsafe {
        let data = crate::data_from_plugin::<Plugin>(plugin);
        let context = editor_context::<Plugin>(plugin);
        CAN_POPUP.with(|value| value.set(false));
        crate::context_menu::refresh_capability(data, &moose_core::thread::MainThread::assume());
        assert!(!context.context_menu(MenuTarget::Global, 1, 2));
        CAN_POPUP.with(|value| value.set(true));
        crate::context_menu::refresh_capability(data, &moose_core::thread::MainThread::assume());
        data.floating_mode.store(true, Ordering::Release);
        assert!(!context.context_menu(MenuTarget::Global, 1, 2));
        assert!(log.popups.lock().unwrap().is_empty());
        data.floating_mode.store(false, Ordering::Release);
        assert!(context.context_menu(MenuTarget::Global, 1, 2));
    });
}

#[test]
fn editor_host_requests_are_made_on_the_main_thread_in_one_callback() {
    with_modular(|plugin, log, _| unsafe {
        let context = editor_context::<Plugin>(plugin);
        let callbacks = log.callbacks.load(Ordering::Relaxed);
        let restarts = log.restarts.load(Ordering::Relaxed);
        assert!(context.context_menu(MenuTarget::Param(3), 10, 20));
        assert!(context.context_menu(MenuTarget::Global, 1, 2));
        context.request_restart();
        context.mark_dirty();
        assert_eq!(log.callbacks.load(Ordering::Relaxed), callbacks + 1);
        assert!(
            log.popups.lock().expect("popups").is_empty(),
            "not off the main thread"
        );
        assert_eq!(log.restarts.load(Ordering::Relaxed), restarts);

        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(
            *log.popups.lock().expect("popups"),
            [
                (CLAP_CONTEXT_MENU_TARGET_KIND_PARAM, 3, 10, 20),
                (CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL, 0, 1, 2),
            ]
        );
        assert_eq!(log.restarts.load(Ordering::Relaxed), restarts + 1);
    });
}

#[test]
fn port_notice_restarts_an_active_plugin_and_rescans_once_inactive() {
    with_modular(|plugin, log, _| unsafe {
        let notifier = NOTIFIER
            .with(|n| n.borrow().clone())
            .expect("connected at init");
        let rescans = || log.port_rescans.lock().expect("port rescans").clone();
        notifier.notify(HostNotice::AUDIO_PORTS);
        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(
            rescans(),
            [CLAP_AUDIO_PORTS_RESCAN_LIST],
            "inactive: rescan now"
        );

        assert!((*plugin).activate.unwrap()(plugin, 48_000.0, 1, 64));
        let restarts = log.restarts.load(Ordering::Relaxed);
        notifier.notify(HostNotice::AUDIO_PORTS);
        (*plugin).on_main_thread.unwrap()(plugin);
        assert_eq!(log.restarts.load(Ordering::Relaxed), restarts + 1);
        assert_eq!(rescans().len(), 1, "no list rescan while active");
        (*plugin).deactivate.unwrap()(plugin);
        assert_eq!(rescans().len(), 2, "rescanned at deactivate");
    });
}

#[test]
fn voice_info_is_hidden_without_hooks() {
    // SAFETY: the host and descriptor outlive the serial calls.
    unsafe {
        let mut host: clap_host = std::mem::zeroed();
        host.clap_version = clap_sys::version::CLAP_VERSION;
        let descriptor = DescriptorHolder::new(&super::host_value_fixture::TestPlugin::info());
        let plugin = create_plugin_instance::<super::host_value_fixture::TestPlugin>(
            &raw const descriptor.descriptor,
            &raw const host,
        );
        assert!((*plugin).init.unwrap()(plugin));
        assert!((*plugin).get_extension.unwrap()(plugin, CLAP_EXT_VOICE_INFO.as_ptr()).is_null());
        (*plugin).destroy.unwrap()(plugin);
    }
}

#[test]
fn param_indication_reaches_the_hooks() {
    with_modular(|plugin, _, _| unsafe {
        let _ = seen();
        let ext = &*(*plugin).get_extension.unwrap()(plugin, CLAP_EXT_PARAM_INDICATION.as_ptr())
            .cast::<clap_plugin_param_indication>();
        let color = clap_color {
            alpha: 1,
            red: 2,
            green: 3,
            blue: 4,
        };
        ext.set_mapping.unwrap()(
            plugin,
            0,
            true,
            &raw const color,
            c"Knob 1".as_ptr(),
            ptr::null(),
        );
        ext.set_mapping.unwrap()(plugin, 0, false, ptr::null(), ptr::null(), ptr::null());
        ext.set_automation.unwrap()(
            plugin,
            0,
            CLAP_PARAM_INDICATION_AUTOMATION_RECORDING,
            ptr::null(),
        );
        assert_eq!(
            seen(),
            [
                Seen::Indication(
                    0,
                    ParamIndication::Mapping(Some(HostMapping {
                        color: Some(HostColor {
                            r: 2,
                            g: 3,
                            b: 4,
                            a: 1
                        }),
                        label: "Knob 1".into(),
                        description: String::new(),
                    }))
                ),
                Seen::Indication(0, ParamIndication::Mapping(None)),
                Seen::Indication(
                    0,
                    ParamIndication::Automation {
                        state: AutomationState::Recording,
                        color: None
                    }
                ),
            ]
        );
    });
}

/// Items a fake menu builder received: (kind, label, action).
type Built = Vec<(clap_context_menu_item_kind, String, u32)>;

unsafe extern "C" fn add_item(
    builder: *const clap_context_menu_builder,
    kind: clap_context_menu_item_kind,
    data: *const c_void,
) -> bool {
    // SAFETY: `ctx` is the test's `Built`; `data` matches `kind`.
    unsafe {
        let built = &mut *(*builder).ctx.cast::<Built>();
        let item = match kind {
            CLAP_CONTEXT_MENU_ITEM_ENTRY => {
                let e = &*data.cast::<clap_context_menu_entry>();
                (CStr::from_ptr(e.label), e.action_id)
            }
            CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY => {
                let e = &*data.cast::<clap_context_menu_check_entry>();
                (CStr::from_ptr(e.label), e.action_id)
            }
            _ => (c"", 0),
        };
        built.push((kind, item.0.to_string_lossy().into_owned(), item.1));
    }
    true
}

#[test]
fn context_menu_populates_and_performs_through_the_hooks() {
    with_modular(|plugin, _, _| unsafe {
        let _ = seen();
        let ext = &*(*plugin).get_extension.unwrap()(plugin, CLAP_EXT_CONTEXT_MENU.as_ptr())
            .cast::<clap_plugin_context_menu>();
        let mut built = Built::new();
        let builder = clap_context_menu_builder {
            ctx: (&raw mut built).cast(),
            add_item: Some(add_item),
            supports: None,
        };
        let target = clap_context_menu_target {
            kind: CLAP_CONTEXT_MENU_TARGET_KIND_PARAM,
            id: 0,
        };
        assert!(ext.populate.unwrap()(
            plugin,
            &raw const target,
            &raw const builder
        ));
        assert_eq!(
            built,
            [
                (CLAP_CONTEXT_MENU_ITEM_ENTRY, "Reset".to_owned(), 1),
                (CLAP_CONTEXT_MENU_ITEM_SEPARATOR, String::new(), 0),
                (CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY, "Learn".to_owned(), 2),
            ]
        );
        built.clear();
        assert!(ext.populate.unwrap()(
            plugin,
            ptr::null(),
            &raw const builder
        ));
        assert_eq!(built.len(), 1, "a null target is the global menu");

        assert!(ext.perform.unwrap()(plugin, &raw const target, 2));
        assert!(!ext.perform.unwrap()(plugin, ptr::null(), 9));
        assert_eq!(
            seen(),
            [
                Seen::Perform(MenuTarget::Param(0), 2),
                Seen::Perform(MenuTarget::Global, 9),
            ]
        );
    });
}

#[test]
fn host_log_and_thread_check_integrate_with_host_handle() {
    use clap_sys::ext::log::{CLAP_EXT_LOG, CLAP_LOG_ERROR, clap_host_log, clap_log_severity};
    use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
    use clap_sys::host::clap_host;
    use std::ffi::CStr;
    use std::sync::Mutex;

    static LOGGED: Mutex<Vec<(clap_log_severity, String)>> = Mutex::new(Vec::new());
    static ON_MAIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    static ON_AUDIO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

    unsafe extern "C" fn log_cb(
        _host: *const clap_host,
        severity: clap_log_severity,
        msg: *const std::ffi::c_char,
    ) {
        let text = unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned();
        LOGGED.lock().unwrap().push((severity, text));
    }

    unsafe extern "C" fn is_main(_host: *const clap_host) -> bool {
        ON_MAIN.load(std::sync::atomic::Ordering::Acquire)
    }

    unsafe extern "C" fn is_audio(_host: *const clap_host) -> bool {
        ON_AUDIO.load(std::sync::atomic::Ordering::Acquire)
    }

    unsafe extern "C" fn get_ext(
        _host: *const clap_host,
        id: *const std::ffi::c_char,
    ) -> *const std::ffi::c_void {
        static LOG_EXT: clap_host_log = clap_host_log { log: Some(log_cb) };
        static TC_EXT: clap_host_thread_check = clap_host_thread_check {
            is_main_thread: Some(is_main),
            is_audio_thread: Some(is_audio),
        };
        let id = unsafe { CStr::from_ptr(id) };
        if id == CLAP_EXT_LOG {
            (&raw const LOG_EXT).cast()
        } else if id == CLAP_EXT_THREAD_CHECK {
            (&raw const TC_EXT).cast()
        } else {
            std::ptr::null()
        }
    }

    LOGGED.lock().unwrap().clear();
    ON_MAIN.store(true, std::sync::atomic::Ordering::Release);
    ON_AUDIO.store(true, std::sync::atomic::Ordering::Release);

    let host = clap_host {
        clap_version: clap_sys::version::CLAP_VERSION,
        host_data: std::ptr::null_mut(),
        name: c"test".as_ptr(),
        vendor: c"test".as_ptr(),
        url: c"".as_ptr(),
        version: c"1".as_ptr(),
        get_extension: Some(get_ext),
        request_restart: None,
        request_process: None,
        request_callback: None,
    };
    let handle = unsafe { crate::host_handle::HostHandle::any(&raw const host) };
    let log_ext = unsafe { handle.extension::<clap_host_log>(CLAP_EXT_LOG) }.expect("log ext");
    let tc_ext = unsafe { handle.extension::<clap_host_thread_check>(CLAP_EXT_THREAD_CHECK) }
        .expect("thread-check ext");

    assert_eq!(unsafe { handle.is_main_thread(tc_ext) }, Some(true));
    assert_eq!(unsafe { handle.is_audio_thread(tc_ext) }, Some(true));
    unsafe {
        handle.debug_assert_main_thread(tc_ext);
        handle.debug_assert_audio_thread(tc_ext);
    }

    let latch = moose_core::wrapper::PanicLatch::new();
    let _ = moose_core::wrapper::run_audio_block_latched::<(), ()>(&latch, "CLAP", || {
        panic!("boom in dsp")
    });
    assert!(latch.is_failed());
    latch.report_with(|msg| {
        assert!(unsafe { handle.log(log_ext, CLAP_LOG_ERROR, msg) });
    });

    let entries = LOGGED.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, CLAP_LOG_ERROR);
    assert!(
        entries[0].1.contains("boom in dsp"),
        "got: {}",
        entries[0].1
    );

    ON_MAIN.store(false, std::sync::atomic::Ordering::Release);
    assert_eq!(unsafe { handle.is_main_thread(tc_ext) }, Some(false));
}
