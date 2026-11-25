//! Lifecycle, host-notice, bus-layout, and [`Vst3Backend`] implementation for [`Vst3Instance`].

use crate::backend::{
    ParamPresentation, ProcessArgs, Vst3Backend, Vst3LearnSource, Vst3NativeEvent, Vst3TrackInfo,
    WakerFn,
};
use crate::com::{self, Host};
use crate::input_events::VST3_EVENT_END;
use crate::instance::{
    EditorScaleState, K_IO_CHANGED, K_KEYSWITCH_CHANGED, K_LATENCY_CHANGED, K_PARAM_VALUES_CHANGED,
    K_RELOAD_COMPONENT, OutputNoteIds, PendingOutputMutation, Vst3Gui, Vst3Instance, Vst3Scratch,
};
use crate::midi_proxy::{
    MIDI_PROXY_PER_CHANNEL, allocate_midi_proxy_ids, midi_proxy_default, midi_proxy_len,
};
use crate::output_events::MidiMap;
use crate::registration::max_layout_channels;
use crate::state::StateLoadQueue;

use moose_core::TransportSlot;
use moose_core::buffer::RawBufferScratch;
use moose_core::bus::{BusConfig, BusKind, BusLayout};
use moose_core::config::{AudioConfig, ProcessMode};
use moose_core::events::{EVENT_LIST_PREALLOC, EventList, LosslessEventCursor};
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostQueue, HostSignal, HostSink, dispatch};
use moose_core::info::HostIdentity;
use moose_core::main_thread::{
    BusDirection, HostColor, HostNotifier, Keyswitch, LearnSource, MenuTarget, Midi2Controller,
    Midi2ControllerAssignment, Midi2ControllerKind, ProgramPreset, TrackInfo,
};
use moose_core::plugin::{PreparationConfig, PreparationFrameBound};
use moose_core::rt::RtSection;
use moose_core::state;
use moose_core::tasks::InstanceContext;
use moose_core::thread::{MainThread, MainThreadCell};
use moose_core::wrapper::{PluginCell, apply_queued_state, enter_plugin, run_extern_callback_with};
use moose_params::{MidiSource, ParamFunction, ParamRange, Params, PersistChangeSignal};
use std::ffi::{CStr, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

impl<P: PluginExport> Vst3Instance<P> {
    /// Construct a new plugin instance, firewalling any panic in `P::create` / `init`.
    /// `host` reaches the plugin's `init` and every `prepare`.
    pub(crate) fn create(host: HostIdentity) -> Option<Self> {
        run_extern_callback_with::<P, Option<Self>>("vst3", "create", None, || {
            let mut plugin = P::create_for(InstanceContext::new(host.clone()));
            plugin.init();
            let info = P::info();
            let param_infos = plugin.params().param_catalog();
            let mut param_ranges: Vec<(u32, ParamRange)> =
                param_infos.iter().map(|i| (i.id, i.range)).collect();
            param_ranges.sort_by_key(|(id, _)| *id);
            let param_id_indices = param_infos.sorted_id_indices();
            let mut midi_maps: Vec<(u32, MidiMap)> = param_infos
                .iter()
                .filter_map(|i| MidiMap::from_param(i).map(|m| (i.id, m)))
                .collect();
            midi_maps.sort_by_key(|(id, _)| *id);
            let params_arc = plugin.params_arc();
            let mut hooks = plugin.main_thread();

            let host_wake_target = Arc::new(AtomicPtr::<Host>::new(std::ptr::null_mut()));
            let host_queue = Arc::new(HostQueue::new({
                let host_wake_target = Arc::clone(&host_wake_target);
                move || {
                    let host = host_wake_target.load(Ordering::Acquire);
                    if !host.is_null() {
                        // SAFETY: `bind_host_queue(0, None)` clears `host_wake_target`
                        // and calls `host_queue.close()` before `Host::drop` tears down
                        // `Host` or its wakes.
                        unsafe { com::wake_host_queue(host) };
                    }
                }
            }));
            let host_notifier = HostNotifier::from_queue(Arc::clone(&host_queue));
            hooks.connect(host_notifier);
            let main_thread = MainThreadCell::new(hooks);
            let meter_store = plugin.meter_store();
            let feedback = Arc::new(moose_core::EditorFeedback::new(
                param_infos.iter().map(|i| i.id),
            ));
            let snapshot = plugin.snapshot_slot();
            let task_spawner = plugin.task_spawner();
            let editor_builder = plugin.editor_builder();
            let delay = moose_core::delay::ReportedDelay::new(plugin.latency(), plugin.tail());
            let midi_proxy_ids = allocate_midi_proxy_ids(&param_infos, midi_proxy_len::<P>());
            let event_capacity = EVENT_LIST_PREALLOC + 2 * param_infos.len();
            let midi_proxy_values = (0..midi_proxy_ids.len())
                .map(|i| {
                    #[allow(clippy::cast_possible_truncation)]
                    let controller = (i as u32) % MIDI_PROXY_PER_CHANNEL;
                    AtomicU64::new(midi_proxy_default(controller).to_bits())
                })
                .collect();

            Some(Self {
                host: std::sync::Mutex::new(host),
                persist_signal: std::sync::Mutex::new(None),
                catalog_signal: std::sync::Mutex::new(None),
                main_thread,
                plugin: PluginCell::new(plugin),
                params_arc,
                meter_store,
                feedback,
                snapshot,
                task_spawner,
                editor_builder,
                param_infos,
                automation: info.automation,
                plugin_id_hash: state::shared_plugin_state_hash(&info),
                active: AtomicBool::new(false),
                processing: AtomicBool::new(false),
                param_ranges,
                param_id_indices,
                midi_maps,
                transport_slot: TransportSlot::new(),
                pending_state: Arc::new(StateLoadQueue::new(1)),
                panic_latch: moose_core::wrapper::PanicLatch::new(),
                delay,
                midi_proxy_ids,
                midi_proxy_values,
                scale: Arc::new(EditorScaleState::default()),
                pending_resize: AtomicU64::new(0),
                last_phys_size: AtomicU64::new(0),
                audio: PluginCell::new(Vst3Scratch {
                    event_list: EventList::with_capacity(event_capacity),
                    input_num_frames: 0,
                    output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
                    sub_event_scratch: EventList::with_capacity(event_capacity),
                    sample_rate: 44100.0,
                    max_block_size: 8192,
                    prepared: false,
                    scratch: RawBufferScratch::default(),
                    output_cursor: LosslessEventCursor::default(),
                    output_preflight_status: VST3_EVENT_END,
                    output_note_ids: OutputNoteIds::new(),
                    pending_output_mutation: PendingOutputMutation::None,
                    param_changed_this_block: false,
                }),
                gui: PluginCell::new(Vst3Gui { editor: None }),
                host_queue,
                host_id: AtomicUsize::new(0),
                host_wake_target,
            })
        })
    }

    pub(crate) fn drain_host_queue_impl(&self) {
        run_extern_callback_with::<P, ()>("vst3", "drain_host_queue", (), || {
            // SAFETY: the COM layer calls this only on the host UI thread.
            let main = unsafe { MainThread::assume() };
            let drain = self.host_queue.drain(&main);
            let mut sink = Vst3Sink {
                host_id: self.host_id(),
                inst: self,
                main: &main,
                restart: 0,
                presentation: false,
            };
            dispatch(drain, &mut sink);
        });
    }

    pub(crate) fn prepare_impl(
        &self,
        sample_rate: f64,
        max_frames: u32,
        process_mode: i32,
    ) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "prepare", false, || {
            let mut audio = self.audio.enter();
            audio.prepared = false;
            let max_frames = max_frames as usize;
            let config = PreparationConfig::new(
                AudioConfig::new(sample_rate, max_frames)
                    .with_process_mode(vst3_process_mode(process_mode)),
                PreparationFrameBound::NegotiatedHost,
            )
            .with_host(
                self.host
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            );
            if config.validate().is_err() {
                return false;
            }
            audio.sample_rate = sample_rate;
            audio.max_block_size = max_frames;
            let (num_in, num_out) = max_layout_channels(&P::bus_layouts());
            audio
                .scratch
                .ensure_capacity(num_in as usize, num_out as usize, max_frames.max(1024));
            {
                let mut plugin = enter_plugin(&self.plugin);
                if plugin.prepare(&config).is_err() {
                    return false;
                }
                self.pending_state
                    .set_block_interval(sample_rate, max_frames);
                self.snapshot.set_block_interval(sample_rate, max_frames);
                self.panic_latch.clear();
                self.delay.publish(plugin.latency(), plugin.tail());
            }
            audio.output_note_ids.clear();
            audio.pending_output_mutation = PendingOutputMutation::None;
            audio.prepared = true;
            true
        })
    }

    pub(crate) fn reset_impl(&self, sample_rate: f64, max_frames: u32, process_mode: i32) {
        run_extern_callback_with::<P, ()>("vst3", "reset", (), || {
            let Some(mut audio) = self.audio.try_enter() else {
                return;
            };
            if !audio.prepared {
                return;
            }
            let Some(mut plugin) = self.plugin.try_enter() else {
                return;
            };
            {
                let _rt = RtSection::enter();
                plugin.reset(
                    &AudioConfig::new(sample_rate, max_frames as usize)
                        .with_process_mode(vst3_process_mode(process_mode)),
                );
            }
            audio.output_note_ids.clear();
            audio.pending_output_mutation = PendingOutputMutation::None;
        });
    }

    pub(crate) fn set_processing_impl(
        &self,
        processing: bool,
        sample_rate: f64,
        max_frames: u32,
        process_mode: i32,
    ) {
        if processing {
            self.processing.store(true, Ordering::Release);
        } else {
            self.reset_impl(sample_rate, max_frames, process_mode);
            self.processing.store(false, Ordering::Release);
            self.pending_state.mark_audio_inactive();
            self.snapshot.mark_audio_inactive();
        }
    }

    pub(crate) fn set_active_impl(&self, active: bool) {
        run_extern_callback_with::<P, ()>("vst3", "set_active", (), || {
            self.active.store(active, Ordering::Relaxed);
            self.processing.store(false, Ordering::Release);
            if !active {
                self.pending_state.mark_audio_inactive();
                self.snapshot.mark_audio_inactive();
                let mut plugin = enter_plugin(&self.plugin);
                if apply_queued_state(&mut *plugin, &self.pending_state).is_some() {
                    plugin.republish_snapshot();
                }
            }
        });
    }

    pub(crate) fn set_persist_waker_impl(&self, wake: Option<WakerFn>) {
        run_extern_callback_with::<P, ()>("vst3", "persist_bind", (), || {
            if let Some(signal) = rebind_signal(
                &self.persist_signal,
                wake,
                Some((Arc::clone(&self.host_queue), HostSignal::PERSIST)),
            ) {
                self.params_arc.set_persist_change_signal(signal);
            }
        });
    }

    pub(crate) fn set_catalog_waker_impl(&self, wake: Option<WakerFn>) {
        run_extern_callback_with::<P, ()>("vst3", "catalog_bind", (), || {
            if let Some(signal) = rebind_signal(
                &self.catalog_signal,
                wake,
                Some((Arc::clone(&self.host_queue), HostSignal::PRESENTATION)),
            ) {
                self.params_arc.set_catalog_signal(signal);
            }
        });
    }

    pub(crate) fn track_info_changed_impl(&self, info: &Vst3TrackInfo) {
        run_extern_callback_with::<P, ()>("vst3", "track_info", (), || {
            let mut track = TrackInfo::new();
            track.name.clone_from(&info.name);
            track.color = info.color_argb.map(argb_color);
            // SAFETY: `IInfoListener::setChannelContextInfos` runs on the UI thread.
            let main = unsafe { MainThread::assume() };
            if let Some(mut hooks) = self.main_thread.try_borrow_mut(&main) {
                hooks.track_info_changed(&main, &track);
            }
        });
    }

    pub(crate) fn midi_learn_impl(&self, bus: u32, channel: u8, source: Vst3LearnSource) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "midi_learn", false, || {
            let source = match source {
                Vst3LearnSource::Midi1(controller) => {
                    vst3_midi_source(controller).map(LearnSource::Midi1)
                }
                Vst3LearnSource::Midi2 {
                    registered,
                    bank,
                    index,
                } => Some(LearnSource::Midi2(Midi2Controller {
                    kind: if registered {
                        Midi2ControllerKind::Registered
                    } else {
                        Midi2ControllerKind::Assignable
                    },
                    bank,
                    index,
                })),
            };
            let Some(source) = source else {
                return false;
            };
            // SAFETY: `IMidiLearn` / `IMidiLearn2` run on the UI thread.
            let main = unsafe { MainThread::assume() };
            self.main_thread
                .try_borrow_mut(&main)
                .is_some_and(|mut hooks| hooks.midi_learn(&main, bus, channel, source))
        })
    }
}

