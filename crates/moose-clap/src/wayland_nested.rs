//! CLAP `wayland-surface-embedding-v1` from the `clap-wayland-experiment`
//! draft (`include/clap/ext/draft/wayland-experiment.h`, Oct 2026): the host
//! runs a nested compositor, hands us a socket to it and a token, and the
//! editor embeds its own surface there. The host pumps the editor through
//! `posix-fd-support` and `timer-support` on its main thread.
//!
//! Flow: `is_api_supported(API, false)` → `create` → `set_size` →
//! [`set_parent`] (ours, not `clap_plugin_gui.set_parent`) → `show`.

use super::{
    CStr, GuiMode, PluginExport, c_char, c_void, clap_host, clap_plugin, data_from_plugin, ptr,
    run_extern_callback_with,
};
use crate::gui::editor_context;
use clap_sys::ext::posix_fd_support::{
    CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
    clap_host_posix_fd_support, clap_plugin_posix_fd_support,
};
use clap_sys::ext::timer_support::{
    CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support, clap_plugin_timer_support,
};
use clap_sys::id::clap_id;
use moose_core::editor::RawWindowHandle;
use std::marker::PhantomData;

pub(super) const API: &CStr = c"wayland-surface-embedding-v1";
const EXT_NESTED_COMPOSITOR: &CStr = c"clap.gui-wayland-nested-compositor/1";
const EXT_XDG_FOREIGN_V2: &CStr = c"clap.gui-xdg-foreign-v2/1";
const EXT_XDG_ACTIVATION_V1: &CStr = c"clap.gui-xdg-activation-v1/1";
const EXT_XKB_KEY_FORWARDING: &CStr = c"clap.gui-xkb-key-forwarding/1";
/// The host's UI timer while embedded: frame deadlines need a clock.
const TICK_MS: u32 = 16;

#[repr(C)]
struct PluginNestedCompositor {
    set_parent: Option<unsafe extern "C" fn(*const clap_plugin, *const c_char) -> bool>,
}

#[repr(C)]
struct HostNestedCompositor {
    open_connection: Option<unsafe extern "C" fn(*const clap_host) -> i32>,
}

#[repr(C)]
struct PluginXdgForeignV2 {
    set_transient: Option<unsafe extern "C" fn(*const clap_plugin, *const c_char) -> bool>,
}

#[repr(C)]
struct PluginXdgActivationV1 {
    activate: Option<unsafe extern "C" fn(*const clap_plugin, *const c_char) -> bool>,
}

const CLAP_XKB_KEY_STATE_RELEASED: u32 = 0;
const CLAP_XKB_KEY_STATE_PRESSED: u32 = 1;

#[repr(C)]
struct ClapXkbKeyEvent {
    keycode: u32,
    keysym: u32,
    modifiers: u32,
    state: u32,
}

#[repr(C)]
struct HostXkbKeyForwarding {
    on_key: Option<unsafe extern "C" fn(*const clap_host, *const ClapXkbKeyEvent)>,
}

/// What the host registered for the open embedded editor.
#[derive(Debug, Default)]
pub(super) struct Session {
    fd: Option<i32>,
    write_registered: bool,
    timer: Option<clap_id>,
}

struct Extensions<P>(PhantomData<P>);
impl<P: PluginExport> Extensions<P> {
    const NESTED: PluginNestedCompositor = PluginNestedCompositor {
        set_parent: Some(set_parent::<P>),
    };
    const XDG_FOREIGN: PluginXdgForeignV2 = PluginXdgForeignV2 {
        set_transient: Some(xdg_foreign_set_transient::<P>),
    };
    const XDG_ACTIVATION: PluginXdgActivationV1 = PluginXdgActivationV1 {
        activate: Some(xdg_activation_activate::<P>),
    };
    const FD: clap_plugin_posix_fd_support = clap_plugin_posix_fd_support {
        on_fd: Some(on_fd::<P>),
    };
    const TIMER: clap_plugin_timer_support = clap_plugin_timer_support {
        on_timer: Some(on_timer::<P>),
    };
}

