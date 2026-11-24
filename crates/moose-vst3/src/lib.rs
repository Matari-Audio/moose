//! VST3 format wrapper for moose.
//!
//! The COM objects (`com/`) are built on the official SDK interfaces from
//! the `vst3` crate and call directly into [`backend::Vst3Backend`]
//! implemented on [`instance::Vst3Instance`].

mod backend;
mod com;
pub mod ffi;
mod gui;
mod host_state;
mod input_events;
mod instance;
mod lifecycle;
mod midi_proxy;
mod output_events;
mod params;
mod process;
mod registration;

pub use process::rt_paranoid_smoke;
pub use registration::register_vst3;

#[cfg(test)]
use crate::{
    backend::*, input_events::*, instance::*, lifecycle::*, midi_proxy::*, output_events::*,
    params::*, registration::*,
};
#[cfg(test)]
use moose_core::config::AudioConfig;
#[cfg(test)]
use moose_core::events::EventList;
#[cfg(test)]
use moose_core::export::PluginExport;
#[cfg(test)]
use moose_core::info::PluginInfo;
#[cfg(test)]
use moose_core::main_thread::{LearnSource, MainThreadHooks, TrackInfo};
#[cfg(test)]
use moose_core::meters::MeterStore;
#[cfg(test)]
use moose_core::plugin::PluginRuntime;
#[cfg(test)]
use moose_core::snapshot::SnapshotSlot;
#[cfg(test)]
use moose_core::thread::MainThread;
#[cfg(all(test, target_os = "linux"))]
use moose_params::Params;
#[cfg(test)]
use moose_params::{MidiSource, ParamFunction, ParamInfo, ParamRange};
#[cfg(test)]
use std::sync::Arc;

use moose_core::state;

// ---------------------------------------------------------------------------
// export_vst3! macro
// ---------------------------------------------------------------------------

#[macro_export]
macro_rules! export_vst3 {
    ($plugin_type:ty) => {
        mod _vst3_entry {
            use super::*;

            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn GetPluginFactory() -> *mut ::std::ffi::c_void {
                static INIT: ::std::sync::Once = ::std::sync::Once::new();
                INIT.call_once(|| {
                    ::moose_vst3::register_vst3::<$plugin_type>();
                });
                ::moose_vst3::ffi::moose_vst3_get_factory()
            }

            #[cfg(target_os = "macos")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn BundleEntry(_: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[unsafe(no_mangle)]
            pub extern "system" fn bundleEntry(_: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn BundleExit() -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[unsafe(no_mangle)]
            pub extern "system" fn bundleExit() -> bool {
                true
            }

            #[cfg(target_os = "linux")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn ModuleEntry(_: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "linux")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn ModuleExit() -> bool {
                true
            }

            #[cfg(target_os = "windows")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn InitDll() -> bool {
                true
            }

            #[cfg(target_os = "windows")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn ExitDll() -> bool {
                true
            }
        }
    };
}

#[cfg(test)]
mod midi_proxy_tests {
    use super::{
        MIDI_PROXY_ID_BASE, MIDI_PROXY_PER_CHANNEL, MIDI_PROXY_PITCH_BEND, MIDI_PROXY_PRESSURE,
        midi_proxy_decode, midi_proxy_default, midi_proxy_event, midi_proxy_id,
    };
    use moose_core::events::EventBody;

    #[test]
    fn id_round_trips_across_the_banks() {
        for port in [0u8, 1, 3, 255] {
            for channel in 0u8..16 {
                for controller in 0..MIDI_PROXY_PER_CHANNEL {
                    let id = midi_proxy_id(port, channel, controller);
                    assert_eq!(midi_proxy_decode(id), Some((port, channel, controller)));
                }
            }
        }
    }

    #[test]
    fn ports_get_distinct_ids() {
        assert_ne!(midi_proxy_id(0, 4, 74), midi_proxy_id(1, 4, 74));
    }

    #[test]
    fn real_param_ids_never_decode() {
        const _: () = assert!(MIDI_PROXY_ID_BASE > moose_params::METER_ID_BASE);
        assert_eq!(midi_proxy_decode(0), None);
        assert_eq!(midi_proxy_decode(moose_params::METER_ID_BASE), None);
        assert_eq!(midi_proxy_decode(MIDI_PROXY_ID_BASE - 1), None);
        assert_eq!(
            midi_proxy_decode(midi_proxy_id(255, 15, MIDI_PROXY_PER_CHANNEL - 1) + 1),
            None
        );
    }