struct Vst3Sink<'a, P: PluginExport> {
    host_id: usize,
    inst: &'a Vst3Instance<P>,
    main: &'a MainThread,
    restart: i32,
    presentation: bool,
}

impl<P: PluginExport> HostSink for Vst3Sink<'_, P> {
    fn mark_dirty(&mut self) {
        com::mark_dirty(self.host_id);
    }

    fn request_restart(&mut self) {
        self.restart |= K_RELOAD_COMPONENT;
    }

    fn audio_ports_changed(&mut self) {
        self.restart |= K_IO_CHANGED;
    }

    fn keyswitches_changed(&mut self) {
        self.restart |= K_KEYSWITCH_CHANGED;
    }

    fn param_values_changed(&mut self) {
        self.restart |= K_PARAM_VALUES_CHANGED;
    }

    fn presentation_changed(&mut self) {
        self.presentation = true;
    }

    fn latency_changed(&mut self) {
        if self.inst.delay.commit_latency() {
            self.restart |= K_LATENCY_CHANGED;
        }
    }

    fn tail_changed(&mut self) {
        let _ = self.inst.delay.commit_tail();
    }

    fn editor_wake(&mut self) {
        self.inst.apply_pending_content_scale();
        self.inst.feedback.wake_editor();
    }

    fn flush_signals(&mut self) {
        if self.restart != 0 {
            com::mark_restart(self.host_id, self.restart);
        } else if self.presentation {
            com::check_presentation(self.host_id);
        }
    }

    fn context_menu(&mut self, target: MenuTarget, x: i32, y: i32) {
        let items = self
            .inst
            .main_thread
            .try_borrow_mut(self.main)
            .map(|hooks| hooks.menu_items(self.main, target))
            .unwrap_or_default();
        let scale = self.inst.host_scale();
        #[allow(clippy::cast_possible_truncation)]
        let hx = (f64::from(x) * scale).round() as i32;
        #[allow(clippy::cast_possible_truncation)]
        let hy = (f64::from(y) * scale).round() as i32;
        com::popup_context_menu(self.host_id, target, hx, hy, &items);
    }
}