/// The plugin-side extensions this draft adds, by id.
pub(super) fn extension<P: PluginExport>(id: &CStr) -> Option<*const c_void> {
    let ext: *const c_void = if id == EXT_NESTED_COMPOSITOR {
        ptr::from_ref(&Extensions::<P>::NESTED).cast()
    } else if id == EXT_XDG_FOREIGN_V2 {
        ptr::from_ref(&Extensions::<P>::XDG_FOREIGN).cast()
    } else if id == EXT_XDG_ACTIVATION_V1 {
        ptr::from_ref(&Extensions::<P>::XDG_ACTIVATION).cast()
    } else if id == CLAP_EXT_POSIX_FD_SUPPORT {
        ptr::from_ref(&Extensions::<P>::FD).cast()
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        ptr::from_ref(&Extensions::<P>::TIMER).cast()
    } else {
        return None;
    };
    Some(ext)
}

/// Forward an unhandled Wayland XKB key event to the host when it exposes
/// `clap.gui-xkb-key-forwarding/1`. [main-thread]
pub(super) unsafe fn forward_xkb_key(
    host: *const clap_host,
    keycode: u32,
    keysym: u32,
    modifiers: u32,
    pressed: bool,
) -> bool {
    unsafe {
        let Some(on_key) =
            host_ext::<HostXkbKeyForwarding>(host, EXT_XKB_KEY_FORWARDING).and_then(|e| e.on_key)
        else {
            return false;
        };
        let event = ClapXkbKeyEvent {
            keycode,
            keysym,
            modifiers,
            state: if pressed {
                CLAP_XKB_KEY_STATE_PRESSED
            } else {
                CLAP_XKB_KEY_STATE_RELEASED
            },
        };
        on_key(host, &raw const event);
        true
    }
}

/// Update `CLAP_POSIX_FD_WRITE` interest via `modify_fd` when the editor's
/// buffered Wayland output state (`Editor::wants_write`) changes. [main-thread]
unsafe fn sync_write_interest(host: *const clap_host, session: &mut Session, wants_write: bool) {
    let Some(fd) = session.fd else {
        return;
    };
    if session.write_registered == wants_write {
        return;
    }
    let flags = if wants_write {
        CLAP_POSIX_FD_READ | CLAP_POSIX_FD_ERROR | CLAP_POSIX_FD_WRITE
    } else {
        CLAP_POSIX_FD_READ | CLAP_POSIX_FD_ERROR
    };
    unsafe {
        if let Some(modify) =
            host_ext::<clap_host_posix_fd_support>(host, CLAP_EXT_POSIX_FD_SUPPORT)
                .and_then(|ext| ext.modify_fd)
            && modify(host, fd, flags)
        {
            session.write_registered = wants_write;
        }
    }
}

/// The host extension `id` as `T`, if the host has it.
unsafe fn host_ext<T>(host: *const clap_host, id: &CStr) -> Option<&'static T> {
    unsafe {
        let get = host.as_ref()?.get_extension?;
        get(host, id.as_ptr()).cast::<T>().as_ref()
    }
}

/// Whether the host can open a connection and pump the editor.
pub(super) unsafe fn host_supports(host: *const clap_host) -> bool {
    unsafe {
        host_ext::<HostNestedCompositor>(host, EXT_NESTED_COMPOSITOR)
            .is_some_and(|ext| ext.open_connection.is_some())
            && host_ext::<clap_host_posix_fd_support>(host, CLAP_EXT_POSIX_FD_SUPPORT)
                .is_some_and(|ext| ext.register_fd.is_some() && ext.unregister_fd.is_some())
            && host_ext::<clap_host_timer_support>(host, CLAP_EXT_TIMER_SUPPORT)
                .is_some_and(|ext| ext.register_timer.is_some() && ext.unregister_timer.is_some())
    }
}

/// Unregisters what `session` registered. Call before closing the editor.
pub(super) unsafe fn unregister(host: *const clap_host, session: &mut Session) {
    unsafe {
        if let Some(fd) = session.fd.take()
            && let Some(unregister) =
                host_ext::<clap_host_posix_fd_support>(host, CLAP_EXT_POSIX_FD_SUPPORT)
                    .and_then(|ext| ext.unregister_fd)
        {
            unregister(host, fd);
        }
        session.write_registered = false;
        if let Some(id) = session.timer.take()
            && let Some(unregister) =
                host_ext::<clap_host_timer_support>(host, CLAP_EXT_TIMER_SUPPORT)
                    .and_then(|ext| ext.unregister_timer)
        {
            unregister(host, id);
        }
    }
}

