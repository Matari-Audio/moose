//! Modernized AAX format wrapper for moose.
//!
//! Unlike legacy dlopen-based AAX shims, `moose-aax` links statically into the
//! `.aaxplugin` native binary and exposes a single versioned C vtable
//! ([`ffi::MooseAaxCallbacks`]) plus an immutable descriptor
//! ([`ffi::MooseAaxDescriptor`]) shared with the C++ `AAX_CMonolithicParameters`
//! template via `moose-shim-types/include/aax_shim_types.h`.
//!
//! Process execution, state streaming, parameter text formatting/parsing,
//! editor gesture queuing, and single-sidechain bus routing are shared with
//! `moose-au` via [`moose_core::c_abi_driver`].

use std::any::TypeId;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Mutex, OnceLock, PoisonError};

use moose_core::bus::BusKind;
use moose_core::export::PluginExport;
use moose_core::info::{PluginCategory, PluginInfo};
use moose_params::{ParamFlags, Params};

pub mod callbacks;
pub mod ffi;
mod host_sink;
mod instance;

pub use callbacks::rt_paranoid_smoke;
pub use ffi::{
    MOOSE_AAX_ABI_MAGIC, MOOSE_AAX_ABI_VERSION, MooseAaxBusLayout, MooseAaxCallbacks,
    MooseAaxDescriptor, MooseAaxHostSink, MooseAaxNativeEvent, MooseAaxParamInfo,
    MooseAaxTransportSnapshot,
};

// AAX_EPlugInCategory constants from AAX_Enums.h.
const AAX_CATEGORY_NONE: u32 = 0x0000_0000;
const AAX_CATEGORY_EQ: u32 = 0x0000_0001;
const AAX_CATEGORY_DYNAMICS: u32 = 0x0000_0002;
const AAX_CATEGORY_PITCH_SHIFT: u32 = 0x0000_0004;
const AAX_CATEGORY_REVERB: u32 = 0x0000_0008;
const AAX_CATEGORY_DELAY: u32 = 0x0000_0010;
const AAX_CATEGORY_MODULATION: u32 = 0x0000_0020;
const AAX_CATEGORY_HARMONIC: u32 = 0x0000_0040;
const AAX_CATEGORY_NOISE_REDUCTION: u32 = 0x0000_0080;
const AAX_CATEGORY_SOUND_FIELD: u32 = 0x0000_0200;
const AAX_CATEGORY_SW_GENERATORS: u32 = 0x0000_0800;
const AAX_CATEGORY_EFFECT: u32 = 0x0000_2000;

/// Map `PluginInfo` category + optional `vst3_subcategory` to AAX's category
/// bitmask (`AAX_EPlugInCategory`).
#[must_use]
pub fn aax_category_for(info: &PluginInfo) -> u32 {
    match info.category {
        PluginCategory::Instrument => AAX_CATEGORY_SW_GENERATORS,
        PluginCategory::Analyzer => AAX_CATEGORY_NONE,
        PluginCategory::Effect | PluginCategory::NoteEffect | PluginCategory::Tool => {
            match info.vst3_subcategory {
                Some("EQ" | "Filter") => AAX_CATEGORY_EQ,
                Some("Dynamics" | "Mastering") => AAX_CATEGORY_DYNAMICS,
                Some("Pitch Shift") => AAX_CATEGORY_PITCH_SHIFT,
                Some("Reverb") => AAX_CATEGORY_REVERB,
                Some("Delay") => AAX_CATEGORY_DELAY,
                Some("Modulation") => AAX_CATEGORY_MODULATION,
                Some("Distortion") => AAX_CATEGORY_HARMONIC,
                Some("Restoration") => AAX_CATEGORY_NOISE_REDUCTION,
                Some("Surround") => AAX_CATEGORY_SOUND_FIELD,
                _ => AAX_CATEGORY_EFFECT,
            }
        }
    }
}

/// Pack a semver string `"MAJOR.MINOR.PATCH"` into AAX's 32-bit package
/// version (`(major << 16) | (minor << 8) | patch`).
#[must_use]
pub fn aax_version_packed(version: &str) -> u32 {
    let mut parts = version.split('.');
    let major = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .min(0xFFFF);
    let minor = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
        .min(0xFF);
    let patch = parts
        .next()
        .and_then(|s| {
            s.split(['-', '+'])
                .next()
                .and_then(|n| n.parse::<u32>().ok())
        })
        .unwrap_or(0)
        .min(0xFF);
    (major << 16) | (minor << 8) | patch
}

fn leak_c_string(s: &str) -> *const std::os::raw::c_char {
    let sanitized: String = s.chars().map(|c| if c == '\0' { ' ' } else { c }).collect();
    CString::new(sanitized)
        .unwrap_or_else(|_| CString::new("Moose").unwrap())
        .into_raw()
        .cast_const()
}

