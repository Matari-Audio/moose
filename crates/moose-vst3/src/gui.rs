//! Editor callbacks and [`Vst3Host`].

use crate::com;
use crate::instance::{EditorScaleState, StateLoadQueue, Vst3Instance};
use crate::lifecycle::vst3_midi_source;
use crate::midi_proxy::{MIDI_PROXY_PER_CHANNEL, allocated_midi_proxy_id};

use moose_core::TransportSlot;
use moose_core::editor::{
    EditGroup, Editor, EditorBridge, HostContext, PluginContext, RawWindowHandle,
    clamp_logical_size, fit_logical_size,
};
use moose_core::events::TransportInfo;
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostMessage, HostQueue, HostSignal};
use moose_core::main_thread::{HostNotice, MenuTarget};
use moose_core::meters::MeterStore;
use moose_core::snapshot::SnapshotSlot;
use moose_core::state;
use moose_core::wrapper::{custom_state_view, run_extern_callback_with};
use moose_params::Params;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// HostContext (100% safe Rust — zero raw instance pointers)
// ---------------------------------------------------------------------------

/// One VST3 instance seen from its editor.
pub(crate) struct Vst3Host<P: PluginExport> {
    host_id: usize,
    scale: Arc<EditorScaleState>,
    params: Arc<P::Params>,
    meter_store: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
    pending_state: Arc<StateLoadQueue>,
    transport: Arc<TransportSlot>,
    queue: Arc<HostQueue>,
    _plugin: PhantomData<fn() -> P>,
}

impl<P: PluginExport> Vst3Host<P> {
    fn from_instance(inst: &Vst3Instance<P>) -> Self {
        Self {
            host_id: inst.host_id(),
            scale: Arc::clone(&inst.scale),
            params: Arc::clone(&inst.params_arc),
            meter_store: Arc::clone(&inst.meter_store),
            snapshot: Arc::clone(&inst.snapshot),
            pending_state: Arc::clone(&inst.pending_state),
            transport: Arc::clone(&inst.transport_slot),
            queue: Arc::clone(&inst.host_queue),
            _plugin: PhantomData,
        }
    }
}

impl<P: PluginExport> EditorBridge for Vst3Host<P> {
    fn begin_edit(&self, id: u32) {
        com::begin_edit(self.host_id, id);
    }

    fn set_param(&self, id: u32, normalized: f64) {
        let norm = self
            .params
            .set_normalized_returning_normalized(id, normalized);
        if self.params.parameter_affects_presentation(id) {
            self.queue.signal(HostSignal::PRESENTATION);
        }
        com::perform_edit(self.host_id, id, norm);
    }

    fn end_edit(&self, id: u32) {
        com::end_edit(self.host_id, id);
    }

    fn request_resize(&self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 {
            return false;
        }
        let (pw, ph) = logical_to_phys(w, h, self.scale.host_scale());
        com::request_resize(self.host_id, pw, ph)
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
        custom_state_view(&self.snapshot, &self.pending_state)
    }

    fn set_state(&self, data: Vec<u8>) {
        self.pending_state.force_push(state::DeserializedState {
            extra: Some(data),
            ..Default::default()
        });
    }

    fn transport(&self) -> Option<TransportInfo> {
        self.transport.read()
    }
}

impl<P: PluginExport> HostContext for Vst3Host<P> {
    fn mark_dirty(&self) {
        self.queue.signal(HostSignal::MARK_DIRTY);
        com::flush_host_queue(self.host_id);
    }

    fn group_edits(&self, edge: EditGroup) {
        com::group_edit(self.host_id, edge);
    }

    fn request_restart(&self) {
        self.queue.signal(HostSignal::RESTART);
        com::flush_host_queue(self.host_id);
    }

    fn context_menu(&self, target: MenuTarget, x: i32, y: i32) -> bool {
        if !com::has_context_menu(self.host_id) {
            return false;
        }
        let ok = self
            .queue
            .post(HostMessage::ContextMenu { target, x, y })
            .is_ok();
        if ok {
            com::flush_host_queue(self.host_id);
        }
        ok
    }

