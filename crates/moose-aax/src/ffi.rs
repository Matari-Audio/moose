//! `#[repr(C)]` FFI mirrors of `moose-shim-types/include/aax_shim_types.h`.
//!
//! Layout and constant parity with the C header is verified by unit tests in
//! `moose-aax` against `moose_shim_types::AAX_SHIM_TYPES_H`.

#![allow(clippy::pub_underscore_fields)]

use std::ffi::c_void;
use std::os::raw::c_char;

/// ABI magic `'MAx\0'`.
pub const MOOSE_AAX_ABI_MAGIC: u32 = 0x4D41_7800;

/// Current modernized single-vtable AAX C ABI revision.
pub const MOOSE_AAX_ABI_VERSION: u32 = MOOSE_AAX_ABI_MAGIC | 1;

// Parameter taper hints.
pub const MOOSE_AAX_RANGE_LINEAR: u8 = 0;
pub const MOOSE_AAX_RANGE_LOG: u8 = 1;
pub const MOOSE_AAX_RANGE_DISCRETE: u8 = 2;
pub const MOOSE_AAX_RANGE_CUSTOM: u8 = 3;

/// Sentinel when the plugin declares no `IS_BYPASS` parameter.
pub const MOOSE_AAX_NO_BYPASS_PARAM: u32 = 0xFFFF_FFFF;

// Parameter flag bits.
pub const MOOSE_AAX_PARAM_FLAG_AUTOMATABLE: u32 = 1 << 0;
pub const MOOSE_AAX_PARAM_FLAG_READ_ONLY: u32 = 1 << 1;
pub const MOOSE_AAX_PARAM_FLAG_HIDDEN: u32 = 1 << 2;
pub const MOOSE_AAX_PARAM_FLAG_BYPASS: u32 = 1 << 3;

// Native event kinds and transactional status codes.
pub const MOOSE_AAX_NATIVE_EVENT_MIDI1: u32 = 0;
pub const MOOSE_AAX_NATIVE_EVENT_SYSEX: u32 = 1;

pub const MOOSE_AAX_EVENT_END: u32 = 0;
pub const MOOSE_AAX_EVENT_EMITTED: u32 = 1;
pub const MOOSE_AAX_EVENT_QUEUE_FULL: u32 = 2;
pub const MOOSE_AAX_EVENT_INVALID: u32 = 3;
pub const MOOSE_AAX_EVENT_UNSUPPORTED: u32 = 4;

pub const MOOSE_AAX_NATIVE_EVENT_CAP: u32 = 256;
pub const MOOSE_AAX_SYSEX_POOL_CAP: u32 = 4096;

// Editor parent view platform discriminants matching AAX_EViewContainer_Type.
pub const MOOSE_AAX_VIEW_UNKNOWN: i32 = 0;
pub const MOOSE_AAX_VIEW_NSVIEW: i32 = 1;
pub const MOOSE_AAX_VIEW_HWND: i32 = 2;

/// One declared main-bus channel width pair (`in_channels`, `out_channels`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MooseAaxBusLayout {
    pub in_channels: i16,
    pub out_channels: i16,
}

/// Immutable plugin descriptor returned by `moose_aax_descriptor()`.
#[repr(C)]
pub struct MooseAaxDescriptor {
    pub abi_version: u32,
    pub _reserved: u32,
    pub name: *const c_char,
    pub vendor: *const c_char,
    pub plugin_id: *const c_char,
    pub version_packed: u32,
    pub manufacturer_id: u32,
    pub product_id: u32,
    pub plugin_type_id: u32,
    pub category: u32,
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub sidechain_in_channels: u32,
    pub bypass_param_id: u32,
    pub has_editor: u32,
    pub wants_input_midi: u32,
    pub emits_midi: u32,
    pub num_layouts: u32,
    pub layouts: *const MooseAaxBusLayout,
    pub num_legacy_chunk_ids: u32,
    pub legacy_chunk_ids: *const u32,
}

// SAFETY: Every pointer in `MooseAaxDescriptor` references process-lifetime
// immutable metadata allocated once at descriptor initialization.
unsafe impl Send for MooseAaxDescriptor {}
// SAFETY: Shared access only reads the descriptor's integers and the process-lifetime, immutable
// arrays described above.
unsafe impl Sync for MooseAaxDescriptor {}

/// Per-parameter metadata populated from the live parameter catalog.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MooseAaxParamInfo {
    pub id: u32,
    pub name: [c_char; 128],
    pub unit: [c_char; 32],
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
    pub step_count: u32,
    pub flags: u32,
    pub range_type: u8,
    pub _pad: [u8; 3],
}

impl Default for MooseAaxParamInfo {
    fn default() -> Self {
        Self {
            id: 0,
            name: [0; 128],
            unit: [0; 32],
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.0,
            step_count: 0,
            flags: 0,
            range_type: MOOSE_AAX_RANGE_LINEAR,
            _pad: [0; 3],
        }
    }
}

/// One ordered MIDI 1.0 or `SysEx` event in a process block.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MooseAaxNativeEvent {
    pub sample_offset: u32,
    pub kind: u32,
    pub data_len: u32,
    pub midi: [u8; 4],
    pub sysex: *const u8,
}