/// `clap_plugin_gui_wayland_nested_compositor.set_parent`. [main-thread]
unsafe extern "C" fn set_parent<P: PluginExport>(
    plugin: *const clap_plugin,
    token: *const c_char,
) -> bool {
    if plugin.is_null() || token.is_null() {
        return false;
    }
    run_extern_callback_with::<P, bool>("CLAP", "nested set_parent", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        let host = data.host.as_ptr();
        let mut gui = data.gui.enter();
        if !gui.gui_created || gui.mode != GuiMode::WaylandNested || !host_supports(host) {
            return false;
        }
        // A second token moves the editor: the old window goes first.
        unregister(host, &mut gui.nested);
        let context = editor_context::<P>(plugin);
        let Some(editor) = gui.editor.as_mut() else {
            return false;
        };
        editor.close();
        let Some(open) = host_ext::<HostNestedCompositor>(host, EXT_NESTED_COMPOSITOR)
            .and_then(|ext| ext.open_connection)
        else {
            return false;
        };
        let socket = open(host);
        if socket < 0 {
            return false;
        }
        // The editor owns `socket` from here on, opened or not.
        editor.open(RawWindowHandle::WaylandNested { socket, token }, context);
        let Some(fd) = editor.event_fd().filter(|_| editor.is_open()) else {
            editor.close();
            return false;
        };
        let mut session = Session::default();
        let wants_write = editor.wants_write();
        let initial_flags = if wants_write {
            CLAP_POSIX_FD_READ | CLAP_POSIX_FD_ERROR | CLAP_POSIX_FD_WRITE
        } else {
            CLAP_POSIX_FD_READ | CLAP_POSIX_FD_ERROR
        };
        if let Some(fd_ext) =
            host_ext::<clap_host_posix_fd_support>(host, CLAP_EXT_POSIX_FD_SUPPORT)
            && let Some(register) = fd_ext.register_fd
            && register(host, fd, initial_flags)
        {
            session.fd = Some(fd);
            session.write_registered = wants_write;
        }
        let mut id: clap_id = 0;
        if let Some(timer_ext) = host_ext::<clap_host_timer_support>(host, CLAP_EXT_TIMER_SUPPORT)
            && let Some(register) = timer_ext.register_timer
            && register(host, TICK_MS, &raw mut id)
        {
            session.timer = Some(id);
        }
        if session.fd.is_none() || session.timer.is_none() {
            // Nothing would pump the editor: refuse instead of showing a dead surface.
            unregister(host, &mut session);
            editor.close();
            return false;
        }
        gui.nested = session;
        true
    })
}

/// `clap.gui-xdg-foreign-v2/1::set_transient`. [main-thread]
unsafe extern "C" fn xdg_foreign_set_transient<P: PluginExport>(
    plugin: *const clap_plugin,
    handle: *const c_char,
) -> bool {
    if plugin.is_null() || handle.is_null() {
        return false;
    }
    run_extern_callback_with::<P, bool>("CLAP", "xdg_foreign.set_transient", false, || unsafe {
        let Ok(handle) = CStr::from_ptr(handle).to_str() else {
            return false;
        };
        let mut gui = data_from_plugin::<P>(plugin).gui.enter();
        gui.editor
            .as_mut()
            .is_some_and(|editor| editor.set_wayland_transient(handle))
    })
}

/// `clap.gui-xdg-activation-v1/1::activate`. [main-thread]
unsafe extern "C" fn xdg_activation_activate<P: PluginExport>(
    plugin: *const clap_plugin,
    token: *const c_char,
) -> bool {
    if plugin.is_null() || token.is_null() {
        return false;
    }
    run_extern_callback_with::<P, bool>("CLAP", "xdg_activation.activate", false, || unsafe {
        let Ok(token) = CStr::from_ptr(token).to_str() else {
            return false;
        };
        let mut gui = data_from_plugin::<P>(plugin).gui.enter();
        gui.editor
            .as_mut()
            .is_some_and(|editor| editor.set_wayland_activation_token(token))
    })
}