    fn notify(&self, notice: HostNotice) {
        self.queue.signal(notice);
        com::flush_host_queue(self.host_id);
    }

    fn window_rescaled(&self, scale: f64) {
        if scale.is_finite() && scale > 0.0 {
            self.scale
                .window_scale
                .store(scale.to_bits(), Ordering::Relaxed);
        }
    }
}

/// Build the [`PluginContext`] handed to the instance's editor.
pub(crate) fn editor_context<P: PluginExport>(inst: &Vst3Instance<P>) -> PluginContext {
    let host: Arc<dyn HostContext> = Arc::new(Vst3Host::<P>::from_instance(inst));
    PluginContext::from_host(host, Arc::clone(&inst.params_arc) as Arc<dyn Params>)
        .with_tasks(inst.task_spawner.clone())
}

/// Compute the physical `ViewRect` dimensions for `editor` on the current platform.
fn editor_phys_size<P: PluginExport>(inst: &Vst3Instance<P>, editor: &dyn Editor) -> (u32, u32) {
    let (ew, eh) = editor.size();
    #[cfg(target_os = "macos")]
    {
        let _ = inst;
        (ew, eh)
    }
    #[cfg(not(target_os = "macos"))]
    {
        logical_to_phys(ew, eh, inst.host_scale())
    }
}

// ---------------------------------------------------------------------------
// GUI methods on Vst3Instance
// ---------------------------------------------------------------------------

