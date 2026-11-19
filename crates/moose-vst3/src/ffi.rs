//! The boundary between the plugin-generic callbacks in `lib.rs` and the
//! VST3 COM objects in `com/`: what `register_vst3` hands over, and the
//! host notifications the plugin side sends back.

use std::ffi::c_void;
use std::mem::{align_of, offset_of, size_of};
use std::os::raw::c_char;

/// Plugin descriptor handed to the COM layer at registration.
#[repr(C)]
pub struct Vst3PluginDescriptor {
    pub name: *const c_char,
    pub vendor: *const c_char,
    pub url: *const c_char,
    pub email: *const c_char,
    pub version: *const c_char,
    /// VST3 class ID (16 bytes)
    pub cid: [u8; 16],
    /// "Audio Module Class" for processors
    pub category: *const c_char,
    /// Subcategories like "Fx" or "Instrument|Synth"
    pub subcategories: *const c_char,
    /// Summed channel counts across all buses of the first layout. Kept
    /// for the "does this side have any audio at all" guards and scratch
    /// sizing; the per-bus arrays below drive bus reporting and routing.
    pub num_inputs: u32,
    pub num_outputs: u32,
    /// Number of audio input / output buses (from the first declared
    /// layout). The bus *structure* - how many buses and each one's
    /// `BusKind` - is consistent across a plugin's layouts; only channel
    /// widths vary. The COM layer reports one VST3 bus per entry, so a
    /// sidechain input surfaces as its own `kBusType_Aux` bus instead of
    /// being summed into the main bus.
    pub num_input_buses: u32,
    pub num_output_buses: u32,
    /// Per-bus role, one byte per bus (`0` = Main, `1` = Sidechain/Aux),
    /// arrays of length `num_input_buses` / `num_output_buses`. The COM layer
    /// maps `1` to `kBusType_Aux` in `getBusInfo`.
    pub input_bus_kinds: *const u8,
    pub output_bus_kinds: *const u8,
    /// Number of MIDI output ports (event output buses). `0` disables
    /// output events entirely - the host never allocates
    /// `ProcessData::outputEvents` and the drain loop after `process()`
    /// is a no-op. The COM layer advertises this many `kEvent | kOutput`
    /// buses; the plugin routes each event to a bus via `Event::port`.
    pub midi_output_ports: i32,
    /// Number of MIDI input ports (event input buses). `0` means the
    /// plugin takes no MIDI (decoupled from `num_inputs` so an audio
    /// effect can also take MIDI). The COM layer advertises this many
    /// `kEvent | kInput` buses and stamps each event's `Event::port`
    /// from the bus it arrived on.
    pub midi_input_ports: i32,
    /// Non-zero when the plugin's `Sample` is `f64`. The COM layer then
    /// answers `canProcessSampleSize(kSample64)` with `kResultOk`,
    /// accepts a 64-bit `setupProcessing`, and routes blocks through
    /// `process_f64` so the plugin reads/writes host memory directly
    /// with no precision conversion.
    pub supports_f64: i32,
    /// Widest total input / output channel counts across *all* declared
    /// layouts. Process scratch (channel pointers, the per-channel output
    /// discard blocks) is sized from these, so any negotiable layout fits
    /// and no two output channels alias one discard block.
    pub max_inputs: u32,
    pub max_outputs: u32,
}

/// Parameter descriptor.
#[repr(C)]
pub struct Vst3ParamDescriptor {
    pub id: u32,
    pub name: *const c_char,
    pub short_name: *const c_char,
    pub units: *const c_char,
    pub min: f64,
    pub max: f64,
    pub default_normalized: f64,
    pub step_count: i32,
    pub flags: i32,
    pub group: *const c_char,
}

/// Flat, pointer-free except for borrowed `SysEx` bytes, representation of one
/// VST3 native event. The COM layer traverses the host's `IEventList` in place and
/// hands events to Rust one at a time, avoiding a fixed-size staging array.
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct Vst3NativeEvent {
    pub kind: u32,
    pub sample_offset: i32,
    pub bus_index: i32,
    pub flags: u32,
    pub channel: i16,
    pub pitch: i16,
    pub note_id: i32,
    /// Note-expression type ID, `SysEx` data type, or legacy controller ID.
    pub type_id: u32,
    pub length: i32,
    pub velocity: f32,
    pub tuning: f32,
    pub value: f64,
    pub ppq_position: f64,
    pub bytes: *const u8,
    pub len: u32,
    pub data1: u8,
    pub data2: u8,
}