/// Close the signal in `slot` (waiting for any running wake) and, with `wake`,
/// install one that signals `queue_signal` and invokes `wake`.
pub(crate) fn rebind_signal(
    slot: &std::sync::Mutex<Option<Arc<PersistChangeSignal>>>,
    wake: Option<WakerFn>,
    queue_signal: Option<(Arc<HostQueue>, HostSignal)>,
) -> Option<std::sync::Weak<PersistChangeSignal>> {
    let mut binding = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(old) = binding.take() {
        old.close();
    }
    let wake = wake?;
    let signal = Arc::new(PersistChangeSignal::new(move || {
        if let Some((queue, bit)) = &queue_signal {
            queue.signal(*bit);
        }
        wake();
    }));
    let weak = Arc::downgrade(&signal);
    *binding = Some(signal);
    Some(weak)
}

/// The SDK `FunctionNameType` string for `function`.
pub(crate) fn function_name(function: ParamFunction) -> Option<&'static CStr> {
    use vst3::Steinberg::Vst::FunctionNameType::{
        kCompGainReduction, kCompGainReductionMax, kCompGainReductionPeakHold,
        kCompResetGainReductionMax, kDryWetMix, kLowLatencyMode, kRandomize,
    };
    let ptr = match function {
        ParamFunction::DryWetMix => kDryWetMix,
        ParamFunction::LowLatencyMode => kLowLatencyMode,
        ParamFunction::Randomize => kRandomize,
        ParamFunction::GainReduction => kCompGainReduction,
        ParamFunction::GainReductionMax => kCompGainReductionMax,
        ParamFunction::GainReductionPeakHold => kCompGainReductionPeakHold,
        ParamFunction::ResetGainReductionMax => kCompResetGainReductionMax,
        _ => return None,
    };
    // SAFETY: SDK `FunctionNameType` constants are static NUL-terminated strings.
    (!ptr.is_null()).then(|| unsafe { CStr::from_ptr(ptr) })
}

