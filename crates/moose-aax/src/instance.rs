//! Per-instance state behind the AAX shim's opaque context pointer.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, AtomicU64};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::ThreadId;

use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::bus::BusLayout;
use moose_core::c_abi_driver::GestureQueue;
use moose_core::delay::ReportedDelay;
use moose_core::editor::{Editor, EditorBuilder};
use moose_core::events::{EventList, LosslessEventCursor};
use moose_core::export::PluginExport;
use moose_core::host_queue::HostQueue;
use moose_core::main_thread::MainThreadHooks;
use moose_core::meters::MeterStore;
use moose_core::plugin::PluginRuntime;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state::StateLoadQueue;
use moose_core::tasks::AnyTaskSpawner;
use moose_core::thread::MainThreadCell;
use moose_core::wrapper::{PanicLatch, PluginCell};
use moose_params::{CatalogSignal, ParamCatalog, Params, PersistChangeSignal};

use crate::ffi::MOOSE_AAX_EVENT_END;
use crate::host_sink::AaxHostSink;

/// Opaque `AaxInstance<P>` handle carried across host callbacks and editor bridges.
#[derive(Clone, Copy)]
pub(crate) struct AaxCtx(pub(crate) *mut c_void);

// SAFETY: `AaxCtx` is dereferenced only inside `HostQueue::with_open`, and
// `cb_destroy` calls `HostQueue::close()` (waiting for all active scopes)
// before freeing the instance.
unsafe impl Send for AaxCtx {}
unsafe impl Sync for AaxCtx {}

impl AaxCtx {
    #[inline]
    pub(crate) fn as_ptr(self) -> *mut c_void {
        self.0
    }
}

/// Active output event transaction between `begin_output_events` and
/// `finish_output_events`.
pub(crate) struct OutputTransaction {
    pub(crate) cursor: LosslessEventCursor,
    pub(crate) preflight_error: u32,
}

/// Audio-thread and preparation scratch owned by an `AaxInstance`.
pub(crate) struct AaxAudio<P: PluginExport> {
    pub(crate) scratch: RawBufferScratch<<P as PluginRuntime>::Sample>,
    pub(crate) event_list: EventList,
    pub(crate) sub_event_scratch: EventList,
    pub(crate) output_events: EventList,
    pub(crate) sample_rate: f64,
    pub(crate) max_frames: u32,
    pub(crate) catalog: Arc<ParamCatalog>,
    pub(crate) output_tx: Option<OutputTransaction>,
    pub(crate) staging_status: u32,
    pub(crate) emits_midi: bool,
    pub(crate) prepared: bool,
    pub(crate) bus_layout: BusLayout,
}

impl<P: PluginExport> AaxAudio<P> {
    pub(crate) fn new(catalog: Arc<ParamCatalog>, emits_midi: bool, bus_layout: BusLayout) -> Self {
        let cap = moose_core::events::EVENT_LIST_PREALLOC;
        Self {
            scratch: RawBufferScratch::default(),
            event_list: EventList::with_capacity(cap),
            sub_event_scratch: EventList::with_capacity(cap),
            output_events: EventList::with_capacity(cap),
            sample_rate: 44_100.0,
            max_frames: 0,
            catalog,
            output_tx: None,
            staging_status: MOOSE_AAX_EVENT_END,
            emits_midi,
            prepared: false,
            bus_layout,
        }
    }
}

/// Main/UI-thread-owned editor state.
#[derive(Default)]
pub(crate) struct AaxGui {
    pub(crate) editor: Option<Box<dyn Editor>>,
    pub(crate) open: bool,
}