const _: () = {
    assert!(size_of::<Vst3NativeEvent>() == 72);
    assert!(align_of::<Vst3NativeEvent>() == 8);
    assert!(offset_of!(Vst3NativeEvent, kind) == 0);
    assert!(offset_of!(Vst3NativeEvent, sample_offset) == 4);
    assert!(offset_of!(Vst3NativeEvent, bus_index) == 8);
    assert!(offset_of!(Vst3NativeEvent, flags) == 12);
    assert!(offset_of!(Vst3NativeEvent, channel) == 16);
    assert!(offset_of!(Vst3NativeEvent, pitch) == 18);
    assert!(offset_of!(Vst3NativeEvent, note_id) == 20);
    assert!(offset_of!(Vst3NativeEvent, type_id) == 24);
    assert!(offset_of!(Vst3NativeEvent, length) == 28);
    assert!(offset_of!(Vst3NativeEvent, velocity) == 32);
    assert!(offset_of!(Vst3NativeEvent, tuning) == 36);
    assert!(offset_of!(Vst3NativeEvent, value) == 40);
    assert!(offset_of!(Vst3NativeEvent, ppq_position) == 48);
    assert!(offset_of!(Vst3NativeEvent, bytes) == 56);
    assert!(offset_of!(Vst3NativeEvent, len) == 64);
    assert!(offset_of!(Vst3NativeEvent, data1) == 68);
    assert!(offset_of!(Vst3NativeEvent, data2) == 69);
};

/// `param_presentation` bit: the parameter is hidden.
pub const PRESENTATION_HIDDEN: i32 = 1;
/// `param_presentation` bit: dynamic-list hosts should not list it.
pub const PRESENTATION_UNAVAILABLE: i32 = 2;

/// The channel the plugin sits on, from `IInfoListener`.
#[repr(C)]
pub struct Vst3TrackInfo {
    /// UTF-8, NUL-terminated; null when the host didn't say.
    pub name: *const c_char,
    /// `0xAARRGGBB`, valid when `has_color`.
    pub color_argb: u32,
    pub has_color: bool,
}

/// Transport info for one block.
#[repr(C)]
pub struct Vst3Transport {
    pub playing: i32,
    pub recording: i32,
    pub tempo: f64,
    pub time_sig_num: i32,
    pub time_sig_den: i32,
    pub position_samples: f64,
    pub position_beats: f64,
    pub bar_start_beats: f64,
    pub cycle_start_beats: f64,
    pub cycle_end_beats: f64,
    pub cycle_active: i32,
}

/// Plugin callbacks the COM layer calls.
pub type ProcessFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    inputs: *const *const c_void,
    outputs: *mut *mut c_void,
    num_input_channels: u32,
    num_output_channels: u32,
    input_bus_channels: *const u32,
    num_input_buses: u32,
    input_bus_active: u32,
    output_bus_channels: *const u32,
    num_output_buses: u32,
    output_bus_active: u32,
    num_frames: u32,
    transport: *const Vst3Transport,
    // VST3 `ProcessData::processMode` for this block.
    process_mode: i32,
) -> u32;

