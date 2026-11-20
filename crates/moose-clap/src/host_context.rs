//! The editor's [`HostContext`] for CLAP: one type for every editor-to-host
//! call. Gestures and values go out through the thread-safe GUI change
//! queue and `params.request_flush`; everything CLAP allows only on the
//! main thread goes through the instance's [`HostQueue`] and is made in
//! `on_main_thread`.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap_sys::plugin::clap_plugin;
use moose_core::editor::{EditorBridge, EditorNotificationDelivery, HostContext, PluginContext};
use moose_core::events::TransportInfo;
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostMessage, HostQueue, HostSignal};
use moose_core::main_thread::{HostNotice, MenuTarget};
use moose_core::meters::MeterStore;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::transport::TransportSlot;
use moose_core::wrapper::custom_state_view;
use moose_params::Params;

use crate::data::{GuiChangeQueue, GuiParamChange, StateLoadQueue};
use crate::data_from_plugin;
use crate::gui::request_host_resize;
use crate::host_handle::{Any, HostHandle, HostParamsHandle};

/// Instance pointer held by [`ClapHost`]. Only dereferenced inside
/// `HostQueue::with_open`, which `clap_plugin_destroy` joins via `close()`
/// before freeing the plugin instance.
#[derive(Clone, Copy)]
struct PluginHandle(*const clap_plugin);

// SAFETY: `ClapHost` only dereferences `self.0` inside `self.queue.with_open`,
// and only reads/writes atomic fields (`floating_mode`, `window_scale`,
// `host_scale`) on the underlying `ClapPluginData<P>`.
unsafe impl Send for PluginHandle {}
unsafe impl Sync for PluginHandle {}

impl PluginHandle {
    fn as_ptr(self) -> *const clap_plugin {
        self.0
    }
}

/// One CLAP instance seen from its editor.
pub(crate) struct ClapHost<P: PluginExport> {
    /// The live instance. Guarded by `queue.with_open` against `destroy`.
    plugin: PluginHandle,
    host: HostHandle<Any>,
    host_params: HostParamsHandle,
    params: Arc<P::Params>,
    gui_changes: Arc<GuiChangeQueue>,
    gui_overflow: Arc<std::sync::atomic::AtomicBool>,
    pending_state: Arc<StateLoadQueue>,
    snapshot: Arc<SnapshotSlot>,
    meter_store: Arc<MeterStore>,
    transport: Arc<TransportSlot>,
    queue: Arc<HostQueue>,
    _plugin: PhantomData<fn() -> P>,
}

impl<P: PluginExport> ClapHost<P> {
    /// # Safety
    /// `plugin` is a live instance created for `P`.
    unsafe fn new(plugin: *const clap_plugin) -> Self {
        // SAFETY: caller contract.
        let data = unsafe { data_from_plugin::<P>(plugin) };
        // SAFETY: the host and its extensions outlive the instance, and
        // `HostHandle<Any>` / `HostParamsHandle` expose only `[thread-safe]`
        // operations, all gated by `queue.with_open`.
        unsafe {
            Self {
                plugin: PluginHandle(plugin),
                host: data.host,
                host_params: HostParamsHandle::new(data.host_params),
                params: Arc::clone(&data.params_arc),
                gui_changes: Arc::clone(&data.gui_changes),
                gui_overflow: Arc::clone(&data.gui_overflow),
                pending_state: Arc::clone(&data.pending_state),
                snapshot: Arc::clone(&data.snapshot),
                meter_store: Arc::clone(&data.meter_store),
                transport: Arc::clone(&data.transport_slot),
                queue: Arc::clone(&data.host_queue),
                _plugin: PhantomData,
            }
        }
    }

    /// Ask the host to flush the queued gestures (`params.request_flush`,
    /// [thread-safe]). Hosts without `clap.params` get nothing.
    fn request_flush(&self) {
        self.queue.with_open(|| {
            // SAFETY: `with_open` holds the instance open against `destroy`,
            // so the host and its extension pointer are live.
            unsafe { self.host.request_flush(self.host_params) };
        });
    }

    /// Queue a gesture for the audio thread and poke the host. `false` when
    /// the queue was full.
    fn push(&self, change: GuiParamChange) -> bool {
        let queued = self.gui_changes.push(change).is_ok();
        self.request_flush();
        queued
    }
}

impl<P: PluginExport> EditorBridge for ClapHost<P> {
    fn begin_edit(&self, id: u32) {
        // The queue drops a gesture only if the audio thread hasn't drained
        // `GUI_QUEUE_CAPACITY` of them; values still reach the host through
        // the rescan fallback in `set_param`.
        self.push(GuiParamChange::GestureBegin(id));
    }