fn build_descriptor<P: PluginExport>() -> &'static MooseAaxDescriptor {
    let info = P::info();
    let plugin = P::create();
    let params = plugin.params_arc();
    let has_editor = u32::from((plugin.editor_builder())(std::sync::Arc::clone(&params)).is_some());

    let bypass_param_id = params
        .param_infos()
        .into_iter()
        .find(|p| p.flags.contains(ParamFlags::IS_BYPASS))
        .map_or(ffi::MOOSE_AAX_NO_BYPASS_PARAM, |p| p.id);

    let first_layout = callbacks::primary_bus_layout::<P>();
    let main_in = first_layout
        .inputs
        .first()
        .map_or(0, |b| b.channels.channel_count());
    let main_out = first_layout
        .outputs
        .first()
        .map_or(0, |b| b.channels.channel_count());
    let sidechain_in_channels = first_layout
        .inputs
        .iter()
        .skip(1)
        .find(|b| b.kind == BusKind::Sidechain)
        .map_or(0, |b| b.channels.channel_count());

    let declared_layouts = P::bus_layouts();
    let layouts_vec: Vec<MooseAaxBusLayout> = if declared_layouts.len() > 1 {
        declared_layouts
            .iter()
            .map(|l| MooseAaxBusLayout {
                in_channels: i16::try_from(
                    l.inputs.first().map_or(0, |b| b.channels.channel_count()),
                )
                .unwrap_or(2),
                out_channels: i16::try_from(
                    l.outputs.first().map_or(0, |b| b.channels.channel_count()),
                )
                .unwrap_or(2),
            })
            .collect()
    } else {
        Vec::new()
    };
    let num_layouts = u32::try_from(layouts_vec.len()).unwrap_or(0);
    let layouts_ptr = if layouts_vec.is_empty() {
        std::ptr::null()
    } else {
        layouts_vec.leak().as_ptr()
    };

    let manufacturer_id = u32::from_be_bytes(info.au_manufacturer);
    let fourcc_id = u32::from_be_bytes(info.fourcc);
    let legacy_chunks: Vec<u32> = info
        .legacy_aax_chunks
        .iter()
        .map(|code| u32::from_be_bytes(*code))
        .collect();
    let num_legacy_chunk_ids = u32::try_from(legacy_chunks.len()).unwrap_or(0);
    let legacy_chunk_ids = if legacy_chunks.is_empty() {
        std::ptr::null()
    } else {
        legacy_chunks.leak().as_ptr()
    };

    Box::leak(Box::new(MooseAaxDescriptor {
        abi_version: MOOSE_AAX_ABI_VERSION,
        _reserved: 0,
        name: leak_c_string(info.name),
        vendor: leak_c_string(info.vendor),
        plugin_id: leak_c_string(info.clap_id),
        version_packed: aax_version_packed(info.version),
        manufacturer_id,
        product_id: fourcc_id,
        plugin_type_id: fourcc_id,
        category: aax_category_for(&info),
        num_inputs: main_in,
        num_outputs: main_out,
        sidechain_in_channels,
        bypass_param_id,
        has_editor,
        wants_input_midi: u32::from(info.accepts_midi_in),
        emits_midi: u32::from(info.emits_midi),
        num_layouts,
        layouts: layouts_ptr,
        num_legacy_chunk_ids,
        legacy_chunk_ids,
    }))
}

/// Return the process-lifetime immutable [`MooseAaxDescriptor`] for `P`.
#[must_use]
pub fn descriptor<P: PluginExport + 'static>() -> &'static MooseAaxDescriptor {
    static CACHE: OnceLock<Mutex<HashMap<TypeId, &'static MooseAaxDescriptor>>> = OnceLock::new();
    let map = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(PoisonError::into_inner);
    guard
        .entry(TypeId::of::<P>())
        .or_insert_with(|| build_descriptor::<P>())
}

