//! Typed pure-Rust backend boundary between generic `Vst3Instance<P>` and the
//! VST3 COM objects in `com/`.
//!
//! Replaces the legacy C-vtable `Vst3Callbacks` / `libc::malloc` indirection
//! with object-safe Rust traits and owned descriptors. Every audio-thread method
//! on [`Vst3Backend`] operates on borrowed stack data with zero heap allocations.

use std::ffi::{CStr, c_void};

use moose_core::main_thread::{
    BusDirection, Keyswitch, MenuTarget, Midi2ControllerAssignment, ProgramPreset,
};
use moose_core::state::StateContext;

/// Owned plugin descriptor handed to the COM factory at registration.
#[derive(Clone, Debug, Default)]
pub(crate) struct PluginDescriptor {
    pub name: String,
    pub vendor: String,
    pub url: String,
    pub email: String,
    pub version: String,
    /// VST3 class ID (16 bytes).
    pub cid: [u8; 16],
    /// `"Audio Module Class"` for processors.
    pub category: String,
    /// Subcategories like `"Fx"` or `"Instrument|Synth"`.
    pub subcategories: String,
    /// Summed channel counts across all buses of the first layout.
    pub num_inputs: u32,
    pub num_outputs: u32,
    /// Per-bus role (`0` = Main, `1` = Sidechain/Aux).
    pub input_bus_kinds: Vec<u8>,
    pub output_bus_kinds: Vec<u8>,
    /// Number of MIDI output ports (event output buses).
    pub midi_output_ports: i32,
    /// Number of MIDI input ports (event input buses).
    pub midi_input_ports: i32,
    /// True when the plugin's `Sample` is `f64`.
    pub supports_f64: bool,
    /// Widest total input / output channel counts across all declared layouts.
    pub max_inputs: u32,
    pub max_outputs: u32,
    /// Optional UTF-8 JSON for VST3 3.7.5+ `IPluginCompatibility`.
    pub compatibility_json: Option<String>,
}

impl PluginDescriptor {
    #[inline]
    pub(crate) fn num_input_buses(&self) -> u32 {
        u32::try_from(self.input_bus_kinds.len()).unwrap_or(u32::MAX)
    }

    #[inline]
    pub(crate) fn num_output_buses(&self) -> u32 {
        u32::try_from(self.output_bus_kinds.len()).unwrap_or(u32::MAX)
    }
}

/// Owned parameter descriptor for COM enumeration and unit-tree building.
#[derive(Clone, Debug, Default)]
pub(crate) struct ParamDescriptor {
    pub id: u32,
    pub name: String,
    pub short_name: String,
    pub units: String,
    pub default_normalized: f64,
    pub step_count: i32,
    pub flags: i32,
    pub group: String,
}

/// Runtime presentation override returned by [`Vst3Backend::param_presentation`].
#[derive(Clone, Debug)]
pub(crate) struct ParamPresentation {
    pub name: String,
    pub hidden: bool,
    pub unavailable: bool,
}

/// Stack-allocated representation of one VST3 native event transferred between
/// `com/component.rs` and `Vst3Instance<P>`.
#[derive(Copy, Clone, Default)]
pub(crate) struct Vst3NativeEvent {
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

/// The channel the plugin sits on, from `IInfoListener`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Vst3TrackInfo {
    pub name: Option<String>,
    /// `0xAARRGGBB`, when the host provided a channel color.
    pub color_argb: Option<u32>,
}

/// A controller from MIDI learn (`IMidiLearn` / `IMidiLearn2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Vst3LearnSource {
    /// A VST3 `ControllerNumbers` value (`IMidiLearn`).
    Midi1(i16),
    /// A MIDI 2 controller (`IMidiLearn2`); `bank` and `index` are 7-bit.
    Midi2 {
        registered: bool,
        bank: u8,
        index: u8,
    },
}

/// Transport info for one audio block (stack-allocated, audio-thread safe).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Vst3Transport {
    pub playing: bool,
    pub recording: bool,
    pub tempo: f64,
    pub time_sig_num: i32,
    pub time_sig_den: i32,
    pub position_samples: f64,
    pub position_beats: f64,
    pub bar_start_beats: f64,
    pub cycle_start_beats: f64,
    pub cycle_end_beats: f64,
    pub cycle_active: bool,
}