    fn set_param(&self, id: u32, normalized: f64) {
        let plain = self.params.set_normalized_returning_plain(id, normalized);
        if self.params.parameter_affects_presentation(id) {
            self.queue.signal(HostSignal::PRESENTATION);
        }
        // The queued PARAM_VALUE event is how the host hears this edit. A
        // values rescan per drag frame made hosts (Bitwig) re-read every
        // parameter mid-gesture, so it is only the fallback for a value the
        // full queue dropped.
        if !self.push(GuiParamChange::Value(id, plain)) {
            self.gui_overflow.store(true, Ordering::Release);
            self.queue.signal(HostSignal::PARAM_RESCAN);
            self.request_flush();
        }
    }

    fn end_edit(&self, id: u32) {
        self.push(GuiParamChange::GestureEnd(id));
    }

    fn request_resize(&self, w: u32, h: u32) -> bool {
        self.queue
            .with_open(|| {
                // SAFETY: `with_open` keeps the instance alive against `destroy`.
                let data = unsafe { data_from_plugin::<P>(self.plugin.as_ptr()) };
                if data.floating_mode.load(Ordering::Acquire) {
                    return false;
                }
                // The scale is read live, so a `set_scale` after the editor opened
                // counts.
                // SAFETY: the host outlives the instance; `gui.request_resize` is
                // [thread-safe & !floating] (gui.h).
                unsafe { request_host_resize(self.host, data.host_scale(), w, h) }
            })
            .unwrap_or(false)
    }

    fn get_param(&self, id: u32) -> f64 {
        self.params.get_normalized(id).unwrap_or(0.0)
    }

    fn get_param_plain(&self, id: u32) -> f64 {
        self.params.get_plain(id).unwrap_or(0.0)
    }

    fn format_param(&self, id: u32) -> String {
        let plain = self.params.get_plain(id).unwrap_or(0.0);
        self.params
            .format_value(id, plain)
            .unwrap_or_else(|| format!("{plain:.1}"))
    }

    fn get_meter(&self, id: u32) -> f32 {
        self.meter_store.read(id)
    }

    fn get_state(&self) -> Vec<u8> {
        // Never touches the plugin. A load still queued for the audio
        // thread wins over the snapshot.
        custom_state_view(&self.snapshot, &self.pending_state)
    }

    fn set_state(&self, data: Vec<u8>) {
        // Raw custom-state bytes, exactly what `save_state()` emits. Always
        // enqueued, never applied here: this can run on the editor's own
        // thread, and the plugin cell is entered only from the audio thread
        // (active) or the main thread (inactive), which drains the queue in
        // `state_save` / `deactivate`.
        self.pending_state.force_push(state::DeserializedState {
            extra: Some(data),
            ..Default::default()
        });
    }

    fn transport(&self) -> Option<TransportInfo> {
        self.transport.read()
    }
}

impl<P: PluginExport> HostContext for ClapHost<P> {
    fn delivery(&self) -> EditorNotificationDelivery {
        EditorNotificationDelivery::QueuedEdits
    }

    fn mark_dirty(&self) {
        // `state.mark_dirty` is [main-thread].
        self.queue.signal(HostSignal::MARK_DIRTY);
    }

    fn request_restart(&self) {
        self.queue.signal(HostSignal::RESTART);
    }

    fn context_menu(&self, target: MenuTarget, x: i32, y: i32) -> bool {
        let supported = self
            .queue
            .with_open(|| {
                // SAFETY: `with_open` keeps the instance alive against `destroy`.
                let data = unsafe { data_from_plugin::<P>(self.plugin.as_ptr()) };
                !data.floating_mode.load(Ordering::Acquire)
                    && data.menu_popup_supported.load(Ordering::Acquire)
            })
            .unwrap_or(false);
        supported
            && self
                .queue
                .post(HostMessage::ContextMenu { target, x, y })
                .is_ok()
    }

    fn notify(&self, notice: HostNotice) {
        self.queue.signal(notice);
    }

    fn window_rescaled(&self, scale: f64) {
        // The next `request_resize` converts at the window's new scale
        // while the host has sent none of its own.
        if scale.is_finite() && scale > 0.0 {
            self.queue.with_open(|| {
                // SAFETY: `with_open` keeps the instance alive against `destroy`.
                let data = unsafe { data_from_plugin::<P>(self.plugin.as_ptr()) };
                data.window_scale.store(scale.to_bits(), Ordering::Relaxed);
            });
        }
    }
}

/// The context handed to the instance's editor.
///
/// # Safety
/// `plugin` is a live instance created for `P`.
pub(crate) unsafe fn editor_context<P: PluginExport>(plugin: *const clap_plugin) -> PluginContext {
    // SAFETY: caller contract.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    // SAFETY: caller contract.
    let host: Arc<dyn HostContext> = Arc::new(unsafe { ClapHost::<P>::new(plugin) });
    PluginContext::from_host(host, Arc::clone(&data.params_arc) as Arc<dyn Params>)
        .with_tasks(data.task_spawner.clone())
}