/// Return the immutable static [`MooseAaxCallbacks`] vtable for `P`.
#[must_use]
pub fn callbacks<P: PluginExport>() -> &'static MooseAaxCallbacks {
    &MooseAaxCallbacks {
        abi_version: MOOSE_AAX_ABI_VERSION,
        _reserved: 0,
        create_with_sink: callbacks::cb_create_with_sink::<P>,
        destroy: callbacks::cb_destroy::<P>,
        reset: callbacks::cb_reset::<P>,
        set_render_mode: callbacks::cb_set_render_mode::<P>,
        process_native: callbacks::cb_process_native::<P>,
        begin_output_events: callbacks::cb_begin_output_events::<P>,
        next_output_event: callbacks::cb_next_output_event::<P>,
        finish_output_events: callbacks::cb_finish_output_events::<P>,
        param_count: callbacks::cb_param_count::<P>,
        param_ids: callbacks::cb_param_ids::<P>,
        param_info_by_id: callbacks::cb_param_info_by_id::<P>,
        param_get: callbacks::cb_param_get::<P>,
        param_set: callbacks::cb_param_set::<P>,
        param_format: callbacks::cb_param_format::<P>,
        param_parse: callbacks::cb_param_parse::<P>,
        param_normalize: callbacks::cb_param_normalize::<P>,
        param_denormalize: callbacks::cb_param_denormalize::<P>,
        state_save: callbacks::cb_state_save::<P>,
        state_load: callbacks::cb_state_load::<P>,
        state_load_foreign: callbacks::cb_state_load_foreign::<P>,
        state_free: callbacks::cb_state_free,
        latency_samples: callbacks::cb_latency_samples::<P>,
        tail_samples: callbacks::cb_tail_samples::<P>,
        drain_host_queue: callbacks::cb_drain_host_queue::<P>,
        editor_has: callbacks::cb_editor_has::<P>,
        editor_get_size: callbacks::cb_editor_get_size::<P>,
        editor_open: callbacks::cb_editor_open::<P>,
        editor_close: callbacks::cb_editor_close::<P>,
        editor_idle: callbacks::cb_editor_idle::<P>,
    }
}