    #[test]
    fn pitch_bend_endpoints_and_center() {
        let bend = |norm: f32| match midi_proxy_event(3, MIDI_PROXY_PITCH_BEND, norm) {
            EventBody::PitchBend { channel, value, .. } => {
                assert_eq!(channel, 3);
                value
            }
            other => panic!("expected PitchBend, got {other:?}"),
        };
        assert_eq!(bend(0.0), 0);
        assert_eq!(bend(0.5), 8192);
        assert_eq!(bend(1.0), 16383);
    }

    #[test]
    fn cc_and_pressure_decode_to_their_events() {
        match midi_proxy_event(0, 74, 1.0) {
            EventBody::ControlChange { cc, value, .. } => {
                assert_eq!(cc, 74);
                assert_eq!(value, 127);
            }
            other => panic!("expected ControlChange, got {other:?}"),
        }
        match midi_proxy_event(9, MIDI_PROXY_PRESSURE, 0.0) {
            EventBody::ChannelPressure {
                channel, pressure, ..
            } => {
                assert_eq!(channel, 9);
                assert_eq!(pressure, 0);
            }
            other => panic!("expected ChannelPressure, got {other:?}"),
        }
    }

    #[test]
    fn defaults_center_only_the_wheel() {
        assert!((midi_proxy_default(MIDI_PROXY_PITCH_BEND) - 0.5).abs() < f64::EPSILON);
        assert!(midi_proxy_default(0).abs() < f64::EPSILON);
        assert!(midi_proxy_default(MIDI_PROXY_PRESSURE).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod channel_limit_tests {
    use super::{max_layout_channels, vst3_bus_widths_supported, vst3_topology_consistent};
    use moose_core::bus::{BusLayout, ChannelConfig};

    #[test]
    fn topology_consistent_when_only_widths_vary() {
        let layouts = [
            BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo),
            BusLayout::mono().with_sidechain_input("Sidechain", ChannelConfig::Mono),
        ];
        assert!(vst3_topology_consistent(&layouts));
    }

    #[test]
    fn topology_inconsistent_when_bus_count_differs() {
        let layouts = [
            BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo),
            BusLayout::mono(),
        ];
        assert!(!vst3_topology_consistent(&layouts));
    }

    #[test]
    fn topology_single_or_empty_is_consistent() {
        assert!(vst3_topology_consistent(&[BusLayout::stereo()]));
        assert!(vst3_topology_consistent(&[]));
    }

    #[test]
    fn widest_layout_wins() {
        let layouts = [BusLayout::stereo(), BusLayout::mono()];
        assert_eq!(max_layout_channels(&layouts), (2, 2));
    }

    #[test]
    fn sums_channels_across_buses() {
        let layouts =
            [BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo)];
        assert_eq!(max_layout_channels(&layouts), (4, 2));
    }

    #[test]
    fn wide_layouts_are_measured_not_capped() {
        let mut layout = BusLayout::new()
            .with_input("Main", ChannelConfig::Stereo)
            .with_output("Main", ChannelConfig::Stereo);
        for _ in 1..17 {
            layout = layout.with_sidechain_input("Aux", ChannelConfig::Stereo);
        }
        assert_eq!(max_layout_channels(&[layout]), (34, 2));
    }