#[repr(C)]
pub struct Vst3Callbacks {
    pub create: unsafe extern "C" fn() -> *mut c_void,
    pub destroy: unsafe extern "C" fn(ctx: *mut c_void),
    /// `process_mode` is the VST3 `ProcessSetup::processMode`
    /// (`kRealtime` / `kPrefetch` / `kOffline`).
    pub reset: unsafe extern "C" fn(
        ctx: *mut c_void,
        sample_rate: f64,
        max_frames: u32,
        process_mode: i32,
    ),
    pub prepare: unsafe extern "C" fn(
        ctx: *mut c_void,
        sample_rate: f64,
        max_frames: u32,
        process_mode: i32,
    ) -> i32,
    /// Render one block. `inputs` / `outputs` are channel pointers of the
    /// negotiated sample width (`f32`, or `f64` for `process_f64`).
    /// Parameter points arrived before through `push_param_change`.
    pub process: ProcessFn,
    /// 64-bit twin of `process`, used when the host negotiated `kSample64`
    /// (offered only when `Vst3PluginDescriptor::supports_f64` is set).
    pub process_f64: ProcessFn,
    /// Queue one host parameter point (plain value) for the current block.
    /// Returns 0 when the block's event list is full; the COM layer then
    /// commits the queue's final value directly.
    pub push_param_change:
        unsafe extern "C" fn(ctx: *mut c_void, id: u32, sample_offset: i32, plain: f64) -> i32,
    pub param_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub param_get_value: unsafe extern "C" fn(ctx: *mut c_void, id: u32) -> f64,
    pub param_set_value: unsafe extern "C" fn(ctx: *mut c_void, id: u32, value: f64),
    pub param_normalize: unsafe extern "C" fn(ctx: *mut c_void, id: u32, plain: f64) -> f64,
    pub param_denormalize: unsafe extern "C" fn(ctx: *mut c_void, id: u32, normalized: f64) -> f64,
    pub param_format: unsafe extern "C" fn(
        ctx: *mut c_void,
        id: u32,
        value: f64,
        out: *mut c_char,
        out_len: u32,
    ) -> u32,
    /// Parse host-entered UTF-8 `text` into a plain param value. Returns
    /// `1` and writes `out_plain` on success, `0` when the text can't be
    /// parsed (the COM layer then reports `kResultFalse` to the host).
    pub param_parse: unsafe extern "C" fn(
        ctx: *mut c_void,
        id: u32,
        text: *const c_char,
        out_plain: *mut f64,
    ) -> i32,
    pub state_save:
        unsafe extern "C" fn(ctx: *mut c_void, out_data: *mut *mut u8, out_len: *mut u32),
    /// Returns `1` when the blob was accepted (moose envelope, or
    /// `migrate_state` translated it), `0` when the load failed -
    /// the COM layer's `setState` forwards that as `kResultFalse`.
    pub state_load: unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: u32) -> i32,
    /// Incremental host `IBStream` reader; no staging copy.
    pub state_load_stream: unsafe extern "C" fn(
        ctx: *mut c_void,
        stream: *mut c_void,
        read: unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> i64,
    ) -> i32,
    pub state_save_stream: unsafe extern "C" fn(
        ctx: *mut c_void,
        stream: *mut c_void,
        write: unsafe extern "C" fn(*mut c_void, *const u8, u32) -> i64,
    ) -> i32,
    pub state_free: unsafe extern "C" fn(data: *mut u8, len: u32),
    // Latency + tail
    pub get_latency: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub get_tail: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// Reset the bounded input queue for one block. Native events then arrive
    /// through `push_input_event` in the host's original `IEventList` order.
    pub begin_input_events: unsafe extern "C" fn(ctx: *mut c_void, num_frames: u32),
    /// Returns 1=emitted, 2=unsupported, 3=invalid, 4=queue full. Queue-full
    /// tells the COM layer to stop traversing so the retained prefix stays ordered.
    pub push_input_event:
        unsafe extern "C" fn(ctx: *mut c_void, event: *const Vst3NativeEvent) -> u32,
    /// Begin one globally ordered output traversal over the lossless event
    /// view. `next_output_event` uses the same result codes plus 0=end.
    pub begin_output_events: unsafe extern "C" fn(ctx: *mut c_void),
    pub next_output_event: unsafe extern "C" fn(ctx: *mut c_void, out: *mut Vst3NativeEvent) -> u32,
    /// Commit the pending note-ID lifecycle mutation only after the host
    /// accepted the event through `IEventList::addEvent`.
    pub commit_output_event: unsafe extern "C" fn(ctx: *mut c_void),
    /// Publish the completed host drain result for the next process block.
    pub finish_output_events: unsafe extern "C" fn(ctx: *mut c_void, status: u32),
    // GUI
    pub gui_has_editor: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    pub gui_get_size: unsafe extern "C" fn(ctx: *mut c_void, w: *mut u32, h: *mut u32),
    /// 1 when the editor opened; 0 fails `IPlugView::attached`.
    pub gui_open: unsafe extern "C" fn(ctx: *mut c_void, parent: *mut c_void) -> i32,
    pub gui_close: unsafe extern "C" fn(ctx: *mut c_void),
    /// Host-driven DPI notification via `IPlugViewContentScaleSupport::
    /// setContentScaleFactor`. Passed to the Rust side so the instance
    /// can remember the host scale (used for physical-pixel size
    /// reporting on Windows/Linux) and forward it to the editor.
    pub gui_set_content_scale: unsafe extern "C" fn(ctx: *mut c_void, scale: f64),
    /// `IPlugView::canResize`. Returns `1` for `kResultTrue` if the
    /// editor advertised `can_resize() == true`, else `0`.
    pub gui_can_resize: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    /// `IPlugView::checkSizeConstraint`. Clamps the requested
    /// physical width / height in place to the editor's
    /// min / max / aspect constraints. Returns `1` for `kResultOk`.
    /// The Ableton-Live behaviour (host calls this even when
    /// `canResize` is false) is handled host-side here: for fixed
    /// editors we snap to the editor's current size and still
    /// return `kResultOk`.
    pub gui_check_size_constraint: unsafe extern "C" fn(ctx: *mut c_void, w: *mut u32, h: *mut u32),
    /// `IPlugView::onSize`. Host commits a new size; delegate to
    /// `Editor::set_size`. Width / height are in physical pixels;
    /// the Rust side scales to logical using the cached host
    /// content-scale before handing to the editor.
    pub gui_set_size: unsafe extern "C" fn(ctx: *mut c_void, w: u32, h: u32),
    /// `IMidiMapping::getMidiControllerAssignment`. Given the event-input
    /// bus, MIDI channel (0..=15), and a VST3 `ControllerNumbers` value
    /// (`0..=127` CC, `kAfterTouch`/`kPitchBend`/`kCtrlProgramChange`),
    /// resolve the bound parameter from the static `midi_map` metadata.
    /// Writes the id via `out_param_id` and returns `1` (`kResultOk`)
    /// on a hit, `0` (`kResultFalse`) otherwise.
    pub midi_mapping_get_param_id: unsafe extern "C" fn(
        ctx: *mut c_void,
        bus_index: i32,
        channel: i16,
        controller: i16,
        out_param_id: *mut u32,
    ) -> i32,
    /// Process-emitted parameter output. The COM layer drains these after
    /// `process` into the host's `outputParameterChanges` queue, so
    /// hosts see parameter changes the plugin makes during processing.
    /// `value` is normalized `[0,1]`; `sample_offset` is block-relative.
    pub get_output_param_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub get_output_param: unsafe extern "C" fn(
        ctx: *mut c_void,
        index: u32,
        out_id: *mut u32,
        out_sample_offset: *mut i32,
        out_value: *mut f64,
    ),
    /// `IComponent::setActive`. `active != 0` between activate and
    /// deactivate. Lets `cb_state_load` tell whether the audio thread
    /// will drain the pending-state queue (active) or whether it must
    /// apply the custom-state blob synchronously (inactive).
    pub set_active: unsafe extern "C" fn(ctx: *mut c_void, active: i32),
    /// Return the `bus_layouts()` index whose main-bus channel counts are
    /// `(in_ch, out_ch)`, or `-1`. Static per plugin type (no `ctx`); the
    /// COM layer's `setBusArrangements` uses it to accept alternate layouts.
    pub match_bus_layout: unsafe extern "C" fn(in_ch: u32, out_ch: u32) -> i32,
    /// Per-bus channel width of a declared layout. `layout_index` indexes
    /// `bus_layouts()`, `is_output != 0` selects the output direction,
    /// `bus_index` the bus within that direction. Returns the channel
    /// count, or `0` when out of range. The COM layer reports `getBusInfo` /
    /// `getBusArrangement` per bus from this and sizes its process gather.
    pub layout_bus_channels:
        unsafe extern "C" fn(layout_index: u32, is_output: i32, bus_index: u32) -> u32,
    /// Match a host-proposed *per-bus* arrangement to a declared layout.
    /// `in_channels` / `out_channels` are arrays of per-bus channel counts
    /// (length `num_in` / `num_out`). Returns the matching `bus_layouts()`
    /// index, or `-1`. Replaces the summed `match_bus_layout` for
    /// `setBusArrangements` so a sidechain bus is matched independently.
    pub match_bus_layout_perbus: unsafe extern "C" fn(
        in_channels: *const u32,
        num_in: u32,
        out_channels: *const u32,
        num_out: u32,
    ) -> i32,
    /// Is a parameter flagged `ParamFlags::CHUNKED` (sample-accurate
    /// automation)? Non-zero when the plugin requests sub-block splitting
    /// on this id, so the COM layer defers its per-offset commits to
    /// `process_chunked` rather than pre-writing the block's end value at
    /// collection time - which would make a mid-block step audible from
    /// sample 0.
    pub param_is_chunked: unsafe extern "C" fn(ctx: *mut c_void, id: u32) -> i32,
    /// `Params::parameter_presentation`: writes the runtime title into
    /// `name` and returns [`PRESENTATION_HIDDEN`] / [`PRESENTATION_UNAVAILABLE`]
    /// bits, or -1 for "no override".
    pub param_presentation:
        unsafe extern "C" fn(ctx: *mut c_void, id: u32, name: *mut c_char, capacity: u32) -> i32,
    /// `Params::parameter_presentation_revision`.
    pub param_presentation_revision: unsafe extern "C" fn(ctx: *mut c_void) -> u64,
    pub wants_persist_notifications: unsafe extern "C" fn(*mut c_void) -> bool,
    pub set_persist_waker:
        unsafe extern "C" fn(*mut c_void, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void),
    pub take_persist_change: unsafe extern "C" fn(*mut c_void, *mut u32, *mut f64) -> bool,
    /// `Params::parameter_affects_presentation`. Any thread, audio included:
    /// must not allocate or lock.
    pub param_affects_presentation: unsafe extern "C" fn(ctx: *mut c_void, id: u32) -> bool,
    /// The SDK `FunctionNameType` string of `id`'s `ParamFunction`
    /// (a static C string), or null.
    pub param_function_name: unsafe extern "C" fn(ctx: *mut c_void, id: u32) -> *const c_char,
    /// Bind (or with `None`, unbind and join) the `CatalogSignal` wake.
    /// The wake runs on whatever thread committed the change and must only
    /// schedule the UI thread.
    pub set_catalog_waker:
        unsafe extern "C" fn(*mut c_void, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void),
    /// `Params::take_catalog_changes` as `CatalogChange` bits. UI thread.
    pub take_catalog_changes: unsafe extern "C" fn(ctx: *mut c_void) -> u8,
    /// `MainThreadHooks::track_info_changed` (`IInfoListener`). UI thread.
    pub track_info_changed: unsafe extern "C" fn(ctx: *mut c_void, info: *const Vst3TrackInfo),
    /// `MainThreadHooks::midi_learn` (`IMidiLearn`): a VST3 controller
    /// number on `bus` / `channel`. Returns 1 when the plugin bound it. UI
    /// thread.
    pub midi_learn:
        unsafe extern "C" fn(ctx: *mut c_void, bus: u32, channel: u8, controller: i16) -> i32,
    /// `IAudioProcessor::setProcessing`. `false` also resets the plugin
    /// (what the shim used to do through `reset`).
    pub set_processing: unsafe extern "C" fn(
        ctx: *mut c_void,
        processing: i32,
        sample_rate: f64,
        max_frames: u32,
        process_mode: i32,
    ),
}