/// A VST3 `ColorSpec` (`0xAARRGGBB`).
pub(crate) fn argb_color(argb: u32) -> HostColor {
    let [a, r, g, b] = argb.to_be_bytes();
    HostColor { r, g, b, a }
}

/// A VST3 `ControllerNumbers` value (`ivstmidicontrollers.h`).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn vst3_midi_source(controller: i16) -> Option<MidiSource> {
    match controller {
        0..=127 => Some(MidiSource::Cc(controller as u8)),
        128 => Some(MidiSource::ChannelPressure),
        129 => Some(MidiSource::PitchBend),
        130 => Some(MidiSource::ProgramChange),
        _ => None,
    }
}

/// Map a VST3 `ProcessModes` value (`kRealtime` 0, `kPrefetch` 1, `kOffline` 2).
pub(crate) fn vst3_process_mode(mode: i32) -> ProcessMode {
    match mode {
        1 => ProcessMode::Buffered,
        2 => ProcessMode::Offline,
        _ => ProcessMode::Realtime,
    }
}

/// Per-bus channel width of a declared layout (`0` when out of range).
pub(crate) fn layout_bus_channels<P: PluginExport>(
    layout_index: u32,
    is_output: bool,
    bus_index: u32,
) -> u32 {
    let (Ok(li), Ok(bi)) = (usize::try_from(layout_index), usize::try_from(bus_index)) else {
        return 0;
    };
    let layouts = P::bus_layouts();
    let Some(layout) = layouts.get(li) else {
        return 0;
    };
    let buses = if is_output {
        &layout.outputs
    } else {
        &layout.inputs
    };
    buses.get(bi).map_or(0, |b| b.channels.channel_count())
}