    #[test]
    fn each_bus_must_fit_a_speaker_mask() {
        for (width, allowed) in [(64, true), (65, false)] {
            let layout = BusLayout::new().with_input("Main", ChannelConfig::Custom(width));
            assert_eq!(vst3_bus_widths_supported(&[layout]), allowed);
        }
        let mut layout = BusLayout::new();
        for _ in 0..17 {
            layout = layout.with_input("Bus", ChannelConfig::Stereo);
        }
        assert!(vst3_bus_widths_supported(&[layout]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moose_core::events::EventBody;
    use moose_params::{MidiSource, ParamFlags, ParamUnit, ParamValueKind};
    use std::cell::Cell;
    use vst3::Steinberg::{IBStream, IBStreamTrait, int32, int64, kResultOk, tresult};
    use vst3::{Class, ComWrapper};

    #[test]
    fn midi_proxy_text_parses_the_fallback_display() {
        assert_eq!(parse_midi_proxy_text("0.50"), Some(0.5));
        assert_eq!(parse_midi_proxy_text(" 2 "), Some(1.0));
        assert_eq!(parse_midi_proxy_text("-1"), Some(0.0));
        assert_eq!(parse_midi_proxy_text("NaN"), None);
        assert_eq!(parse_midi_proxy_text("loud"), None);
    }

    struct ScriptedStream {
        replies: Vec<(tresult, i32)>,
        calls: Cell<usize>,
    }

    impl Class for ScriptedStream {
        type Interfaces = (IBStream,);
    }

    impl IBStreamTrait for ScriptedStream {
        unsafe fn read(
            &self,
            _buffer: *mut std::ffi::c_void,
            num_bytes: int32,
            num_bytes_read: *mut int32,
        ) -> tresult {
            let i = self.calls.get();
            self.calls.set(i + 1);
            let (rc, count) = self.replies.get(i).copied().unwrap_or((kResultOk, 0));
            unsafe {
                *num_bytes_read = if count == i32::MAX { num_bytes } else { count };
            }
            rc
        }

        unsafe fn write(
            &self,
            buffer: *mut std::ffi::c_void,
            num_bytes: int32,
            num_bytes_written: *mut int32,
        ) -> tresult {
            unsafe { self.read(buffer, num_bytes, num_bytes_written) }
        }

        unsafe fn seek(&self, _pos: int64, _mode: int32, _result: *mut int64) -> tresult {
            kResultOk
        }

        unsafe fn tell(&self, _pos: *mut int64) -> tresult {
            kResultOk
        }
    }

    #[test]
    fn state_read_chunk_preserves_short_reads_and_rejects_bad_counts() {
        let stream = ComWrapper::new(ScriptedStream {
            replies: vec![(0, 317), (1, 17), (0, 0), (0, -1), (0, 8193)],
            calls: Cell::new(0),
        });
        let com_ref = stream.as_com_ref::<IBStream>().unwrap();
        let mut buf = [0_u8; 8192];
        for expected in [317, 17, 0, -1, -1] {
            assert_eq!(crate::com::read_state_chunk(com_ref, &mut buf), expected);
        }
    }

    #[test]
    fn state_write_chunk_rejects_zero_progress_and_overruns() {
        let stream = ComWrapper::new(ScriptedStream {
            replies: vec![(0, 60), (0, 0), (0, 101)],
            calls: Cell::new(0),
        });
        let com_ref = stream.as_com_ref::<IBStream>().unwrap();
        let blob = [7u8; 100];
        assert_eq!(crate::com::write_state_chunk(com_ref, &blob), 60);
        assert_eq!(crate::com::write_state_chunk(com_ref, &blob), -1);
        assert_eq!(crate::com::write_state_chunk(com_ref, &blob), -1);
    }

    fn info(range: ParamRange, midi_map: Option<MidiSource>) -> ParamInfo {
        ParamInfo {
            id: 1,
            name: "p",
            short_name: "p",
            group: "",
            range,
            default_plain: 0.0,
            flags: ParamFlags::AUTOMATABLE,
            unit: ParamUnit::None,
            kind: ParamValueKind::Float,
            midi_map,
            midi_channel: None,
        }
    }

    fn bridge(info: &ParamInfo, plain: f64) -> Option<EventBody> {
        MidiMap::from_param(info).map(|m| midi_event_from_map(&m, plain))
    }

    #[test]
    fn unmapped_param_does_not_bridge() {
        let i = info(ParamRange::Linear { min: 0.0, max: 1.0 }, None);
        assert!(bridge(&i, 0.5).is_none());
    }

    #[test]
    fn midi_map_cache_holds_only_mapped_ids_and_binary_searches() {
        let range = ParamRange::Linear {
            min: 0.0,
            max: 127.0,
        };
        let mut mapped_cc = info(range, Some(MidiSource::Cc(74)));
        mapped_cc.id = 5;
        let mut unmapped = info(range, None);
        unmapped.id = 2;
        let mut mapped_bend = info(range, Some(MidiSource::PitchBend));
        mapped_bend.id = 9;

        let mut cache: Vec<(u32, MidiMap)> = [&mapped_cc, &unmapped, &mapped_bend]
            .into_iter()
            .filter_map(|i| MidiMap::from_param(i).map(|m| (i.id, m)))
            .collect();
        cache.sort_by_key(|(id, _)| *id);

        assert_eq!(cache.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [5, 9]);

        let find = |id: u32| cache.binary_search_by_key(&id, |(i, _)| *i);
        let idx = find(5).expect("mapped id 5 present");
        assert!(matches!(
            midi_event_from_map(&cache[idx].1, 127.0),
            EventBody::ControlChange { cc: 74, .. }
        ));
        assert!(find(9).is_ok(), "mapped id 9 present");
        assert!(find(2).is_err(), "unmapped id absent");
        assert!(find(999).is_err(), "unknown id absent");
    }

    #[test]
    fn note_expression_maps_per_note_cc_and_bend() {
        let (type_id, note_id, value) = note_expression_of(&EventBody::PerNoteCC {
            group: 0,
            channel: 2,
            note: 60,
            cc: 7,
            value: u32::MAX,
            registered: true,
        })
        .expect("volume maps");
        assert_eq!(type_id, 0);
        assert_eq!(note_id, vst3_note_id(2, 60));
        assert!((value - 1.0).abs() < 1e-9);

        let (type_id, _, value) = note_expression_of(&EventBody::PerNotePitchBend {
            group: 0,
            channel: 0,
            note: 64,
            value: 0x8000_0000,
        })
        .expect("bend maps");
        assert_eq!(type_id, 2);
        assert!((value - 0.5).abs() < 1e-3);

        let (_, _, value) = note_expression_of(&EventBody::PerNotePitchBend {
            group: 0,
            channel: 0,
            note: 64,
            value: u32::MAX,
        })
        .expect("bend maps");
        assert!((value - 0.7).abs() < 1e-6);

        assert!(
            note_expression_of(&EventBody::PerNoteCC {
                group: 0,
                channel: 0,
                note: 60,
                cc: 20,
                value: 0,
                registered: true,
            })
            .is_none()
        );
    }

    #[test]
    fn assignable_per_note_cc_is_not_an_expression() {
        assert!(
            note_expression_of(&EventBody::PerNoteCC {
                group: 0,
                channel: 0,
                note: 60,
                cc: 7,
                value: u32::MAX,
                registered: false,
            })
            .is_none()
        );
    }

    #[test]
    fn note_id_is_deterministic() {
        assert_eq!(vst3_note_id(0, 0), 0);
        assert_eq!(vst3_note_id(2, 60), 0x013C);
        assert_eq!(vst3_note_id(15, 127), 0x07FF);
    }

    #[test]
    fn tuning_norm_round_trips_and_saturates() {
        assert_eq!(vst3_tuning_to_wire(0.5), 0x8000_0000);
        assert!((wire_to_vst3_tuning(0x8000_0000) - 0.5).abs() < 1e-9);
        let wire = vst3_tuning_to_wire(0.55);
        assert!((wire_to_vst3_tuning(wire) - 0.55).abs() < 1e-6);
        assert_eq!(vst3_tuning_to_wire(1.0), u32::MAX);
        assert_eq!(vst3_tuning_to_wire(0.0), 0);
    }

    #[test]
    fn unit_conversion_round_trips_full_precision() {
        let center = unit_to_u32(0.5);
        assert!((u32_to_unit(center) - 0.5).abs() < 1e-9);
        assert_eq!(unit_to_u32(0.0), 0);
        assert_eq!(unit_to_u32(1.0), u32::MAX);
        assert_eq!(unit_to_u32(-0.25), 0);
        assert_eq!(unit_to_u32(1.5), u32::MAX);
        assert_eq!(unit_to_u32(f64::NAN), 0);
    }

    #[test]
    fn pitch_bend_maps_wheel_position_to_14bit() {
        let i = info(
            ParamRange::Linear {
                min: -1.0,
                max: 1.0,
            },
            Some(MidiSource::PitchBend),
        );

        assert!(matches!(
            bridge(&i, 0.0),
            Some(EventBody::PitchBend { value: 8192, .. })
        ));
        assert!(matches!(
            bridge(&i, -1.0),
            Some(EventBody::PitchBend { value: 0, .. })
        ));
        assert!(matches!(
            bridge(&i, 1.0),
            Some(EventBody::PitchBend { value: 16383, .. })
        ));
    }

    #[test]
    fn cc_and_pressure_and_program_map_to_7bit() {
        let cc = info(
            ParamRange::Linear { min: 0.0, max: 1.0 },
            Some(MidiSource::Cc(74)),
        );
        assert!(matches!(
            bridge(&cc, 1.0),
            Some(EventBody::ControlChange {
                cc: 74,
                value: 127,
                ..
            })
        ));

        let pressure = info(
            ParamRange::Linear { min: 0.0, max: 1.0 },
            Some(MidiSource::ChannelPressure),
        );
        assert!(matches!(
            bridge(&pressure, 0.0),
            Some(EventBody::ChannelPressure { pressure: 0, .. })
        ));

        let program = info(
            ParamRange::Linear { min: 0.0, max: 1.0 },
            Some(MidiSource::ProgramChange),
        );
        assert!(matches!(
            bridge(&program, 1.0),
            Some(EventBody::ProgramChange { program: 127, .. })
        ));
    }
}

#[cfg(test)]
extern crate self as moose;
#[cfg(test)]
use moose_test_fixtures::host_value_fixture;

#[cfg(test)]
mod state_tests {
    use super::host_value_fixture::TestPlugin;
    use super::*;

    #[test]
    fn load_then_save_while_not_processing_returns_loaded_custom_state() {
        for phase in ["inactive", "activated", "stopped"] {
            let inst = Vst3Instance::<TestPlugin>::create().expect("create");
            let hash = inst.plugin_id_hash;
            if phase != "inactive" {
                assert!(inst.prepare_impl(48_000., 512, 0));
                inst.set_active_impl(true);
            }
            if phase == "stopped" {
                inst.set_processing_impl(true, 48_000., 512, 0);
                inst.set_processing_impl(false, 48_000., 512, 0);
            }
            let blob = state::serialize_state(hash, &[], &[], &[4, 5, 6], &[]);
            assert!(inst.load_state_bytes(&blob), "{phase}");
            let saved = inst.save_state_bytes();
            let restored = state::deserialize_state(&saved, hash).expect("own envelope");
            assert_eq!(restored.extra.as_deref(), Some(&[4, 5, 6][..]), "{phase}");
        }
    }

    #[test]
    fn rejected_load_fails_and_keeps_the_previous_state() {
        use moose_params::Params;
        for phase in ["inactive", "stopped"] {
            let inst = Vst3Instance::<TestPlugin>::create().expect("create");
            let hash = inst.plugin_id_hash;
            if phase == "stopped" {
                assert!(inst.prepare_impl(48_000., 512, 0));
                inst.set_active_impl(true);
                inst.set_processing_impl(true, 48_000., 512, 0);
                inst.set_processing_impl(false, 48_000., 512, 0);
            }
            let load = |gain: f64, extra: &[u8]| {
                let blob = state::serialize_state(hash, &[0], &[gain], extra, &[]);
                inst.load_state_bytes(&blob)
            };
            assert!(load(-12.0, &[1]), "{phase}");
            assert!(
                !load(-30.0, b"bad"),
                "{phase}: rejected load reports failure"
            );
            assert_eq!(
                inst.params_arc.get_plain(0),
                Some(-12.0),
                "{phase}: params kept"
            );
            let saved = inst.save_state_bytes();
            let restored = state::deserialize_state(&saved, hash).expect("own envelope");
            assert_eq!(
                restored.extra.as_deref(),
                Some(&[1][..]),
                "{phase}: custom kept"
            );
        }
    }
}

#[cfg(test)]
mod main_thread_tests {
    use super::host_value_fixture::TestPlugin;
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<String>>>);

    impl MainThreadHooks for Seen {
        fn track_info_changed(&mut self, _: &MainThread, info: &TrackInfo) {
            self.0
                .lock()
                .unwrap()
                .push(format!("{:?} {:?}", info.name, info.color));
        }
        fn midi_learn(
            &mut self,
            _: &MainThread,
            bus: u32,
            channel: u8,
            source: LearnSource,
        ) -> bool {
            self.0
                .lock()
                .unwrap()
                .push(format!("{bus} {channel} {source:?}"));
            !matches!(source, LearnSource::Midi1(MidiSource::PitchBend))
        }
    }

    #[test]
    fn host_info_and_midi_learn_reach_the_hooks() {
        let inst = Vst3Instance::<TestPlugin>::create().expect("create");
        let seen = Seen::default();
        // SAFETY: test thread acts as host UI thread.
        *inst
            .main_thread
            .borrow_mut(&unsafe { MainThread::assume() }) = Box::new(seen.clone());
        let info = Vst3TrackInfo {
            name: Some(String::from("Bass")),
            color_argb: Some(0x80FF_2010),
        };
        inst.track_info_changed_impl(&info);
        let midi1 = Vst3LearnSource::Midi1;
        assert!(inst.midi_learn_impl(1, 2, midi1(74)));
        assert!(!inst.midi_learn_impl(0, 0, midi1(129)));
        assert!(!inst.midi_learn_impl(0, 0, midi1(131)), "not a controller");
        let nrpn = Vst3LearnSource::Midi2 {
            registered: false,
            bank: 5,
            index: 9,
        };
        assert!(inst.midi_learn_impl(0, 3, nrpn));
        assert_eq!(
            *seen.0.lock().unwrap(),
            [
                r#"Some("Bass") Some(HostColor { r: 255, g: 32, b: 16, a: 128 })"#,
                "1 2 Midi1(Cc(74))",
                "0 0 Midi1(PitchBend)",
                "0 3 Midi2(Midi2Controller { kind: Assignable, bank: 5, index: 9 })",
            ]
        );
    }

    #[test]
    fn function_names_match_the_sdk() {
        for (function, name) in [
            (ParamFunction::DryWetMix, c"DryWetMix"),
            (ParamFunction::Randomize, c"Randomize"),
            (ParamFunction::LowLatencyMode, c"LowLatencyMode"),
            (ParamFunction::GainReduction, c"Comp:GainReduction"),
            (ParamFunction::GainReductionMax, c"Comp:GainReductionMax"),
            (
                ParamFunction::GainReductionPeakHold,
                c"Comp:GainReductionPeakHold",
            ),
            (
                ParamFunction::ResetGainReductionMax,
                c"Comp:ResetGainReductionMax",
            ),
        ] {
            assert_eq!(function_name(function), Some(name));
        }
    }

    #[test]
    fn process_is_wait_free_and_silences_when_plugin_cell_is_contended() {
        let inst = Arc::new(Vst3Instance::<TestPlugin>::create().expect("create"));
        assert!(inst.prepare_impl(48_000.0, 64, 0));

        let held = inst.plugin.enter();
        let worker_inst = Arc::clone(&inst);
        let (out_l, out_r) = std::thread::spawn(move || {
            let in_l = [0.5f32; 16];
            let in_r = [0.5f32; 16];
            let mut out_l = [1.0f32; 16];
            let mut out_r = [1.0f32; 16];
            let in_ptrs = [
                in_l.as_ptr().cast::<std::ffi::c_void>(),
                in_r.as_ptr().cast::<std::ffi::c_void>(),
            ];
            let mut out_ptrs = [
                out_l.as_mut_ptr().cast::<std::ffi::c_void>(),
                out_r.as_mut_ptr().cast::<std::ffi::c_void>(),
            ];
            let bus_channels = [2u32];
            let mut args = ProcessArgs {
                inputs: &in_ptrs,
                outputs: &mut out_ptrs,
                in_bus_channels: &bus_channels,
                in_bus_active: 1,
                out_bus_channels: &bus_channels,
                out_bus_active: 1,
                num_frames: 16,
                transport: None,
                process_mode: 0,
            };
            unsafe { worker_inst.process_block::<f32>(&mut args) };
            (out_l, out_r)
        })
        .join()
        .expect("VST3 audio thread must not park on overlap");

        assert!(out_l.iter().all(|&s| s == 0.0));
        assert!(out_r.iter().all(|&s| s == 0.0));
        drop(held);
    }
}

#[cfg(test)]
mod host_value_tests {
    use super::host_value_fixture::TestPlugin;
    use super::*;
    use moose_params::Params;
    use std::sync::atomic::Ordering;

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "unchanged storage values must compare exactly"
    )]
    fn ffi_text_hooks_preserve_static_plain_and_normalized_storage() {
        let inst = Vst3Instance::<TestPlugin>::create().expect("create");
        let params = inst.params_arc.clone();
        let normalized = 49. / 96.;
        let plain = inst.param_denormalize_impl(1, normalized);
        assert_eq!(plain, normalized);
        assert_eq!(inst.param_format_impl(1, plain).as_deref(), Some("+1 st"));
        let parsed = inst.param_parse_impl(1, "+1 st").expect("parsed");
        assert_eq!(inst.param_normalize_impl(1, parsed), normalized);
        inst.param_set_value_impl(1, parsed);
        assert_eq!(params.get_plain(1), Some(normalized));
        for invalid in ["0.5", "nan"] {
            assert_eq!(inst.param_parse_impl(1, invalid), None);
        }
        assert_eq!(inst.param_parse_impl(0, "-6 dB"), Some(-6.));
        params.rebound.store(true, Ordering::Relaxed);
        assert_eq!(
            inst.param_format_impl(1, normalized).as_deref(),
            Some("51%")
        );
        assert_eq!(inst.param_parse_impl(1, "25%"), Some(0.25));
        assert_eq!(params.get_plain(1), Some(normalized));
    }
}