/// Register the plugin with the COM layer.
///
/// # Safety
/// Every pointer must reference leaked, immutable registration data.
pub unsafe fn moose_vst3_register(
    descriptor: *const Vst3PluginDescriptor,
    callbacks: *const Vst3Callbacks,
    param_descriptors: *const Vst3ParamDescriptor,
    num_params: u32,
) {
    let params = if param_descriptors.is_null() {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(param_descriptors, num_params as usize) }
    };
    unsafe { crate::com::register(&*descriptor, &*callbacks, params) };
}

/// A new reference to the plugin factory. Called by `GetPluginFactory`.
#[must_use]
pub fn moose_vst3_get_factory() -> *mut c_void {
    crate::com::factory()
}

/// Host: begin editing a parameter (mouse-down). From any thread; calls off
/// the host UI thread are queued for it.
pub fn moose_vst3_begin_edit(ctx: *mut c_void, id: u32) {
    crate::com::begin_edit(ctx, id);
}

/// Host: parameter value changed during a gesture.
pub fn moose_vst3_perform_edit(ctx: *mut c_void, id: u32, normalized: f64) {
    crate::com::perform_edit(ctx, id, normalized);
}

/// Host: end editing a parameter (mouse-up).
pub fn moose_vst3_end_edit(ctx: *mut c_void, id: u32) {
    crate::com::end_edit(ctx, id);
}