/// Per-instance AAX runtime state.
pub(crate) struct AaxInstance<P: PluginExport> {
    /// Keep persist-change hook registered for the instance lifetime.
    pub(crate) persist_signal: Option<Arc<PersistChangeSignal>>,
    /// Keep catalog-change hook registered for the instance lifetime.
    pub(crate) catalog_signal: Option<Arc<CatalogSignal>>,
    /// Main-thread notification mailbox drained from `cb_drain_host_queue`
    /// (invoked by AAX `TimerWakeup`).
    pub(crate) host_queue: Arc<HostQueue>,
    /// Main-thread plugin hooks connected to `host_queue`.
    pub(crate) _main_thread: MainThreadCell<Box<dyn MainThreadHooks>>,
    /// Plugin DSP/lifecycle state.
    pub(crate) plugin: PluginCell<P>,
    /// Shared parameter store.
    pub(crate) params_arc: Arc<P::Params>,
    /// Live parameter catalog queried on the host control thread.
    pub(crate) exposed: Mutex<Arc<ParamCatalog>>,
    /// Shared meter storage.
    pub(crate) meter_store: Arc<MeterStore>,
    /// Lock-free custom-state snapshot slot.
    pub(crate) snapshot: Arc<SnapshotSlot>,
    /// Optional background-task spawner.
    pub(crate) task_spawner: Option<AnyTaskSpawner>,
    /// Lock-free editor factory cached at creation.
    pub(crate) editor_builder: EditorBuilder<P::Params>,
    /// Event-driven latency & tail tracker.
    pub(crate) delay: ReportedDelay,
    /// Current `ProcessMode` discriminant (`0` realtime, `1` buffered, `2` offline).
    pub(crate) render_mode: AtomicU8,
    /// Thread ID of the host UI/main thread (`create_with_sink` / `drain_host_queue`).
    pub(crate) ui_thread: Mutex<ThreadId>,
    /// Bounded off-UI-thread editor gesture queue drained on `TimerWakeup`.
    pub(crate) editor_gestures: Mutex<GestureQueue>,
    /// Configured parameter automation mode.
    pub(crate) automation: moose_core::Automation,
    pub(crate) input_bus_count: usize,
    pub(crate) output_bus_count: usize,
    pub(crate) sidechain_channels: u32,
    pub(crate) plugin_id_hash: u64,
    /// Audio/preparation scratch cell.
    pub(crate) audio: PluginCell<AaxAudio<P>>,
    /// Main/UI-thread editor cell.
    pub(crate) gui: PluginCell<AaxGui>,
    /// Packed `(width << 32) | height` deferred editor resize.
    pub(crate) pending_resize: AtomicU64,
    /// Shared transport snapshot written by audio thread, read by editor.
    pub(crate) transport_slot: Arc<TransportSlot>,
    /// Bounded SPSC state-load queue.
    pub(crate) pending_state: Arc<StateLoadQueue>,
    /// Audio-thread panic latch cleared on the next clean `reset`.
    pub(crate) panic_latch: PanicLatch,
    /// Host notification sink. MUST remain the **last** field so its `Drop`
    /// (`release_user_data`) runs after every signal, editor, and plugin drops.
    pub(crate) host_sink: AaxHostSink,
}

impl<P: PluginExport> AaxInstance<P> {
    pub(crate) fn record_ui_thread(&self) {
        let mut guard = self
            .ui_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = std::thread::current().id();
    }

    pub(crate) fn is_ui_thread(&self) -> bool {
        let guard = self
            .ui_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard == std::thread::current().id()
    }

    /// Rebuild the live exposed catalog snapshot from `params_arc`.
    pub(crate) fn sync_catalog(&self) {
        let _ = self.params_arc.take_catalog_changes();
        let fresh = self.params_arc.param_catalog();
        let mut guard = self.exposed.lock().unwrap_or_else(PoisonError::into_inner);
        *guard = Arc::clone(&fresh);
        drop(guard);
        if let Some(mut audio) = self.audio.try_enter() {
            audio.catalog = fresh;
        }
    }

    /// Snapshot the current live parameter catalog.
    pub(crate) fn catalog_snapshot(&self) -> Arc<ParamCatalog> {
        Arc::clone(&self.exposed.lock().unwrap_or_else(PoisonError::into_inner))
    }
}