impl<P: PluginExport> Vst3Instance<P> {
    pub(crate) fn gui_has_editor_impl(&self) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "gui_has_editor", false, || {
            let mut gui = self.gui.enter();
            if gui.editor.is_none() {
                gui.editor = (self.editor_builder)(self.params_arc.clone());
                if let (Some(scale), Some(editor)) =
                    (self.reported_host_scale(), gui.editor.as_mut())
                {
                    editor.set_scale_factor(scale);
                }
                if let Some(ref editor) = gui.editor {
                    let (pw, ph) = editor_phys_size(self, editor.as_ref());
                    self.store_phys_size(pw, ph);
                }
            }
            gui.editor.is_some()
        })
    }

    pub(crate) fn gui_get_size_impl(&self) -> (u32, u32) {
        run_extern_callback_with::<P, (u32, u32)>("vst3", "gui_get_size", (0, 0), || {
            let Some(mut gui) = self.gui.try_enter() else {
                let mut packed = self.pending_resize.load(Ordering::Relaxed);
                if packed == 0 {
                    packed = self.last_phys_size.load(Ordering::Relaxed);
                }
                if packed != 0 {
                    #[allow(clippy::cast_possible_truncation)]
                    return ((packed >> 32) as u32, packed as u32);
                }
                return (0, 0);
            };
            let packed = self.pending_resize.swap(0, Ordering::Relaxed);
            if let Some(editor) = gui.editor.as_deref() {
                self.note_window_scale(editor);
            }
            if packed != 0
                && let Some(editor) = gui.editor.as_mut()
            {
                #[allow(clippy::cast_possible_truncation)]
                let (pw, ph) = ((packed >> 32) as u32, packed as u32);
                apply_physical_resize(editor.as_mut(), pw, ph, self.host_scale());
            }
            if let Some(ref editor) = gui.editor {
                let (pw, ph) = editor_phys_size(self, editor.as_ref());
                self.store_phys_size(pw, ph);
                (pw, ph)
            } else {
                (0, 0)
            }
        })
    }

    pub(crate) fn gui_param_at_impl(&self, x: i32, y: i32) -> Option<u32> {
        run_extern_callback_with::<P, Option<u32>>("vst3", "gui_param_at", None, || {
            let gui = self.gui.try_enter()?;
            let editor = gui.editor.as_deref()?;
            #[cfg(target_os = "macos")]
            let (lx, ly) = (x, y);
            #[cfg(not(target_os = "macos"))]
            let (lx, ly) = {
                self.note_window_scale(editor);
                let scale = self.host_scale();
                #[allow(clippy::cast_possible_truncation)]
                (
                    (f64::from(x) / scale).round() as i32,
                    (f64::from(y) / scale).round() as i32,
                )
            };
            editor.param_at(lx, ly)
        })
    }

    pub(crate) fn gui_set_content_scale_impl(&self, scale: f64) {
        run_extern_callback_with::<P, ()>("vst3", "gui_set_content_scale", (), || {
            if cfg!(target_os = "macos") || !scale.is_finite() || scale <= 0.0 {
                return;
            }
            let scale = scale.clamp(0.25, 8.0);
            let changed = self.reported_host_scale() != Some(scale);
            self.set_host_scale(scale);
            let Some(mut gui) = self.gui.try_enter() else {
                return;
            };
            let Some(editor) = gui.editor.as_mut() else {
                return;
            };
            editor.set_scale_factor(scale);
            let (lw, lh) = editor.size();
            let (pw, ph) = logical_to_phys(lw, lh, scale);
            self.store_phys_size(pw, ph);
            drop(gui);
            if changed && lw > 0 && lh > 0 {
                let _ = com::request_resize(self.host_id(), pw, ph);
            }
        });
    }

    pub(crate) fn gui_can_resize_impl(&self) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "gui_can_resize", false, || {
            self.gui
                .enter()
                .editor
                .as_ref()
                .is_some_and(|e| e.can_resize())
        })
    }

    pub(crate) fn gui_check_size_constraint_impl(&self, w: u32, h: u32) -> (u32, u32) {
        run_extern_callback_with::<P, (u32, u32)>(
            "vst3",
            "gui_check_size_constraint",
            (w, h),
            || {
                let Some(gui) = self.gui.try_enter() else {
                    return (w, h);
                };
                let Some(ref editor) = gui.editor else {
                    return (w, h);
                };
                self.note_window_scale(editor.as_ref());
                let host_scale = self.host_scale();
                if editor.can_resize() {
                    let (lw, lh) = phys_to_logical(w, h, host_scale);
                    let (fw, fh) = fit_logical_size(lw, lh, editor.as_ref());
                    logical_to_phys(fw, fh, host_scale)
                } else {
                    let (cw, ch) = editor.size();
                    logical_to_phys(cw, ch, host_scale)
                }
            },
        )
    }

    pub(crate) fn gui_set_size_impl(&self, w: u32, h: u32) {
        run_extern_callback_with::<P, ()>("vst3", "gui_set_size", (), || {
            if w == 0 || h == 0 {
                return;
            }
            match self.gui.try_enter() {
                Some(mut gui) => {
                    self.pending_resize.store(0, Ordering::Relaxed);
                    if let Some(editor) = gui.editor.as_mut() {
                        self.note_window_scale(editor.as_ref());
                        apply_physical_resize(editor.as_mut(), w, h, self.host_scale());
                        let (pw, ph) = editor_phys_size(self, editor.as_ref());
                        self.store_phys_size(pw, ph);
                    }
                }
                None => {
                    self.pending_resize
                        .store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
                }
            }
        });
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) fn midi_mapping_get_param_id_impl(
        &self,
        bus_index: i32,
        channel: i16,
        controller: i16,
    ) -> Option<u32> {
        let source = vst3_midi_source(controller)?;
        let channel = u8::try_from(channel).ok()?;
        if let Some(id) = moose_params::map_source_to_param(&self.param_infos, channel, source) {
            return Some(id);
        }
        let port = u8::try_from(bus_index).ok()?;
        if P::info().accepts_midi_in
            && port < P::info().midi_input_ports
            && channel < 16
            && let Ok(controller) = u32::try_from(controller)
            && controller < MIDI_PROXY_PER_CHANNEL
        {
            return allocated_midi_proxy_id(&self.midi_proxy_ids, port, channel, controller);
        }
        None
    }

    pub(crate) fn gui_open_impl(&self, parent: *mut std::ffi::c_void) -> bool {
        #[cfg(target_os = "macos")]
        let handle = RawWindowHandle::AppKit(parent);
        #[cfg(target_os = "windows")]
        let handle = RawWindowHandle::Win32(parent);
        #[cfg(target_os = "linux")]
        let handle = RawWindowHandle::X11(parent as u64);
        self.gui_open_with(handle)
    }

    pub(crate) fn gui_open_with(&self, handle: RawWindowHandle) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "gui_open", false, || {
            let mut gui = self.gui.enter();
            if let Some(ref mut editor) = gui.editor {
                if let Some(scale) = self.reported_host_scale() {
                    editor.set_scale_factor(scale);
                }
                let (pw, ph) = editor_phys_size(self, editor.as_ref());
                self.store_phys_size(pw, ph);
                let context = editor_context(self);
                editor.open(handle, context);
                if let Some(scale) = self.reported_host_scale() {
                    editor.set_scale_factor(scale);
                }
                self.note_window_scale(editor.as_ref());
                let (pw, ph) = editor_phys_size(self, editor.as_ref());
                self.store_phys_size(pw, ph);
                editor.is_open()
            } else {
                false
            }
        })
    }

    pub(crate) fn gui_close_impl(&self) {
        run_extern_callback_with::<P, ()>("vst3", "gui_close", (), || {
            if let Some(ref mut editor) = self.gui.enter().editor {
                editor.close();
            }
        });
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn gui_supports_wayland_impl(&self) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "gui_supports_wayland", false, || {
            let gui = self.gui.enter();
            gui.editor
                .as_ref()
                .is_some_and(|e| e.supports_wayland_embedding())
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn gui_open_wayland_impl(
        &self,
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
    ) -> bool {
        self.gui_open_with(RawWindowHandle::WaylandSurface { display, surface })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn gui_event_fd_impl(&self) -> Option<i32> {
        run_extern_callback_with::<P, Option<i32>>("vst3", "gui_event_fd", None, || {
            let gui = self.gui.enter();
            gui.editor.as_ref().and_then(|e| e.event_fd())
        })
    }

    pub(crate) fn gui_on_fd_impl(&self) {
        run_extern_callback_with::<P, ()>("vst3", "gui_on_fd", (), || {
            if let Some(mut gui) = self.gui.try_enter()
                && let Some(editor) = gui.editor.as_mut()
            {
                editor.on_fd();
            }
        });
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn gui_on_timer_impl(&self) {
        run_extern_callback_with::<P, ()>("vst3", "gui_on_timer", (), || {
            if let Some(mut gui) = self.gui.try_enter()
                && let Some(editor) = gui.editor.as_mut()
            {
                editor.on_timer();
            }
        });
    }

    pub(crate) fn gui_wayland_set_sync_impl(&self, sync: bool) {
        run_extern_callback_with::<P, ()>("vst3", "gui_wayland_set_sync", (), || {
            if let Some(mut gui) = self.gui.try_enter()
                && let Some(editor) = gui.editor.as_mut()
            {
                editor.set_wayland_sync(sync);
            }
        });
    }
}

pub(crate) fn phys_to_logical(pw: u32, ph: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (pw, ph);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lw = (f64::from(pw) / host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lh = (f64::from(ph) / host_scale).round() as u32;
    (lw.max(1), lh.max(1))
}

pub(crate) fn logical_to_phys(lw: u32, lh: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (lw, lh);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pw = (f64::from(lw) * host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ph = (f64::from(lh) * host_scale).round() as u32;
    (pw.max(1), ph.max(1))
}

pub(crate) fn apply_physical_resize(editor: &mut dyn Editor, w: u32, h: u32, host_scale: f64) {
    if editor.can_resize() {
        let (lw, lh) = phys_to_logical(w, h, host_scale);
        let (cw, ch) = clamp_logical_size(lw, lh, editor);
        editor.set_size(cw, ch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_value_fixture::TestPlugin;
    use moose_core::thread::MainThread;
    use std::sync::Mutex;

    struct RecordingEditor {
        size: (u32, u32),
        open: bool,
        scales: Arc<Mutex<Vec<f64>>>,
    }

    impl Editor for RecordingEditor {
        fn size(&self) -> (u32, u32) {
            self.size
        }

        fn can_resize(&self) -> bool {
            true
        }

        fn set_size(&mut self, width: u32, height: u32) -> bool {
            self.size = (width, height);
            true
        }

        fn set_scale_factor(&mut self, factor: f64) {
            self.scales.lock().unwrap().push(factor);
        }

        fn open(&mut self, _parent: RawWindowHandle, _context: PluginContext) {
            self.open = true;
        }

        fn close(&mut self) {
            self.open = false;
        }

        fn is_open(&self) -> bool {
            self.open
        }
    }

    #[test]
    fn vst3_host_context_routes_state_and_signals_through_host_queue() {
        let inst = Vst3Instance::<TestPlugin>::create().expect("create");
        let editor_ctx = editor_context(&inst);

        editor_ctx.set_state(vec![1, 2, 3]);
        assert_eq!(editor_ctx.get_state(), [1, 2, 3]);

        editor_ctx.mark_dirty();
        editor_ctx.request_restart();
        editor_ctx.notify_host(HostNotice::AUDIO_PORTS);
        editor_ctx.window_rescaled(1.5);
        assert_eq!(
            f64::from_bits(inst.scale.window_scale.load(Ordering::Relaxed)),
            1.5
        );
        assert!(!editor_ctx.context_menu(MenuTarget::Global, 12, 34));

        // SAFETY: test thread acts as host main thread.
        let main = unsafe { MainThread::assume() };
        let drain = inst.host_queue.drain(&main);
        assert!(drain.signals.contains(HostSignal::MARK_DIRTY));
        assert!(drain.signals.contains(HostSignal::RESTART));
        assert!(drain.signals.notices().contains(HostNotice::AUDIO_PORTS));
        assert_eq!(drain.count(), 0, "a refused menu posts nothing");
    }

    #[test]
    fn busy_gui_get_size_returns_cached_last_phys_size_instead_of_zero() {
        let inst = Vst3Instance::<TestPlugin>::create().expect("create");
        inst.gui.enter().editor = Some(Box::new(RecordingEditor {
            size: (640, 400),
            open: false,
            scales: Arc::default(),
        }));

        assert_eq!(inst.gui_get_size_impl(), (640, 400));

        let held = inst.gui.enter();
        assert_eq!(inst.gui_get_size_impl(), (640, 400));
        drop(held);
    }

    #[test]
    fn gui_open_with_only_replays_reported_host_scale() {
        let inst = Vst3Instance::<TestPlugin>::create().expect("create");
        let scales = Arc::new(Mutex::new(Vec::new()));
        inst.gui.enter().editor = Some(Box::new(RecordingEditor {
            size: (500, 300),
            open: false,
            scales: Arc::clone(&scales),
        }));

        assert_eq!(inst.reported_host_scale(), None);
        assert!(inst.gui_open_with(RawWindowHandle::X11(1)));
        assert!(
            scales.lock().unwrap().is_empty(),
            "must not pin editor to default 1.0 scale when host reported no content scale"
        );

        inst.gui_close_impl();
        inst.gui_set_content_scale_impl(2.0);
        scales.lock().unwrap().clear();

        assert!(inst.gui_open_with(RawWindowHandle::X11(1)));
        #[cfg(target_os = "macos")]
        assert!(scales.lock().unwrap().is_empty());
        #[cfg(not(target_os = "macos"))]
        assert_eq!(*scales.lock().unwrap(), vec![2.0, 2.0]);
    }

    #[test]
    fn gui_set_size_clears_stale_pending_resize() {
        let inst = Vst3Instance::<TestPlugin>::create().expect("create");
        inst.gui.enter().editor = Some(Box::new(RecordingEditor {
            size: (400, 300),
            open: true,
            scales: Arc::default(),
        }));

        let held = inst.gui.enter();
        inst.gui_set_size_impl(900, 700);
        assert_ne!(inst.pending_resize.load(Ordering::Relaxed), 0);
        drop(held);

        inst.gui_set_size_impl(640, 480);
        assert_eq!(inst.pending_resize.load(Ordering::Relaxed), 0);

        assert_eq!(inst.gui_get_size_impl(), (640, 480));
    }

    #[test]
    fn midi_mapping_rejects_negative_channel() {
        let inst = Vst3Instance::<TestPlugin>::create().expect("create");
        assert_eq!(inst.midi_mapping_get_param_id_impl(0, -1, 1), None);
    }
}