/// Export the static-linked AAX entry points (`moose_aax_abi_version`,
/// `moose_aax_descriptor`, `moose_aax_callbacks`) for `$plugin_type`.
#[macro_export]
macro_rules! export_aax {
    ($plugin_type:ty) => {
        #[doc(hidden)]
        pub mod __moose_aax_exports {
            use super::*;

            #[unsafe(no_mangle)]
            pub extern "C" fn moose_aax_abi_version() -> u32 {
                $crate::MOOSE_AAX_ABI_VERSION
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn moose_aax_descriptor() -> *const $crate::MooseAaxDescriptor {
                $crate::descriptor::<$plugin_type>()
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn moose_aax_callbacks() -> *const $crate::MooseAaxCallbacks {
                $crate::callbacks::<$plugin_type>()
            }
        }
    };
}

#[cfg(test)]
extern crate self as moose;
#[cfg(test)]
use moose_params as params;

#[cfg(test)]
mod tests {
    use std::ffi::{CStr, CString, c_void};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use moose_core::buffer::AudioBuffer;
    use moose_core::bus::BusLayout;
    use moose_core::config::AudioConfig;
    use moose_core::events::{Event, EventBody, EventList};
    use moose_core::export::PluginExport;
    use moose_core::host_queue::HostSignal;
    use moose_core::info::{Automation, MidiDialect, PluginCategory, PluginInfo};
    use moose_core::meters::MeterStore;
    use moose_core::plugin::{PluginRuntime, PreparationConfig, PreparationError};
    use moose_core::process::{ProcessContext, ProcessStatus};
    use moose_core::snapshot::SnapshotSlot;
    use moose_core::state::{ForeignState, MigratedState, PluginFormat, StateLoadError};
    use moose_derive::Params;
    use moose_params::{BoolParam, FloatParam, FloatParamReadF32, IntParam};
    use moose_shim_types::AAX_SHIM_TYPES_H;

    use super::*;
    use crate::ffi::{
        MOOSE_AAX_EVENT_EMITTED, MOOSE_AAX_EVENT_END, MOOSE_AAX_EVENT_INVALID,
        MOOSE_AAX_NATIVE_EVENT_MIDI1, MOOSE_AAX_NATIVE_EVENT_SYSEX, MOOSE_AAX_RANGE_CUSTOM,
        MOOSE_AAX_RANGE_DISCRETE, MOOSE_AAX_RANGE_LINEAR, MOOSE_AAX_RANGE_LOG,
    };

    #[derive(Params)]
    struct TestParams {
        #[param(
            id = 1,
            name = "Gain",
            unit = "custom:x",
            range = "linear(0, 2)",
            default = 0.5
        )]
        gain: FloatParam,

        #[param(
            id = 2,
            name = "Freq",
            unit = "Hz",
            range = "log(20, 20000)",
            default = 1000.0
        )]
        freq: FloatParam,

        #[param(id = 3, name = "Mode", range = "discrete(0, 3)", default = 0)]
        mode: IntParam,

        #[param(id = 4, name = "Skew", range = "skewed(0, 1, 2.0)", default = 0.5)]
        skew: FloatParam,

        #[param(id = 5, name = "Bypass", default = false, flags = "bypass")]
        bypass: BoolParam,
    }

    struct TestPlugin {
        params: Arc<TestParams>,
        meters: Arc<MeterStore>,
        snapshot: Arc<SnapshotSlot>,
        latency: u32,
        custom_tag: u8,
    }

    impl PluginRuntime for TestPlugin {
        type Sample = f32;

        fn info() -> PluginInfo {
            PluginInfo {
                name: "AAX Test Plugin",
                vendor: "Moose Test",
                url: "https://example.invalid",
                version: "2.3.4",
                category: PluginCategory::Effect,
                description: None,
                clap_manual_url: None,
                clap_support_url: None,
                clap_features: &[],
                accepts_midi_in: true,
                emits_midi: true,
                midi_input_dialect: MidiDialect::Midi1,
                midi_output_dialect: MidiDialect::Midi1,
                midi_input_ports: 1,
                midi_output_ports: 1,
                bundle_id: "aax-test",
                vst3_id: "com.moose.aax-test",
                clap_id: "com.moose.aax-test",
                fourcc: *b"AaxT",
                au_type: *b"aumf",
                au_manufacturer: *b"Moos",
                vst3_subcategory: Some("EQ"),
                preset_user_dir: None,
                preset_extension: "trucepreset",
                vst3_name: None,
                clap_name: None,
                au_name: None,
                au3_name: None,
                mute_preview_output: false,
                automation: Automation::DEFAULT,
                legacy_au_keys: &[],
                legacy_aax_chunks: const { &[*b"Juce"] },
            }
        }

        fn bus_layouts() -> Vec<BusLayout> {
            vec![BusLayout::stereo()]
        }

        fn prepare(&mut self, _config: &PreparationConfig) -> Result<(), PreparationError> {
            Ok(())
        }

        fn reset(&mut self, _config: &AudioConfig) {}

        fn process(
            &mut self,
            buffer: &mut AudioBuffer<'_, f32>,
            events: &EventList,
            ctx: &mut ProcessContext<'_>,
        ) -> ProcessStatus {
            let gain = self.params.gain.value();
            let frames = buffer.num_samples();
            for ch in 0..buffer.num_output_channels() {
                for s in 0..frames {
                    let x = buffer.input(ch)[s];
                    buffer.output(ch)[s] = x * gain;
                }
            }
            let mode = self.params.mode.value();
            if mode == 2 {
                self.latency = 256;
            }
            for idx in 0..events.len() {
                if let Some(ev) = events.get(idx) {
                    match ev.body {
                        EventBody::SysEx { .. } => {
                            let bytes = events.sysex_bytes(&ev.body);
                            let _ = ctx.output_events.push_sysex(ev.sample_offset, bytes);
                        }
                        body => {
                            let _ = ctx.output_events.try_push(Event {
                                sample_offset: ev.sample_offset,
                                port: 0,
                                body,
                            });
                        }
                    }
                }
            }
            ProcessStatus::Normal
        }

        fn latency(&self) -> u32 {
            self.latency
        }

        fn save_state(&self) -> Vec<u8> {
            vec![self.custom_tag]
        }

        fn load_state(&mut self, data: &[u8]) -> Result<(), StateLoadError> {
            if data == [0xFF] {
                return Err(StateLoadError::Malformed("rejected sentinel"));
            }
            if let Some(&b) = data.first() {
                self.custom_tag = b;
            }
            Ok(())
        }

        fn migrate_state(foreign: &ForeignState<'_>) -> Option<MigratedState> {
            match foreign {
                ForeignState::Raw {
                    format: PluginFormat::Aax,
                    source_key: Some("LGCY"),
                    bytes,
                } if bytes == b"OLD" => Some(MigratedState {
                    params: vec![(1, 1.5)],
                    extra: Some(vec![99]),
                    ..MigratedState::default()
                }),
                _ => None,
            }
        }
    }

    impl PluginExport for TestPlugin {
        type Params = TestParams;

        fn create() -> Self {
            Self {
                params: Arc::new(TestParams::default()),
                meters: MeterStore::new(),
                snapshot: SnapshotSlot::new(),
                latency: 64,
                custom_tag: 42,
            }
        }

        fn params(&self) -> &Self::Params {
            self.params.as_ref()
        }

        fn params_arc(&self) -> Arc<Self::Params> {
            Arc::clone(&self.params)
        }

        fn meter_store(&self) -> Arc<MeterStore> {
            Arc::clone(&self.meters)
        }

        fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
            Arc::clone(&self.snapshot)
        }
    }

    #[test]
    fn header_abi_constants_match_rust_ffi() {
        assert!(AAX_SHIM_TYPES_H.contains(&format!("0x{MOOSE_AAX_ABI_MAGIC:08X}u")));
        assert!(AAX_SHIM_TYPES_H.contains(&format!("0x{MOOSE_AAX_ABI_VERSION:08X}u")));
        assert!(AAX_SHIM_TYPES_H.contains("MooseAaxCallbacks"));
        assert!(AAX_SHIM_TYPES_H.contains("MooseAaxHostSink"));
        assert!(AAX_SHIM_TYPES_H.contains("moose_aax_descriptor"));
        assert!(AAX_SHIM_TYPES_H.contains("moose_aax_callbacks"));
        assert_eq!(aax_version_packed("2.3.4"), (2 << 16) | (3 << 8) | 4);
    }

    #[test]
    fn descriptor_and_live_catalog_tapers() {
        let desc = descriptor::<TestPlugin>();
        assert_eq!(desc.abi_version, MOOSE_AAX_ABI_VERSION);
        assert_eq!(desc.version_packed, (2 << 16) | (3 << 8) | 4);
        assert_eq!(desc.manufacturer_id, u32::from_be_bytes(*b"Moos"));
        assert_eq!(desc.product_id, u32::from_be_bytes(*b"AaxT"));
        assert_eq!(desc.category, AAX_CATEGORY_EQ);
        assert_eq!(desc.bypass_param_id, 5);
        assert_eq!(desc.wants_input_midi, 1);
        assert_eq!(desc.emits_midi, 1);
        assert_eq!(desc.num_legacy_chunk_ids, 1);
        assert!(!desc.legacy_chunk_ids.is_null());
        assert_eq!(
            // SAFETY: `legacy_chunk_ids` is non-null (asserted) and points to
            // `num_legacy_chunk_ids` (1) leaked ids.
            unsafe { *desc.legacy_chunk_ids },
            u32::from_be_bytes(*b"Juce")
        );

        let cbs = callbacks::<TestPlugin>();
        // SAFETY: A null sink is accepted, and `create_with_sink` has no other precondition.
        let ctx = unsafe { (cbs.create_with_sink)(std::ptr::null()) };
        assert!(!ctx.is_null());

        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        assert_eq!(unsafe { (cbs.param_count)(ctx) }, 5);
        let mut ids = [0u32; 8];
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. `ids` has room for the 8 ids requested.
        assert_eq!(unsafe { (cbs.param_ids)(ctx, ids.as_mut_ptr(), 8) }, 5);
        assert_eq!(&ids[..5], &[1, 2, 3, 4, 5]);

        let mut info = MooseAaxParamInfo::default();
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. `info` is a local out slot.
        assert_eq!(unsafe { (cbs.param_info_by_id)(ctx, 1, &raw mut info) }, 1);
        assert_eq!(info.range_type, MOOSE_AAX_RANGE_LINEAR);
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. `info` is a local out slot.
        assert_eq!(unsafe { (cbs.param_info_by_id)(ctx, 2, &raw mut info) }, 1);
        assert_eq!(info.range_type, MOOSE_AAX_RANGE_LOG);
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. `info` is a local out slot.
        assert_eq!(unsafe { (cbs.param_info_by_id)(ctx, 3, &raw mut info) }, 1);
        assert_eq!(info.range_type, MOOSE_AAX_RANGE_DISCRETE);
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. `info` is a local out slot.
        assert_eq!(unsafe { (cbs.param_info_by_id)(ctx, 4, &raw mut info) }, 1);
        assert_eq!(info.range_type, MOOSE_AAX_RANGE_CUSTOM);

        let mut buf = [0 as std::os::raw::c_char; 64];
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. `buf` holds the 64 bytes passed.
        let written = unsafe { (cbs.param_format)(ctx, 1, 1.25, buf.as_mut_ptr(), 64) };
        assert!(written > 0);
        // SAFETY: `param_format` NUL-terminated `buf` within its 64 bytes.
        let formatted = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        assert!(formatted.contains("1.25"));

        let text = CString::new("1.5").unwrap();
        let mut parsed = 0.0;
        assert_eq!(
            // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
            // destroyed once at the end. `text` is NUL-terminated and `parsed` is a local out slot.
            unsafe { (cbs.param_parse)(ctx, 1, text.as_ptr(), &raw mut parsed) },
            1
        );
        assert!((parsed - 1.5).abs() < 1e-6);

        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        let norm = unsafe { (cbs.param_normalize)(ctx, 4, 0.25) };
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        let plain = unsafe { (cbs.param_denormalize)(ctx, 4, norm) };
        assert!((plain - 0.25).abs() < 1e-6);

        // SAFETY: `ctx` was created above and is destroyed exactly once.
        unsafe { (cbs.destroy)(ctx) };
    }

    #[test]
    fn host_sink_signals_and_120_concurrent_instances() {
        #[derive(Default)]
        struct SinkLog {
            latency: AtomicU32,
            dirty_count: AtomicU32,
            released: AtomicU32,
        }

        unsafe extern "C" fn on_latency(ud: *mut c_void, samples: u32) {
            // SAFETY: `ud` is the boxed `SinkLog` registered as `user_data`; it outlives the
            // instance.
            unsafe { &*ud.cast::<SinkLog>() }
                .latency
                .store(samples, Ordering::SeqCst);
        }
        unsafe extern "C" fn on_dirty(ud: *mut c_void) {
            // SAFETY: `ud` is the boxed `SinkLog` registered as `user_data`; it outlives the
            // instance.
            unsafe { &*ud.cast::<SinkLog>() }
                .dirty_count
                .fetch_add(1, Ordering::SeqCst);
        }
        unsafe extern "C" fn on_release(ud: *mut c_void) {
            // SAFETY: `ud` is the boxed `SinkLog`, still alive when the instance releases it in
            // `destroy`.
            unsafe { &*ud.cast::<SinkLog>() }
                .released
                .fetch_add(1, Ordering::SeqCst);
        }

        let log = Box::new(SinkLog::default());
        let raw_log = std::ptr::from_ref(log.as_ref()).cast_mut().cast::<c_void>();
        let sink = MooseAaxHostSink {
            size: u32::try_from(std::mem::size_of::<MooseAaxHostSink>()).unwrap(),
            user_data: raw_log,
            release_user_data: Some(on_release),
            latency_changed: Some(on_latency),
            state_dirty: Some(on_dirty),
            ..MooseAaxHostSink::default()
        };

        let cbs = callbacks::<TestPlugin>();
        // SAFETY: `sink` is a full-size local and `log` (its `user_data`) outlives the instance.
        let ctx = unsafe { (cbs.create_with_sink)(&raw const sink) };
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        unsafe { (cbs.reset)(ctx, 48_000.0, 128) };
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        assert_eq!(unsafe { (cbs.latency_samples)(ctx) }, 64);
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. The test thread stands in for the host UI thread.
        unsafe { (cbs.drain_host_queue)(ctx) };
        assert_eq!(log.latency.load(Ordering::SeqCst), 64);

        let in_l = [1.0f32; 32];
        let in_r = [1.0f32; 32];
        let mut out_l = [0.0f32; 32];
        let mut out_r = [0.0f32; 32];
        let inputs = [in_l.as_ptr(), in_r.as_ptr()];
        let mut outputs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];

        // SAFETY: `ctx` is the live instance created above; parameter writes are atomic.
        unsafe { (cbs.param_set)(ctx, 3, 2.0) };
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. Both pointer arrays hold 2 channels of 32-sample local
        // buffers, within the 128-frame reset; events and transport are null with zero counts.
        let status = unsafe {
            (cbs.process_native)(
                ctx,
                inputs.as_ptr(),
                outputs.as_mut_ptr(),
                2,
                2,
                1,
                1,
                32,
                std::ptr::null(),
                0,
                MOOSE_AAX_EVENT_END,
                std::ptr::null(),
            )
        };
        assert_eq!(status, MOOSE_AAX_EVENT_END);
        assert!((out_l[0] - 0.5).abs() < 1e-6);

        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. The test thread stands in for the host UI thread.
        unsafe { (cbs.drain_host_queue)(ctx) };
        assert_eq!(log.latency.load(Ordering::SeqCst), 256);
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        assert_eq!(unsafe { (cbs.latency_samples)(ctx) }, 256);

        // Signal MARK_DIRTY on the instance HostQueue and verify TimerWakeup drain delivers state_dirty.
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. Only a shared reference to the instance is formed.
        unsafe {
            let inst = &*ctx.cast::<crate::instance::AaxInstance<TestPlugin>>();
            inst.host_queue.signal(HostSignal::MARK_DIRTY);
            (cbs.drain_host_queue)(ctx);
        }
        assert_eq!(log.dirty_count.load(Ordering::SeqCst), 1);

        // SAFETY: `ctx` is destroyed exactly once; `log` is dropped only after its release callback
        // ran.
        unsafe { (cbs.destroy)(ctx) };
        assert_eq!(log.released.load(Ordering::SeqCst), 1);
        drop(log);

        let instances: Vec<*mut c_void> = (0..120)
            // SAFETY: A null sink is accepted, and `create_with_sink` has no other precondition.
            .map(|_| unsafe { (cbs.create_with_sink)(std::ptr::null()) })
            .collect();
        for &c in &instances {
            assert!(!c.is_null());
            // SAFETY: Each `c` is a distinct live instance from above, destroyed exactly once.
            unsafe { (cbs.destroy)(c) };
        }
    }

    #[test]
    fn state_round_trip_foreign_migration_and_midi_sysex_echo() {
        let cbs = callbacks::<TestPlugin>();
        // SAFETY: A null sink is accepted, and `create_with_sink` has no other precondition.
        let ctx = unsafe { (cbs.create_with_sink)(std::ptr::null()) };
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        unsafe { (cbs.reset)(ctx, 48_000.0, 128) };

        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        unsafe { (cbs.param_set)(ctx, 1, 1.75) };
        let mut ptr: *mut u8 = std::ptr::null_mut();
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. `ptr` is a local out slot.
        let len = unsafe { (cbs.state_save)(ctx, &raw mut ptr) };
        assert!(len > 0 && !ptr.is_null());
        // SAFETY: `state_save` returned a non-null buffer of `len` bytes, copied here before it is
        // freed.
        let saved = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
        // SAFETY: `ptr`/`len` are the unmodified `state_save` buffer, freed once.
        unsafe { (cbs.state_free)(ptr, len) };

        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        unsafe { (cbs.param_set)(ctx, 1, 0.25) };
        assert_eq!(
            // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
            // destroyed once at the end. `saved` holds the bytes passed.
            unsafe { (cbs.state_load)(ctx, saved.as_ptr(), u32::try_from(saved.len()).unwrap()) },
            1
        );
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        assert!((unsafe { (cbs.param_get)(ctx, 1) } - 1.75).abs() < 1e-5);

        let garbage = b"not-a-valid-moose-state";
        assert_eq!(
            // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
            // destroyed once at the end. `garbage` holds the bytes passed.
            unsafe {
                (cbs.state_load)(ctx, garbage.as_ptr(), u32::try_from(garbage.len()).unwrap())
            },
            0
        );
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        assert!((unsafe { (cbs.param_get)(ctx, 1) } - 1.75).abs() < 1e-5);

        let legacy_chunk_id = u32::from_be_bytes(*b"LGCY");
        assert_eq!(
            // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
            // destroyed once at the end. The literal holds the 3 bytes passed.
            unsafe { (cbs.state_load_foreign)(ctx, legacy_chunk_id, b"OLD".as_ptr(), 3) },
            1
        );
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        assert!((unsafe { (cbs.param_get)(ctx, 1) } - 1.5).abs() < 1e-5);

        let sysex_payload = [0x7E_u8, 0x7F, 0x06, 0x01];
        let in_events = [
            MooseAaxNativeEvent {
                sample_offset: 2,
                kind: MOOSE_AAX_NATIVE_EVENT_MIDI1,
                data_len: 3,
                midi: [0x90, 60, 100, 0],
                sysex: std::ptr::null(),
            },
            MooseAaxNativeEvent {
                sample_offset: 5,
                kind: MOOSE_AAX_NATIVE_EVENT_SYSEX,
                data_len: 4,
                midi: [0; 4],
                sysex: sysex_payload.as_ptr(),
            },
        ];
        let in_l = [0.0f32; 32];
        let in_r = [0.0f32; 32];
        let mut out_l = [0.0f32; 32];
        let mut out_r = [0.0f32; 32];
        let inputs = [in_l.as_ptr(), in_r.as_ptr()];
        let mut outputs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];

        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. Both pointer arrays hold 2 channels of 32-sample buffers, and
        // `in_events` holds the 2 events passed (its `SysEx` points to `sysex_payload`, 4 bytes).
        let st = unsafe {
            (cbs.process_native)(
                ctx,
                inputs.as_ptr(),
                outputs.as_mut_ptr(),
                2,
                2,
                1,
                1,
                32,
                in_events.as_ptr(),
                2,
                MOOSE_AAX_EVENT_END,
                std::ptr::null(),
            )
        };
        assert_eq!(st, MOOSE_AAX_EVENT_END);

        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        unsafe { (cbs.begin_output_events)(ctx, 32) };
        let mut out_ev = MooseAaxNativeEvent::default();
        assert_eq!(
            // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
            // destroyed once at the end. `out_ev` is a local out slot.
            unsafe { (cbs.next_output_event)(ctx, &raw mut out_ev) },
            MOOSE_AAX_EVENT_EMITTED
        );
        assert_eq!(out_ev.kind, MOOSE_AAX_NATIVE_EVENT_MIDI1);
        assert_eq!(out_ev.sample_offset, 2);
        assert_eq!(&out_ev.midi[..3], &[0x90, 60, 100]);

        assert_eq!(
            // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
            // destroyed once at the end. `out_ev` is a local out slot.
            unsafe { (cbs.next_output_event)(ctx, &raw mut out_ev) },
            MOOSE_AAX_EVENT_EMITTED
        );
        assert_eq!(out_ev.kind, MOOSE_AAX_NATIVE_EVENT_SYSEX);
        assert_eq!(out_ev.sample_offset, 5);
        assert_eq!(
            // SAFETY: `out_ev.sysex` points to `data_len` bytes in the instance's output list,
            // untouched until `finish_output_events`.
            unsafe { std::slice::from_raw_parts(out_ev.sysex, out_ev.data_len as usize) },
            &sysex_payload
        );
        assert_eq!(
            // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
            // destroyed once at the end. `out_ev` is a local out slot.
            unsafe { (cbs.next_output_event)(ctx, &raw mut out_ev) },
            MOOSE_AAX_EVENT_END
        );
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        unsafe { (cbs.finish_output_events)(ctx, MOOSE_AAX_EVENT_EMITTED) };

        let over_in_l = [0.0f32; 256];
        let over_in_r = [0.0f32; 256];
        let mut over_out_l = [1.0f32; 256];
        let mut over_out_r = [1.0f32; 256];
        let over_inputs = [over_in_l.as_ptr(), over_in_r.as_ptr()];
        let mut over_outputs = [over_out_l.as_mut_ptr(), over_out_r.as_mut_ptr()];
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. The 256-frame buffers exceed the 128-frame reset on purpose;
        // the callback must reject them and only zero the outputs.
        let bad = unsafe {
            (cbs.process_native)(
                ctx,
                over_inputs.as_ptr(),
                over_outputs.as_mut_ptr(),
                2,
                2,
                1,
                1,
                256,
                std::ptr::null(),
                0,
                MOOSE_AAX_EVENT_END,
                std::ptr::null(),
            )
        };
        assert_eq!(bad, MOOSE_AAX_EVENT_INVALID);
        assert!(over_out_l.iter().all(|&s| s == 0.0));
        assert!(over_out_r.iter().all(|&s| s == 0.0));

        // SAFETY: `ctx` was created above and is destroyed exactly once.
        unsafe { (cbs.destroy)(ctx) };
    }

    #[test]
    fn rt_paranoid_smoke_runs_cleanly() {
        rt_paranoid_smoke::<TestPlugin>();
    }

    #[test]
    fn process_is_wait_free_and_silences_when_plugin_cell_is_contended() {
        let cbs = callbacks::<TestPlugin>();
        let sink = MooseAaxHostSink::default();
        // SAFETY: `sink` is a valid empty local; `create_with_sink` copies it.
        let ctx = unsafe { (cbs.create_with_sink)(&raw const sink) };
        assert!(!ctx.is_null());
        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end.
        unsafe { (cbs.reset)(ctx, 48_000.0, 64) };

        // SAFETY: `ctx` is the live instance created by `create_with_sink` in this test and is
        // destroyed once at the end. Only a shared reference is formed; the plugin cell guard is
        // dropped before `destroy`.
        let inst = unsafe { &*ctx.cast::<crate::instance::AaxInstance<TestPlugin>>() };
        let ctx_addr = ctx as usize;
        let held = inst.plugin.enter();

        let (status, out_l, out_r) = std::thread::spawn(move || {
            let ctx = ctx_addr as *mut c_void;
            let in_l = [0.5f32; 16];
            let in_r = [0.5f32; 16];
            let mut out_l = [1.0f32; 16];
            let mut out_r = [1.0f32; 16];
            let in_ptrs = [in_l.as_ptr(), in_r.as_ptr()];
            let mut out_ptrs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
            // SAFETY: `ctx` stays live: the main thread holds the plugin guard and joins this
            // thread before `destroy`. The pointer arrays reference this thread's 16-sample stereo
            // locals.
            let status = unsafe {
                (cbs.process_native)(
                    ctx,
                    in_ptrs.as_ptr(),
                    out_ptrs.as_mut_ptr(),
                    2,
                    2,
                    1,
                    1,
                    16,
                    std::ptr::null(),
                    0,
                    MOOSE_AAX_EVENT_END,
                    std::ptr::null(),
                )
            };
            (status, out_l, out_r)
        })
        .join()
        .expect("AAX audio thread must not park on overlap");

        assert_eq!(status, MOOSE_AAX_EVENT_INVALID);
        assert!(out_l.iter().all(|&s| s == 0.0));
        assert!(out_r.iter().all(|&s| s == 0.0));

        drop(held);
        // SAFETY: `ctx` was created above and is destroyed exactly once, after the render thread
        // joined.
        unsafe { (cbs.destroy)(ctx) };
    }
}
