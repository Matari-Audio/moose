//! `clap.context-menu`. Plugin side: the host asks for the plugin's items
//! ([`MainThreadHooks::menu_items`]) and reports a pick
//! ([`MainThreadHooks::perform_menu`]). Host side: the editor asks the host
//! to show its menu (`PluginContext::context_menu`), which the wrapper
//! queues and pops up from `on_main_thread` ([`popup`]).
//!
//! [`MainThreadHooks::menu_items`]: moose_core::main_thread::MainThreadHooks::menu_items
//! [`MainThreadHooks::perform_menu`]: moose_core::main_thread::MainThreadHooks::perform_menu

use std::ffi::{CString, c_void};

use clap_sys::ext::context_menu::{
    CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU, CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY,
    CLAP_CONTEXT_MENU_ITEM_END_SUBMENU, CLAP_CONTEXT_MENU_ITEM_ENTRY,
    CLAP_CONTEXT_MENU_ITEM_SEPARATOR, CLAP_CONTEXT_MENU_ITEM_TITLE,
    CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL, CLAP_CONTEXT_MENU_TARGET_KIND_PARAM,
    CLAP_EXT_CONTEXT_MENU, CLAP_EXT_CONTEXT_MENU_COMPAT, clap_context_menu_builder,
    clap_context_menu_check_entry, clap_context_menu_entry, clap_context_menu_item_kind,
    clap_context_menu_item_title, clap_context_menu_submenu, clap_context_menu_target,
    clap_host_context_menu, clap_plugin_context_menu,
};
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;
use moose_core::export::PluginExport;
use moose_core::main_thread::{MenuItem, MenuTarget};
use moose_core::thread::MainThread;
use moose_core::wrapper::run_extern_callback_with;

use crate::data::ClapPluginData;
use crate::data_from_plugin;
use crate::host_handle::{Any, HostHandle};

pub(crate) fn make_extension<P: PluginExport>() -> clap_plugin_context_menu {
    clap_plugin_context_menu {
        populate: Some(populate::<P>),
        perform: Some(perform::<P>),
    }
}

/// # Safety
/// `target` is null or points to a readable target.
unsafe fn menu_target(target: *const clap_context_menu_target) -> MenuTarget {
    // SAFETY: caller contract. A null target means the plugin as a whole
    // (context-menu.h).
    match unsafe { target.as_ref() } {
        Some(t) if t.kind == CLAP_CONTEXT_MENU_TARGET_KIND_PARAM => MenuTarget::Param(t.id),
        _ => MenuTarget::Global,
    }
}

/// A label for C; interior NULs are dropped.
fn c_label(label: &str) -> CString {
    CString::new(label.replace('\0', "")).unwrap_or_default()
}

/// Add one item through the host's builder. `false` when the host refused it.
///
/// # Safety
/// `builder` points to the live builder the host passed to `populate`.
unsafe fn add(builder: *const clap_context_menu_builder, item: &MenuItem) -> bool {
    // SAFETY: caller contract.
    let Some(add_item) = (unsafe { (*builder).add_item }) else {
        return false;
    };
    let push = |kind: clap_context_menu_item_kind, data: *const c_void| {
        // SAFETY: `data` points to the item struct `kind` names, alive for
        // the call; the host copies what it keeps (context-menu.h).
        unsafe { add_item(builder, kind, data) }
    };
    match item {
        MenuItem::Entry {
            label,
            action,
            enabled,
        } => {
            let label = c_label(label);
            let entry = clap_context_menu_entry {
                label: label.as_ptr(),
                is_enabled: *enabled,
                action_id: *action,
            };
            push(CLAP_CONTEXT_MENU_ITEM_ENTRY, (&raw const entry).cast())
        }
        MenuItem::Check {
            label,
            action,
            enabled,
            checked,
        } => {
            let label = c_label(label);
            let entry = clap_context_menu_check_entry {
                label: label.as_ptr(),
                is_enabled: *enabled,
                is_checked: *checked,
                action_id: *action,
            };
            push(
                CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY,
                (&raw const entry).cast(),
            )
        }
        MenuItem::Separator => push(CLAP_CONTEXT_MENU_ITEM_SEPARATOR, std::ptr::null()),
        MenuItem::Submenu { label, enabled } => {
            let label = c_label(label);
            let submenu = clap_context_menu_submenu {
                label: label.as_ptr(),
                is_enabled: *enabled,
            };
            push(
                CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU,
                (&raw const submenu).cast(),
            )
        }
        MenuItem::EndSubmenu => push(CLAP_CONTEXT_MENU_ITEM_END_SUBMENU, std::ptr::null()),
        MenuItem::Title { label } => {
            let label = c_label(label);
            let title = clap_context_menu_item_title {
                title: label.as_ptr(),
                is_enabled: true,
            };
            push(CLAP_CONTEXT_MENU_ITEM_TITLE, (&raw const title).cast())
        }
        // Item kinds added to moose after this wrapper are left out.
        _ => true,
    }
}