/// Stack-borrowed audio process arguments passed to [`Vst3Backend::process_f32`]
/// and [`Vst3Backend::process_f64`].
pub(crate) struct ProcessArgs<'a> {
    pub inputs: &'a [*const c_void],
    pub outputs: &'a mut [*mut c_void],
    pub in_bus_channels: &'a [u32],
    pub in_bus_active: u32,
    pub out_bus_channels: &'a [u32],
    pub out_bus_active: u32,
    pub num_frames: u32,
    pub transport: Option<&'a Vst3Transport>,
    pub process_mode: i32,
}

/// UI-thread wakeup callback bound for `PersistChangeSignal` / `CatalogSignal`.
pub(crate) type WakerFn = Box<dyn Fn() + Send + Sync>;

/// Factory-level plugin metadata and instantiation hooks.
pub(crate) trait Vst3FactoryBackend: Send + Sync + 'static {
    /// Instantiate a new plugin backend, or `None` if construction failed/panicked.
    fn create(&self) -> Option<Box<dyn Vst3Backend>>;

    /// Per-bus channel width of a declared layout, or `0` when out of range.
    fn layout_bus_channels(&self, _layout_index: u32, _is_output: bool, _bus_index: u32) -> u32 {
        0
    }

    /// Match a host-proposed per-bus arrangement to a declared `bus_layouts()` index.
    fn match_bus_layout_perbus(&self, _in_channels: &[u32], _out_channels: &[u32]) -> Option<u32> {
        Some(0)
    }
}