/// Plugin -> host resize request through the live view's `IPlugFrame`.
/// Returns 1 when made or queued, 0 when no view or frame is live or the
/// host refused. Resolved through the instance each time, so a view the
/// host re-created (theme change, dock/undock) is never stale.
pub fn moose_vst3_request_resize(ctx: *mut c_void, w: u32, h: u32) -> i32 {
    crate::com::request_resize(ctx, w, h)
}

/// Flag a `restartComponent` (`RestartFlags` bitmask). Inside `process`
/// it only sets bits (RT-safe); the UI thread sends them. From the UI
/// thread it is sent at once.
pub fn moose_vst3_mark_restart(ctx: *mut c_void, flags: i32) {
    crate::com::mark_restart(ctx, flags);
}

/// Open (`Begin`) or close (`End`) a group of edits
/// (`IComponentHandler2::startGroupEdit` / `finishGroupEdit`). Any thread;
/// it is delivered on the UI thread in order with the edits.
pub fn moose_vst3_group_edit(ctx: *mut c_void, edge: moose_core::editor::EditGroup) {
    crate::com::group_edit(ctx, edge);
}

/// Tell the host the project changed (`IComponentHandler2::setDirty`), for
/// custom-state edits it can't see as parameter edits. Any thread; it is
/// delivered on the UI thread.
pub fn moose_vst3_mark_dirty(ctx: *mut c_void) {
    crate::com::mark_dirty(ctx);
}