impl Default for MooseAaxNativeEvent {
    fn default() -> Self {
        Self {
            sample_offset: 0,
            kind: MOOSE_AAX_NATIVE_EVENT_MIDI1,
            data_len: 0,
            midi: [0; 4],
            sysex: std::ptr::null(),
        }
    }
}

/// Transport snapshot captured from `AAX_ITransport` once per render block.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MooseAaxTransportSnapshot {
    pub valid: i32,
    pub playing: i32,
    pub tempo: f64,
    pub time_sig_num: i32,
    pub time_sig_den: i32,
    pub position_samples: f64,
    pub position_beats: f64,
    pub bar_start_beats: f64,
    pub loop_active: i32,
    pub _pad: i32,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
}

/// Host callback sink supplied to `create_with_sink`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MooseAaxHostSink {
    pub size: u32,
    pub _reserved: u32,
    pub user_data: *mut c_void,
    pub release_user_data: Option<unsafe extern "C" fn(*mut c_void)>,
    pub touch_param: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    pub set_param_normalized: Option<unsafe extern "C" fn(*mut c_void, u32, f64)>,
    pub release_param: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    pub latency_changed: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    pub state_dirty: Option<unsafe extern "C" fn(*mut c_void)>,
    pub param_values_changed: Option<unsafe extern "C" fn(*mut c_void)>,
    pub param_presentation_changed: Option<unsafe extern "C" fn(*mut c_void)>,
    pub request_resize: Option<unsafe extern "C" fn(*mut c_void, u32, u32) -> i32>,
}

impl Default for MooseAaxHostSink {
    fn default() -> Self {
        Self {
            size: 0,
            _reserved: 0,
            user_data: std::ptr::null_mut(),
            release_user_data: None,
            touch_param: None,
            set_param_normalized: None,
            release_param: None,
            latency_changed: None,
            state_dirty: None,
            param_values_changed: None,
            param_presentation_changed: None,
            request_resize: None,
        }
    }
}

/// Single C callback vtable returned by `moose_aax_callbacks()`.
#[repr(C)]
pub struct MooseAaxCallbacks {
    pub abi_version: u32,
    pub _reserved: u32,

    pub create_with_sink: unsafe extern "C" fn(*const MooseAaxHostSink) -> *mut c_void,
    pub destroy: unsafe extern "C" fn(*mut c_void),
    pub reset: unsafe extern "C" fn(*mut c_void, f64, u32),
    pub set_render_mode: unsafe extern "C" fn(*mut c_void, u32),

    pub process_native: unsafe extern "C" fn(
        *mut c_void,
        *const *const f32,
        *mut *mut f32,
        u32,
        u32,
        u32,
        u32,
        u32,
        *const MooseAaxNativeEvent,
        u32,
        u32,
        *const MooseAaxTransportSnapshot,
    ) -> u32,
    pub begin_output_events: unsafe extern "C" fn(*mut c_void, u32),
    pub next_output_event: unsafe extern "C" fn(*mut c_void, *mut MooseAaxNativeEvent) -> u32,
    pub finish_output_events: unsafe extern "C" fn(*mut c_void, u32),

    pub param_count: unsafe extern "C" fn(*mut c_void) -> u32,
    pub param_ids: unsafe extern "C" fn(*mut c_void, *mut u32, u32) -> u32,
    pub param_info_by_id: unsafe extern "C" fn(*mut c_void, u32, *mut MooseAaxParamInfo) -> i32,
    pub param_get: unsafe extern "C" fn(*mut c_void, u32) -> f64,
    pub param_set: unsafe extern "C" fn(*mut c_void, u32, f64),
    pub param_format: unsafe extern "C" fn(*mut c_void, u32, f64, *mut c_char, u32) -> u32,
    pub param_parse: unsafe extern "C" fn(*mut c_void, u32, *const c_char, *mut f64) -> i32,
    pub param_normalize: unsafe extern "C" fn(*mut c_void, u32, f64) -> f64,
    pub param_denormalize: unsafe extern "C" fn(*mut c_void, u32, f64) -> f64,

    pub state_save: unsafe extern "C" fn(*mut c_void, *mut *mut u8) -> u32,
    pub state_load: unsafe extern "C" fn(*mut c_void, *const u8, u32) -> i32,
    pub state_load_foreign: unsafe extern "C" fn(*mut c_void, u32, *const u8, u32) -> i32,
    pub state_free: unsafe extern "C" fn(*mut u8, u32),

    pub latency_samples: unsafe extern "C" fn(*mut c_void) -> u32,
    pub tail_samples: unsafe extern "C" fn(*mut c_void) -> u32,
    pub drain_host_queue: unsafe extern "C" fn(*mut c_void),

    pub editor_has: unsafe extern "C" fn(*mut c_void) -> i32,
    pub editor_get_size: unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32) -> i32,
    pub editor_open: unsafe extern "C" fn(*mut c_void, *mut c_void, i32) -> i32,
    pub editor_close: unsafe extern "C" fn(*mut c_void),
    pub editor_idle: unsafe extern "C" fn(*mut c_void),
}

// SAFETY: Function pointers and integer fields only; immutable static vtable.
unsafe impl Send for MooseAaxCallbacks {}
// SAFETY: Shared access only reads plain function pointers and integers that never change after
// construction.
unsafe impl Sync for MooseAaxCallbacks {}