unsafe extern "C" fn populate<P: PluginExport>(
    plugin: *const clap_plugin,
    target: *const clap_context_menu_target,
    builder: *const clap_context_menu_builder,
) -> bool {
    if builder.is_null() {
        return false;
    }
    // SAFETY: CLAP passes the live instance created for `P`.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    // SAFETY: `clap_plugin_context_menu.populate` is [main-thread]
    // (context-menu.h).
    let main = unsafe { MainThread::assume() };
    // SAFETY: the host passes null or a valid target.
    let target = unsafe { menu_target(target) };
    run_extern_callback_with::<P, bool>("CLAP", "context_menu populate", false, || {
        let Some(items) = data.with_hooks(&main, |hooks| hooks.menu_items(&main, target)) else {
            return false;
        };
        // SAFETY: `builder` is the host's live builder, checked non-null.
        items.iter().all(|item| unsafe { add(builder, item) })
    })
}

unsafe extern "C" fn perform<P: PluginExport>(
    plugin: *const clap_plugin,
    target: *const clap_context_menu_target,
    action_id: clap_id,
) -> bool {
    // SAFETY: CLAP passes the live instance created for `P`.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    // SAFETY: `clap_plugin_context_menu.perform` is [main-thread]
    // (context-menu.h).
    let main = unsafe { MainThread::assume() };
    // SAFETY: the host passes null or a valid target.
    let target = unsafe { menu_target(target) };
    run_extern_callback_with::<P, bool>("CLAP", "context_menu perform", false, || {
        data.with_hooks(&main, |hooks| hooks.perform_menu(&main, target, action_id))
            .unwrap_or(false)
    })
}

/// The host's `clap.context-menu`, if it can pop a menu up.
///
/// # Safety
/// `host` is null or the live host. `get_extension` is [thread-safe].
pub(crate) unsafe fn host_menu(host: HostHandle<Any>) -> Option<&'static clap_host_context_menu> {
    // SAFETY: caller contract; host extensions live as long as the host.
    unsafe {
        let ext = host
            .extension::<clap_host_context_menu>(CLAP_EXT_CONTEXT_MENU)
            .or_else(|| host.extension::<clap_host_context_menu>(CLAP_EXT_CONTEXT_MENU_COMPAT))?;
        ext.popup.is_some().then_some(ext)
    }
}

/// Refresh the main-thread-only capability after initialization or GUI creation.
pub(crate) fn refresh_capability<P: PluginExport>(data: &ClapPluginData<P>, main: &MainThread) {
    let host = data.host.on_main_thread(main);
    // SAFETY: the instance owns a live host; `HostHandle<MainThread>` proves
    // main-thread access for `can_popup`.
    let supported = unsafe {
        host_menu(host.as_any()).is_some_and(|menu| {
            menu.can_popup
                .is_none_or(|can_popup| can_popup(host.as_ptr()))
        })
    };
    data.menu_popup_supported
        .store(supported, std::sync::atomic::Ordering::Release);
}

/// Logical editor points to the host's window coordinates, at the scale
/// `request_resize` uses.
fn to_host_coords(v: i32, scale: f64) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a scaled window coordinate is far inside i32"
    )]
    let scaled = (f64::from(v) * scale).round() as i32;
    scaled
}

/// [main-thread] Show the host's context menu for `target` at the editor's
/// logical `(x, y)`. Nothing happens when the host has no menu or says it
/// can't pop one up now.
pub(crate) fn popup<P: PluginExport>(
    data: &ClapPluginData<P>,
    main: &MainThread,
    target: MenuTarget,
    x: i32,
    y: i32,
) {
    let host = data.host.on_main_thread(main);
    // SAFETY: the instance holds a live host pointer.
    let Some(menu) = (unsafe { host_menu(host.as_any()) }) else {
        return;
    };
    let target = match target {
        MenuTarget::Param(id) => clap_context_menu_target {
            kind: CLAP_CONTEXT_MENU_TARGET_KIND_PARAM,
            id,
        },
        MenuTarget::Global => clap_context_menu_target {
            kind: CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL,
            id: 0,
        },
    };
    let scale = data.host_scale();
    // SAFETY: `can_popup` and `popup` are [main-thread] (context-menu.h),
    // which `HostHandle<MainThread>` proves; `target` lives across the call.
    unsafe {
        if let Some(can_popup) = menu.can_popup
            && !can_popup(host.as_ptr())
        {
            return;
        }
        if let Some(popup) = menu.popup {
            popup(
                host.as_ptr(),
                &raw const target,
                0,
                to_host_coords(x, scale),
                to_host_coords(y, scale),
            );
        }
    }
}