/// `clap_plugin_posix_fd_support.on_fd`. [main-thread]
unsafe extern "C" fn on_fd<P: PluginExport>(plugin: *const clap_plugin, fd: i32, _: u32) {
    if plugin.is_null() {
        return;
    }
    run_extern_callback_with::<P, ()>("CLAP", "on_fd", (), || unsafe {
        let data = data_from_plugin::<P>(plugin);
        // Busy means another GUI call is on the stack; the timer catches up.
        let Some(mut gui) = data.gui.try_enter() else {
            return;
        };
        if gui.nested.fd == Some(fd) {
            let wants_write = if let Some(editor) = gui.editor.as_mut() {
                editor.on_fd();
                editor.wants_write()
            } else {
                false
            };
            sync_write_interest(data.host.as_ptr(), &mut gui.nested, wants_write);
        }
    });
}

/// `clap_plugin_timer_support.on_timer`. [main-thread]
unsafe extern "C" fn on_timer<P: PluginExport>(plugin: *const clap_plugin, id: clap_id) {
    if plugin.is_null() {
        return;
    }
    run_extern_callback_with::<P, ()>("CLAP", "on_timer", (), || unsafe {
        let data = data_from_plugin::<P>(plugin);
        let Some(mut gui) = data.gui.try_enter() else {
            return;
        };
        if gui.nested.timer == Some(id) {
            let wants_write = if let Some(editor) = gui.editor.as_mut() {
                editor.on_timer();
                editor.wants_write()
            } else {
                false
            };
            sync_write_interest(data.host.as_ptr(), &mut gui.nested, wants_write);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_value_fixture::TestPlugin;
    use crate::*;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    const TIMER: clap_id = 7;

    #[derive(Default)]
    struct Log {
        token: Option<String>,
        registered: Vec<i32>,
        modified: Vec<(i32, u32)>,
        unregistered: Vec<i32>,
        timers: Vec<clap_id>,
        stopped: Vec<clap_id>,
        pumped: (u32, u32),
        transient: Option<String>,
        activation: Option<String>,
        forwarded_keys: Vec<(u32, u32, u32, u32)>,
    }
    type Shared = Arc<Mutex<Log>>;

    /// `host_data`: whether the host offers the draft, and the call log.
    struct Host {
        offer: AtomicBool,
        log: Shared,
        /// The compositor's end of each connection the plugin opened.
        peers: Mutex<Vec<UnixStream>>,
    }
    unsafe fn state(host: *const clap_host) -> &'static Host {
        // SAFETY: each test keeps its `Host` alive past the plugin.
        unsafe { &*(*host).host_data.cast::<Host>() }
    }
    unsafe extern "C" fn open_connection(host: *const clap_host) -> i32 {
        let (ours, theirs) = UnixStream::pair().unwrap();
        unsafe { state(host) }.peers.lock().unwrap().push(theirs);
        ours.into_raw_fd()
    }
    unsafe extern "C" fn register_fd(host: *const clap_host, fd: i32, _: u32) -> bool {
        unsafe { state(host) }
            .log
            .lock()
            .unwrap()
            .registered
            .push(fd);
        true
    }
    unsafe extern "C" fn modify_fd(host: *const clap_host, fd: i32, flags: u32) -> bool {
        unsafe { state(host) }
            .log
            .lock()
            .unwrap()
            .modified
            .push((fd, flags));
        true
    }
    unsafe extern "C" fn on_xkb_key(host: *const clap_host, ev: *const ClapXkbKeyEvent) {
        let ev = unsafe { &*ev };
        unsafe { state(host) }
            .log
            .lock()
            .unwrap()
            .forwarded_keys
            .push((ev.keycode, ev.keysym, ev.modifiers, ev.state));
    }
    unsafe extern "C" fn unregister_fd(host: *const clap_host, fd: i32) -> bool {
        unsafe { state(host) }
            .log
            .lock()
            .unwrap()
            .unregistered
            .push(fd);
        true
    }
    unsafe extern "C" fn register_timer(host: *const clap_host, ms: u32, id: *mut clap_id) -> bool {
        assert_eq!(ms, TICK_MS);
        unsafe {
            *id = TIMER;
            state(host).log.lock().unwrap().timers.push(TIMER);
        }
        true
    }
    unsafe extern "C" fn unregister_timer(host: *const clap_host, id: clap_id) -> bool {
        unsafe { state(host) }.log.lock().unwrap().stopped.push(id);
        true
    }
    unsafe extern "C" fn extension(host: *const clap_host, id: *const c_char) -> *const c_void {
        static NESTED: HostNestedCompositor = HostNestedCompositor {
            open_connection: Some(open_connection),
        };
        static FD: clap_host_posix_fd_support = clap_host_posix_fd_support {
            register_fd: Some(register_fd),
            modify_fd: Some(modify_fd),
            unregister_fd: Some(unregister_fd),
        };
        static TIMERS: clap_host_timer_support = clap_host_timer_support {
            register_timer: Some(register_timer),
            unregister_timer: Some(unregister_timer),
        };
        static XKB_KEYS: HostXkbKeyForwarding = HostXkbKeyForwarding {
            on_key: Some(on_xkb_key),
        };
        // SAFETY: CLAP passes a valid id; the host outlives the plugin.
        let (id, offer) = unsafe {
            (
                CStr::from_ptr(id),
                state(host).offer.load(Ordering::Relaxed),
            )
        };
        match () {
            () if offer && id == EXT_NESTED_COMPOSITOR => (&raw const NESTED).cast(),
            () if id == CLAP_EXT_POSIX_FD_SUPPORT => (&raw const FD).cast(),
            () if id == CLAP_EXT_TIMER_SUPPORT => (&raw const TIMERS).cast(),
            () if id == EXT_XKB_KEY_FORWARDING => (&raw const XKB_KEYS).cast(),
            () => ptr::null(),
        }
    }

    /// Takes the socket as an editor does, and logs the token and pumping.
    struct Probe {
        log: Shared,
        socket: Option<OwnedFd>,
        want_write: bool,
        ctx: Option<PluginContext>,
    }
    impl Editor for Probe {
        fn size(&self) -> (u32, u32) {
            (320, 240)
        }
        fn open(&mut self, parent: RawWindowHandle, ctx: PluginContext) {
            let RawWindowHandle::WaylandNested { socket, token } = parent else {
                panic!("expected a nested-compositor handle, got {parent:?}");
            };
            // SAFETY: the handle's contract: `open` owns the socket, the token is live.
            unsafe {
                self.socket = Some(OwnedFd::from_raw_fd(socket));
                self.log.lock().unwrap().token =
                    Some(CStr::from_ptr(token).to_string_lossy().into());
            }
            self.ctx = Some(ctx);
        }
        fn is_open(&self) -> bool {
            self.socket.is_some()
        }
        fn supports_wayland_embedding(&self) -> bool {
            true
        }
        fn event_fd(&self) -> Option<i32> {
            self.socket.as_ref().map(AsRawFd::as_raw_fd)
        }
        fn on_fd(&mut self) {
            self.log.lock().unwrap().pumped.0 += 1;
            self.want_write = true;
            if let Some(ctx) = &self.ctx {
                assert!(ctx.forward_xkb_key(65, 0x20, 0x01, true));
            }
        }
        fn wants_write(&self) -> bool {
            self.want_write
        }
        fn set_wayland_transient(&mut self, handle: &str) -> bool {
            self.log.lock().unwrap().transient = Some(handle.to_owned());
            true
        }
        fn set_wayland_activation_token(&mut self, token: &str) -> bool {
            self.log.lock().unwrap().activation = Some(token.to_owned());
            true
        }
        fn on_timer(&mut self) {
            self.log.lock().unwrap().pumped.1 += 1;
            self.want_write = false;
        }
        fn close(&mut self) {
            self.socket = None;
            self.ctx = None;
        }
    }

    #[test]
    fn embeds_through_the_hosts_compositor_and_unregisters_on_destroy() {
        let log = Shared::default();
        let state = Host {
            offer: AtomicBool::new(false),
            log: Arc::clone(&log),
            peers: Mutex::default(),
        };
        // SAFETY: own host, descriptor, plugin and fake editor; `state` outlives the instance.
        unsafe {
            let mut host: clap_host = std::mem::zeroed();
            host.clap_version = CLAP_VERSION;
            host.host_data = (&raw const state).cast_mut().cast();
            host.get_extension = Some(extension);
            let descriptor = DescriptorHolder::new(&TestPlugin::info());
            let plugin = create_plugin_instance::<TestPlugin>(
                &raw const descriptor.descriptor,
                &raw const host,
            );
            assert!(clap_plugin_init::<TestPlugin>(plugin));
            let data = data_from_plugin::<TestPlugin>(plugin);
            data.gui.enter().editor = Some(Box::new(Probe {
                log: Arc::clone(&log),
                socket: None,
                want_write: false,
                ctx: None,
            }));
            let api = API.as_ptr();
            assert!(
                !gui_is_api_supported::<TestPlugin>(plugin, api, false),
                "host lacks the draft"
            );
            state.offer.store(true, Ordering::Relaxed);
            assert!(gui_is_api_supported::<TestPlugin>(plugin, api, false));
            assert!(
                !gui_is_api_supported::<TestPlugin>(plugin, api, true),
                "never floating"
            );
            assert!(gui_create::<TestPlugin>(plugin, api, false));
            // The draft keeps clap_plugin_gui.set_parent and set_scale out of it.
            let window = clap_window {
                api,
                specific: clap_sys::ext::gui::clap_window_handle { x11: 1 },
            };
            assert!(!gui_set_parent::<TestPlugin>(plugin, &raw const window));
            assert!(!gui_set_scale::<TestPlugin>(plugin, 2.));

            let get = |id: &CStr| clap_plugin_get_extension::<TestPlugin>(plugin, id.as_ptr());
            let nested = &*get(EXT_NESTED_COMPOSITOR).cast::<PluginNestedCompositor>();
            let xdg_foreign = &*get(EXT_XDG_FOREIGN_V2).cast::<PluginXdgForeignV2>();
            let xdg_activation = &*get(EXT_XDG_ACTIVATION_V1).cast::<PluginXdgActivationV1>();
            let fds = &*get(CLAP_EXT_POSIX_FD_SUPPORT).cast::<clap_plugin_posix_fd_support>();
            let timers = &*get(CLAP_EXT_TIMER_SUPPORT).cast::<clap_plugin_timer_support>();
            assert!(nested.set_parent.unwrap()(plugin, c"slot-1".as_ptr()));
            assert!(xdg_foreign.set_transient.unwrap()(
                plugin,
                c"xdg-handle-1".as_ptr()
            ));
            assert!(xdg_activation.activate.unwrap()(
                plugin,
                c"act-token-1".as_ptr()
            ));
            let fd = data
                .gui
                .enter()
                .editor
                .as_ref()
                .unwrap()
                .event_fd()
                .unwrap();
            {
                let log = log.lock().unwrap();
                assert_eq!(log.token.as_deref(), Some("slot-1"));
                assert_eq!(log.transient.as_deref(), Some("xdg-handle-1"));
                assert_eq!(log.activation.as_deref(), Some("act-token-1"));
                assert_eq!(
                    (log.registered.as_slice(), log.timers.as_slice()),
                    (&[fd][..], &[TIMER][..])
                );
            }
            fds.on_fd.unwrap()(plugin, fd, CLAP_POSIX_FD_READ);
            fds.on_fd.unwrap()(plugin, fd + 100, CLAP_POSIX_FD_READ);
            timers.on_timer.unwrap()(plugin, TIMER);
            timers.on_timer.unwrap()(plugin, TIMER + 1);
            // Drain main-thread HostQueue to deliver forwarded XKB key.
            clap_plugin_on_main_thread::<TestPlugin>(plugin);
            {
                let log = log.lock().unwrap();
                assert_eq!(log.pumped, (1, 1), "only our fd and timer pump the editor");
                assert_eq!(
                    log.modified,
                    [
                        (
                            fd,
                            CLAP_POSIX_FD_READ | CLAP_POSIX_FD_ERROR | CLAP_POSIX_FD_WRITE
                        ),
                        (fd, CLAP_POSIX_FD_READ | CLAP_POSIX_FD_ERROR),
                    ]
                );
                assert_eq!(
                    log.forwarded_keys,
                    [(65, 0x20, 0x01, CLAP_XKB_KEY_STATE_PRESSED)]
                );
            }

            assert!(gui_show::<TestPlugin>(plugin));
            assert!(gui_hide::<TestPlugin>(plugin));
            assert!(
                data.gui.enter().editor.as_ref().unwrap().is_open(),
                "hide keeps the embedding"
            );
            gui_destroy::<TestPlugin>(plugin);
            {
                let log = log.lock().unwrap();
                assert_eq!(
                    (log.unregistered.as_slice(), log.stopped.as_slice()),
                    (&[fd][..], &[TIMER][..])
                );
            }
            clap_plugin_destroy::<TestPlugin>(plugin);
        }
    }
}