/// Per-instance plugin backend called directly by the COM layer (`Host`,
/// `Component`, `View`, Wayland hooks).
///
/// Default method bodies provide safe no-op behavior so unit tests in
/// `com::native_tests` can override only the methods relevant to each scenario.
pub(crate) trait Vst3Backend: Send + Sync + 'static {
    // --- Lifecycle ---

    fn prepare(&self, _sample_rate: f64, _max_frames: u32, _process_mode: i32) -> bool {
        true
    }

    fn reset(&self, _sample_rate: f64, _max_frames: u32, _process_mode: i32) {}

    fn set_active(&self, _active: bool) {}

    fn set_processing(
        &self,
        _processing: bool,
        _sample_rate: f64,
        _max_frames: u32,
        _process_mode: i32,
    ) {
    }

    fn get_latency(&self) -> u32 {
        0
    }

    fn get_tail(&self) -> u32 {
        0
    }

    // --- Audio & Events (RT-safe: zero allocations) ---

    fn begin_input_events(&self, _num_frames: u32) {}

    fn push_input_event(&self, _event: &Vst3NativeEvent, _data: Option<&[u8]>) -> u32 {
        0
    }

    fn push_param_change(
        &self,
        _id: u32,
        _sample_offset: i32,
        _plain: f64,
        _from_offset: i32,
        _from_plain: f64,
    ) -> bool {
        true
    }

    /// Render one 32-bit float block.
    ///
    /// # Safety
    /// Every non-null channel pointer in `args.inputs` / `args.outputs` must be
    /// valid for `args.num_frames` `f32` samples.
    unsafe fn process_f32(&self, _args: &mut ProcessArgs<'_>) -> bool {
        true
    }

    /// Render one 64-bit float block.
    ///
    /// # Safety
    /// Every non-null channel pointer in `args.inputs` / `args.outputs` must be
    /// valid for `args.num_frames` `f64` samples.
    unsafe fn process_f64(&self, _args: &mut ProcessArgs<'_>) -> bool {
        true
    }

    fn begin_output_events(&self) {}

    fn next_output_event(&self, _out: &mut Vst3NativeEvent) -> u32 {
        0
    }

    fn commit_output_event(&self) {}

    fn finish_output_events(&self, _status: u32) {}

    fn output_param_count(&self) -> u32 {
        0
    }

    fn output_param(&self, _index: u32) -> Option<(u32, i32, f64)> {
        None
    }

    // --- Parameters ---

    fn param_get_value(&self, _id: u32) -> f64 {
        0.0
    }

    fn param_set_value(&self, _id: u32, _value: f64) {}

    fn param_normalize(&self, _id: u32, plain: f64) -> f64 {
        plain
    }

    fn param_denormalize(&self, _id: u32, normalized: f64) -> f64 {
        normalized
    }

    fn param_format(&self, _id: u32, _value: f64) -> Option<String> {
        None
    }

    fn param_parse(&self, _id: u32, _text: &str) -> Option<f64> {
        None
    }

    fn param_is_chunked(&self, _id: u32) -> bool {
        false
    }

    fn param_presentation(&self, _id: u32) -> Option<ParamPresentation> {
        None
    }

    fn param_presentation_revision(&self) -> u64 {
        0
    }

    fn param_affects_presentation(&self, _id: u32) -> bool {
        false
    }

    fn param_function_name(&self, _id: u32) -> Option<&'static CStr> {
        None
    }

    /// Live `(step_count, optional_default_normalized)` for dynamic ranges.
    fn param_step_count(&self, _id: u32) -> Option<(i32, Option<f64>)> {
        None
    }

    // --- State (direct Rust streaming; zero libc::malloc/free copies) ---

    fn state_load_stream(&self, _read: &mut dyn FnMut(&mut [u8]) -> i64) -> bool {
        false
    }

    fn state_load_stream_with_context(
        &self,
        _context: StateContext,
        read: &mut dyn FnMut(&mut [u8]) -> i64,
    ) -> bool {
        self.state_load_stream(read)
    }

    fn state_save_stream(&self, _write: &mut dyn FnMut(&[u8]) -> i64) -> bool {
        false
    }

    fn state_save_stream_with_context(
        &self,
        _context: StateContext,
        write: &mut dyn FnMut(&[u8]) -> i64,
    ) -> bool {
        self.state_save_stream(write)
    }

    // --- GUI ---

    fn gui_has_editor(&self) -> bool {
        false
    }

    fn gui_get_size(&self) -> (u32, u32) {
        (0, 0)
    }

    /// # Safety
    /// `parent` is a valid platform window handle for the duration of the editor session.
    unsafe fn gui_open(&self, _parent: *mut c_void) -> bool {
        true
    }

    fn gui_close(&self) {}

    fn gui_set_content_scale(&self, _scale: f64) {}

    fn gui_can_resize(&self) -> bool {
        false
    }

    fn gui_check_size_constraint(&self, w: u32, h: u32) -> (u32, u32) {
        (w, h)
    }

    fn gui_set_size(&self, _w: u32, _h: u32) {}

    fn gui_param_at(&self, _x: i32, _y: i32) -> Option<u32> {
        None
    }

    #[cfg(target_os = "linux")]
    fn gui_supports_wayland(&self) -> bool {
        false
    }

    /// # Safety
    /// `display` and `surface` are valid Wayland pointers until `gui_close` returns.
    #[cfg(target_os = "linux")]
    unsafe fn gui_open_wayland(&self, _display: *mut c_void, _surface: *mut c_void) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    fn gui_event_fd(&self) -> Option<i32> {
        None
    }

    fn gui_on_fd(&self) {}

    #[cfg(target_os = "linux")]
    fn gui_on_timer(&self) {}

    fn gui_wayland_set_sync(&self, _sync: bool) {}

    // --- MIDI, Persistence & Main-Thread Hooks ---

    fn midi_mapping_get_param_id(
        &self,
        _bus_index: i32,
        _channel: i16,
        _controller: i16,
    ) -> Option<u32> {
        None
    }

    fn midi2_controller_assignments(
        &self,
        _direction: BusDirection,
    ) -> Vec<Midi2ControllerAssignment> {
        Vec::new()
    }

    fn wants_persist_notifications(&self) -> bool {
        false
    }

    fn set_persist_waker(&self, _wake: Option<WakerFn>) {}

    fn take_persist_change(&self) -> Option<(u32, f64)> {
        None
    }

    fn set_catalog_waker(&self, _wake: Option<WakerFn>) {}

    fn take_catalog_changes(&self) -> u8 {
        0
    }

    fn track_info_changed(&self, _info: &Vst3TrackInfo) {}

    fn midi_learn(&self, _bus: u32, _channel: u8, _source: Vst3LearnSource) -> bool {
        false
    }

    fn remap_param_id(&self, _old_cid: [u8; 16], _old_id: u32) -> Option<u32> {
        None
    }

    fn set_audio_presentation_latency(&self, _is_input: bool, _bus_index: u32, _samples: u32) {}

    /// Bind (`Some`) or unbind and close (`None`) the instance's `HostQueue`
    /// wake target against the owning COM `Host`.
    ///
    /// # Safety
    /// When `host` is `Some(ptr)`, `ptr` must remain valid until
    /// `bind_host_queue(0, None)` returns.
    unsafe fn bind_host_queue(&self, _host_id: usize, _host: Option<*const crate::com::Host>) {}

    fn drain_host_queue(&self) {}

    fn perform_menu(&self, _target: MenuTarget, _action: u32) -> bool {
        false
    }

    fn keyswitches(&self, _bus: u32, _channel: u8) -> Vec<Keyswitch> {
        Vec::new()
    }

    fn program_list(&self) -> Vec<ProgramPreset> {
        Vec::new()
    }

    fn set_program_data(&self, _index: usize, _data: &[u8]) -> bool {
        false
    }
}