/// Match a host-proposed per-bus arrangement to a declared `bus_layouts()` index.
pub(crate) fn match_bus_layout_perbus<P: PluginExport>(ins: &[u32], outs: &[u32]) -> Option<u32> {
    let widths_match = |buses: &[BusConfig], want: &[u32]| {
        buses.len() == want.len()
            && buses
                .iter()
                .zip(want)
                .all(|(b, &w)| b.channels.channel_count() == w)
    };
    P::bus_layouts()
        .iter()
        .position(|l| widths_match(&l.inputs, ins) && widths_match(&l.outputs, outs))
        .and_then(|i| u32::try_from(i).ok())
}

/// Owned per-bus kind bytes (`0` = Main, `1` = Sidechain) from the first declared layout.
pub(crate) fn descriptor_bus_kinds<P: PluginExport>() -> (Vec<u8>, Vec<u8>) {
    let first = P::bus_layouts().into_iter().next().unwrap_or_default();
    let kinds = |buses: &[BusConfig]| {
        buses
            .iter()
            .map(|b| u8::from(b.kind == BusKind::Sidechain))
            .collect()
    };
    (kinds(&first.inputs), kinds(&first.outputs))
}

/// True when every declared layout shares the first layout's bus topology.
pub(crate) fn vst3_topology_consistent(layouts: &[BusLayout]) -> bool {
    let kinds = |buses: &[BusConfig]| buses.iter().map(|b| b.kind).collect::<Vec<BusKind>>();
    let Some(first) = layouts.first() else {
        return true;
    };
    let (first_in, first_out) = (kinds(&first.inputs), kinds(&first.outputs));
    layouts
        .iter()
        .all(|l| kinds(&l.inputs) == first_in && kinds(&l.outputs) == first_out)
}

// ---------------------------------------------------------------------------
// Vst3Backend implementation on Vst3Instance<P>
// ---------------------------------------------------------------------------