#[cfg(test)]
use moose_test_fixtures::large_state_fixture;
#[cfg(test)]
mod large_state_tests {
    use super::large_state_fixture::{LargePlugin, payload};
    use super::*;
    type Large = LargePlugin<{ 64 * 1024 * 1024 }>;

    #[test]
    fn large_host_state_roundtrips_through_stream_callback() {
        let inst = Vst3Instance::<Large>::create().expect("create");
        *inst.params_arc.payload.lock().unwrap() = payload();
        let bytes = inst.save_state_bytes();
        assert!(bytes.len() > 32 * 1024 * 1024);
        inst.params_arc.payload.lock().unwrap().clear();

        let mut offset = 0usize;
        assert!(inst.state_load_stream_impl(&mut |dst| {
            let n = dst.len().min(317).min(bytes.len() - offset);
            dst[..n].copy_from_slice(&bytes[offset..offset + n]);
            offset += n;
            i64::try_from(n).expect("test read size fits i64")
        }));
        assert_eq!(*inst.params_arc.payload.lock().unwrap(), payload());

        let truncated = &bytes[..bytes.len() - 1];
        let mut offset = 0usize;
        assert!(!inst.state_load_stream_impl(&mut |dst| {
            let n = dst.len().min(317).min(truncated.len() - offset);
            dst[..n].copy_from_slice(&truncated[offset..offset + n]);
            offset += n;
            i64::try_from(n).expect("test read size fits i64")
        }));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod persist_notify_tests;

#[cfg(test)]
mod native_note_tests;

#[cfg(test)]
mod param_point_tests {
    use super::param_point_event;
    use moose_core::Automation;
    use moose_core::events::EventBody;

    #[test]
    fn per_sample_gets_the_segment_as_a_linear_ramp() {
        let event = param_point_event(Automation::PerSample, 7, 300, 0.75, Some((100, 0.25)));
        assert_eq!(event.sample_offset, 100);
        assert_eq!(
            event.body,
            EventBody::ParamRamp {
                id: 7,
                start: 0.25,
                end: 0.75,
                len: 200
            }
        );
    }

    #[test]
    fn block_split_gets_the_point() {
        let event = param_point_event(
            Automation::BlockSplit { min: 32 },
            7,
            300,
            0.75,
            Some((100, 0.25)),
        );
        assert_eq!(event.sample_offset, 300);
        assert_eq!(event.body, EventBody::ParamChange { id: 7, value: 0.75 });
    }

    #[test]
    fn out_of_order_point_degrades_to_a_step() {
        let event = param_point_event(Automation::PerSample, 7, 300, 0.75, None);
        assert_eq!(event.body, EventBody::ParamChange { id: 7, value: 0.75 });
    }
}