impl<P: PluginExport> Vst3Backend for Vst3Instance<P> {
    fn set_host_name(&self, name: &str) {
        let mut host = self
            .host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *host = host.clone().with_name(name);
    }

    fn prepare(&self, sample_rate: f64, max_frames: u32, process_mode: i32) -> bool {
        self.prepare_impl(sample_rate, max_frames, process_mode)
    }

    fn reset(&self, sample_rate: f64, max_frames: u32, process_mode: i32) {
        self.reset_impl(sample_rate, max_frames, process_mode);
    }

    fn set_active(&self, active: bool) {
        self.set_active_impl(active);
    }

    fn set_processing(
        &self,
        processing: bool,
        sample_rate: f64,
        max_frames: u32,
        process_mode: i32,
    ) {
        self.set_processing_impl(processing, sample_rate, max_frames, process_mode);
    }

    fn get_latency(&self) -> u32 {
        self.delay.latency()
    }

    fn get_tail(&self) -> u32 {
        self.delay.tail()
    }

    fn begin_input_events(&self, num_frames: u32) {
        self.begin_input_events_impl(num_frames);
    }

    fn push_input_event(&self, event: &Vst3NativeEvent, data: Option<&[u8]>) -> u32 {
        self.push_input_event_impl(event, data)
    }

    fn push_param_change(
        &self,
        id: u32,
        sample_offset: i32,
        plain: f64,
        from_offset: i32,
        from_plain: f64,
    ) -> bool {
        self.push_param_change_impl(id, sample_offset, plain, from_offset, from_plain)
    }

    unsafe fn process_f32(&self, args: &mut ProcessArgs<'_>) -> bool {
        unsafe { self.process_block::<f32>(args) }
    }

    unsafe fn process_f64(&self, args: &mut ProcessArgs<'_>) -> bool {
        unsafe { self.process_block::<f64>(args) }
    }

    fn begin_output_events(&self) {
        self.begin_output_events_impl();
    }

    fn next_output_event(&self, out: &mut Vst3NativeEvent) -> u32 {
        self.next_output_event_impl(out)
    }

    fn commit_output_event(&self) {
        self.commit_output_event_impl();
    }

    fn finish_output_events(&self, status: u32) {
        self.finish_output_events_impl(status);
    }

    fn output_param_count(&self) -> u32 {
        self.output_param_count_impl()
    }

    fn output_param(&self, index: u32) -> Option<(u32, i32, f64)> {
        self.output_param_impl(index)
    }

    fn param_get_value(&self, id: u32) -> f64 {
        self.param_get_value_impl(id)
    }

    fn param_set_value(&self, id: u32, value: f64) {
        self.param_set_value_impl(id, value);
    }

    fn param_normalize(&self, id: u32, plain: f64) -> f64 {
        self.param_normalize_impl(id, plain)
    }

    fn param_denormalize(&self, id: u32, normalized: f64) -> f64 {
        self.param_denormalize_impl(id, normalized)
    }

    fn param_format(&self, id: u32, value: f64) -> Option<String> {
        self.param_format_impl(id, value)
    }

    fn param_parse(&self, id: u32, text: &str) -> Option<f64> {
        self.param_parse_impl(id, text)
    }

    fn param_is_chunked(&self, id: u32) -> bool {
        self.param_is_chunked_impl(id)
    }

    fn param_presentation(&self, id: u32) -> Option<ParamPresentation> {
        self.param_presentation_impl(id)
    }

    fn param_presentation_revision(&self) -> u64 {
        self.param_presentation_revision_impl()
    }

    fn param_affects_presentation(&self, id: u32) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "parameter_affects_presentation", false, || {
            self.params_arc.parameter_affects_presentation(id)
        })
    }

    fn param_function_name(&self, id: u32) -> Option<&'static CStr> {
        run_extern_callback_with::<P, Option<&'static CStr>>(
            "vst3",
            "parameter_function",
            None,
            || {
                self.params_arc
                    .parameter_function(id)
                    .and_then(function_name)
            },
        )
    }

    fn param_step_count(&self, id: u32) -> Option<(i32, Option<f64>)> {
        self.param_step_count_impl(id)
    }

    fn state_load_stream(&self, read: &mut dyn FnMut(&mut [u8]) -> i64) -> bool {
        self.state_load_stream_impl(read)
    }

    fn state_load_stream_with_context(
        &self,
        context: state::StateContext,
        read: &mut dyn FnMut(&mut [u8]) -> i64,
    ) -> bool {
        self.state_load_stream_with_context_impl(context, read)
    }

    fn state_save_stream(&self, write: &mut dyn FnMut(&[u8]) -> i64) -> bool {
        self.state_save_stream_impl(write)
    }

    fn state_save_stream_with_context(
        &self,
        context: state::StateContext,
        write: &mut dyn FnMut(&[u8]) -> i64,
    ) -> bool {
        self.state_save_stream_with_context_impl(context, write)
    }

    fn gui_has_editor(&self) -> bool {
        self.gui_has_editor_impl()
    }

    fn gui_get_size(&self) -> (u32, u32) {
        self.gui_get_size_impl()
    }

    unsafe fn gui_open(&self, parent: *mut c_void) -> bool {
        self.gui_open_impl(parent)
    }

    fn gui_close(&self) {
        self.gui_close_impl();
    }

    fn gui_set_content_scale(&self, scale: f64) {
        self.gui_set_content_scale_impl(scale);
    }

    fn gui_can_resize(&self) -> bool {
        self.gui_can_resize_impl()
    }

    fn gui_check_size_constraint(&self, w: u32, h: u32) -> (u32, u32) {
        self.gui_check_size_constraint_impl(w, h)
    }

    fn gui_set_size(&self, w: u32, h: u32) {
        self.gui_set_size_impl(w, h);
    }

    fn gui_param_at(&self, x: i32, y: i32) -> Option<u32> {
        self.gui_param_at_impl(x, y)
    }

    #[cfg(target_os = "linux")]
    fn gui_supports_wayland(&self) -> bool {
        self.gui_supports_wayland_impl()
    }

    #[cfg(target_os = "linux")]
    unsafe fn gui_open_wayland(&self, display: *mut c_void, surface: *mut c_void) -> bool {
        self.gui_open_wayland_impl(display, surface)
    }

    #[cfg(target_os = "linux")]
    fn gui_event_fd(&self) -> Option<i32> {
        self.gui_event_fd_impl()
    }

    fn gui_on_fd(&self) {
        self.gui_on_fd_impl();
    }

    #[cfg(target_os = "linux")]
    fn gui_on_timer(&self) {
        self.gui_on_timer_impl();
    }

    fn gui_wayland_set_sync(&self, sync: bool) {
        self.gui_wayland_set_sync_impl(sync);
    }

    fn midi_mapping_get_param_id(
        &self,
        bus_index: i32,
        channel: i16,
        controller: i16,
    ) -> Option<u32> {
        self.midi_mapping_get_param_id_impl(bus_index, channel, controller)
    }

    fn midi2_controller_assignments(
        &self,
        direction: BusDirection,
    ) -> Vec<Midi2ControllerAssignment> {
        self.midi2_controller_assignments_impl(direction)
    }

    fn wants_persist_notifications(&self) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "persist_wants", false, || {
            self.params_arc.wants_persist_notifications()
        })
    }

    fn set_persist_waker(&self, wake: Option<WakerFn>) {
        self.set_persist_waker_impl(wake);
    }

    fn take_persist_change(&self) -> Option<(u32, f64)> {
        run_extern_callback_with::<P, Option<(u32, f64)>>("vst3", "persist_drain", None, || {
            let (changed_id, normalized) = self.params_arc.take_persist_change()?;
            if !normalized.is_finite()
                || !(0.0..=1.0).contains(&normalized)
                || self.params_arc.get_normalized(changed_id).is_none()
            {
                return None;
            }
            let clamped = self
                .params_arc
                .set_normalized_returning_normalized(changed_id, normalized);
            Some((changed_id, clamped))
        })
    }

    fn set_catalog_waker(&self, wake: Option<WakerFn>) {
        self.set_catalog_waker_impl(wake);
    }

    fn take_catalog_changes(&self) -> u8 {
        run_extern_callback_with::<P, u8>("vst3", "catalog_changes", 0, || {
            self.params_arc.take_catalog_changes().flags.bits()
        })
    }

    fn track_info_changed(&self, info: &Vst3TrackInfo) {
        self.track_info_changed_impl(info);
    }

    fn midi_learn(&self, bus: u32, channel: u8, source: Vst3LearnSource) -> bool {
        self.midi_learn_impl(bus, channel, source)
    }

    fn remap_param_id(&self, cid: [u8; 16], param_id: u32) -> Option<u32> {
        run_extern_callback_with::<P, Option<u32>>("vst3", "remap_param_id", None, || {
            // SAFETY: `IRemapParamID::getCompatibleParamID` runs on the VST3 UI/main thread.
            let main = unsafe { MainThread::assume() };
            self.main_thread
                .try_borrow_mut(&main)
                .and_then(|hooks| hooks.remap_param_id(&main, cid, param_id))
        })
    }

    fn set_audio_presentation_latency(&self, is_input: bool, bus_index: u32, samples: u32) {
        run_extern_callback_with::<P, ()>("vst3", "audio_presentation_latency", (), || {
            let direction = if is_input {
                BusDirection::Input
            } else {
                BusDirection::Output
            };
            // SAFETY: `IAudioPresentationLatency` runs on the host main thread.
            let main = unsafe { MainThread::assume() };
            if let Some(mut hooks) = self.main_thread.try_borrow_mut(&main) {
                hooks.audio_presentation_latency_changed(&main, direction, bus_index, samples);
            }
        });
    }

    unsafe fn bind_host_queue(&self, host_id: usize, host: Option<*const Host>) {
        run_extern_callback_with::<P, ()>("vst3", "bind_host_queue", (), || {
            self.host_id.store(host_id, Ordering::Release);
            let ptr = host.map_or(std::ptr::null_mut(), <*const Host>::cast_mut);
            self.host_wake_target.store(ptr, Ordering::Release);
            if ptr.is_null() {
                self.host_queue.close();
            }
        });
    }

    fn drain_host_queue(&self) {
        self.drain_host_queue_impl();
    }

    fn perform_menu(&self, target: MenuTarget, action: u32) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "perform_menu", false, || {
            // SAFETY: `IContextMenuTarget::executeMenuItem` is a UI-thread call.
            let main = unsafe { MainThread::assume() };
            self.main_thread
                .try_borrow_mut(&main)
                .is_some_and(|mut hooks| hooks.perform_menu(&main, target, action))
        })
    }

    fn keyswitches(&self, bus: u32, channel: u8) -> Vec<Keyswitch> {
        run_extern_callback_with::<P, Vec<Keyswitch>>("vst3", "keyswitches", Vec::new(), || {
            // SAFETY: `IKeyswitchController` callbacks run on the host UI thread.
            let main = unsafe { MainThread::assume() };
            self.main_thread
                .try_borrow_mut(&main)
                .map(|hooks| hooks.keyswitches(&main, bus, channel))
                .unwrap_or_default()
        })
    }

    fn program_list(&self) -> Vec<ProgramPreset> {
        run_extern_callback_with::<P, Vec<ProgramPreset>>(
            "vst3",
            "program_list",
            Vec::new(),
            || {
                // SAFETY: `IUnitInfo` / `IProgramListData` callbacks run on the host UI thread.
                let main = unsafe { MainThread::assume() };
                self.main_thread
                    .try_borrow_mut(&main)
                    .map(|hooks| hooks.program_list(&main))
                    .unwrap_or_default()
            },
        )
    }

    fn set_program_data(&self, index: usize, data: &[u8]) -> bool {
        run_extern_callback_with::<P, bool>("vst3", "set_program_data", false, || {
            // SAFETY: `IProgramListData::setProgramData` runs on the host UI thread.
            let main = unsafe { MainThread::assume() };
            self.main_thread
                .try_borrow_mut(&main)
                .is_some_and(|mut hooks| hooks.set_program_data(&main, index, data))
        })
    }
}

/// Identity for test-built instances (no host context).
#[cfg(test)]
pub(crate) fn vst3_test_host() -> HostIdentity {
    HostIdentity::new(moose_core::info::HostFormat::Vst3)
}
