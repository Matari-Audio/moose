//! VST3 format wrapper for moose.
//!
//! The COM objects (`com/`) are built on the official SDK interfaces from
//! the `vst3` crate. This file holds the plugin side: a callback table,
//! generic over the plugin type, that the COM layer calls.

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

// The inline test modules below `use super::*` and reach every split module.
#[cfg(test)]
use crate::{
    host_state::*, input_events::*, instance::*, lifecycle::*, midi_proxy::*, output_events::*,
    params::*, process::*, registration::*,
};
#[cfg(test)]
use ffi::Vst3NativeEvent;
#[cfg(test)]
use moose_core::config::AudioConfig;
#[cfg(test)]
use moose_core::events::Event;
#[cfg(test)]
use moose_core::events::EventList;
#[cfg(test)]
use moose_core::export::PluginExport;
#[cfg(test)]
use moose_core::info::PluginInfo;
#[cfg(test)]
use moose_core::main_thread::LearnSource;
#[cfg(test)]
use moose_core::main_thread::MainThreadHooks;
#[cfg(test)]
use moose_core::main_thread::TrackInfo;
#[cfg(test)]
use moose_core::meters::MeterStore;
#[cfg(test)]
use moose_core::plugin::PluginRuntime;
#[cfg(test)]
use moose_core::snapshot::SnapshotSlot;
#[cfg(test)]
use moose_core::thread::MainThread;
#[cfg(test)]
use moose_params::MidiSource;
#[cfg(test)]
use moose_params::ParamFunction;
#[cfg(test)]
use moose_params::ParamInfo;
#[cfg(test)]
use moose_params::ParamRange;
#[cfg(all(test, target_os = "linux"))]
use moose_params::Params;
#[cfg(test)]
use std::ffi::CStr;
#[cfg(test)]
use std::slice;
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
            pub extern "C" fn moose_vst3_init() {
                ::moose_vst3::register_vst3::<$plugin_type>();
            }

            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn GetPluginFactory() -> *mut ::std::ffi::c_void {
                // Lazy init: register on first call
                static INIT: ::std::sync::Once = ::std::sync::Once::new();
                INIT.call_once(|| {
                    moose_vst3_init();
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
        // Every (port, channel, controller) triple survives the trip -
        // multi-timbral hosts rely on the port dimension to keep each
        // bus's controllers separate.
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
        // The whole point of per-port banks: the same (channel, cc)
        // on two buses must be two parameter queues host-side.
        assert_ne!(midi_proxy_id(0, 4, 74), midi_proxy_id(1, 4, 74));
    }

    #[test]
    fn real_param_ids_never_decode() {
        // Hash ids live below METER_ID_BASE, meters just above it -
        // both far under the proxy base.
        const _: () = assert!(MIDI_PROXY_ID_BASE > moose_params::METER_ID_BASE);
        assert_eq!(midi_proxy_decode(0), None);
        assert_eq!(midi_proxy_decode(moose_params::METER_ID_BASE), None);
        assert_eq!(midi_proxy_decode(MIDI_PROXY_ID_BASE - 1), None);
        // One past the last bank is out again.
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

    /// Layouts differing only in channel *width* (the point of multi-layout)
    /// share one bus topology and are accepted.
    #[test]
    fn topology_consistent_when_only_widths_vary() {
        let layouts = [
            BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo),
            BusLayout::mono().with_sidechain_input("Sidechain", ChannelConfig::Mono),
        ];
        assert!(vst3_topology_consistent(&layouts));
    }

    /// A layout with a different input bus *count* (the report's case: a
    /// mono main with no sidechain alongside a main+sidechain layout) can
    /// never be matched, so it's rejected at registration.
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
        // Main stereo + stereo sidechain = 4 input channels, 2 output.
        let layouts =
            [BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo)];
        assert_eq!(max_layout_channels(&layouts), (4, 2));
    }

    #[test]
    fn wide_layouts_are_measured_not_capped() {
        // 17 stereo buses = 34 channels: past the old 32-channel shim cap,
        // now just the width the process pointer arrays are sized for.
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
    use std::ffi::c_void;

    #[test]
    fn midi_proxy_text_parses_the_fallback_display() {
        assert_eq!(parse_midi_proxy_text("0.50"), Some(0.5));
        assert_eq!(parse_midi_proxy_text(" 2 "), Some(1.0));
        assert_eq!(parse_midi_proxy_text("-1"), Some(0.0));
        assert_eq!(parse_midi_proxy_text("NaN"), None);
        assert_eq!(parse_midi_proxy_text("loud"), None);
    }

    use crate::com::{
        read_state_chunk as moose_vst3_read_state_chunk,
        write_state_stream as moose_vst3_write_state_stream,
    };

    /// A fake `IBStream`: the vtable pointer first, then a script of
    /// `(tresult, byte count)` replies the shim sees per read/write.
    #[repr(C)]
    struct FakeStream {
        vtbl: *const FakeVtbl,
        replies: Vec<(i32, i32)>,
        calls: usize,
    }

    unsafe extern "C" fn fake_io(s: *mut c_void, _buf: *mut c_void, n: i32, out: *mut i32) -> i32 {
        let s = unsafe { &mut *s.cast::<FakeStream>() };
        let (r, count) = s.replies.get(s.calls).copied().unwrap_or((0, 0));
        s.calls += 1;
        unsafe { *out = if count == i32::MAX { n } else { count } };
        r
    }

    type IoFn = unsafe extern "C" fn(*mut c_void, *mut c_void, i32, *mut i32) -> i32;

    /// `IBStream` vtable layout: 3 `FUnknown` slots, read, write, seek, tell.
    #[repr(C)]
    struct FakeVtbl {
        _unknown: [usize; 3],
        read: IoFn,
        write: IoFn,
        _seek_tell: [usize; 2],
    }

    static FAKE_VTBL: FakeVtbl = FakeVtbl {
        _unknown: [0; 3],
        read: fake_io,
        write: fake_io,
        _seek_tell: [0; 2],
    };

    fn fake(replies: &[(i32, i32)]) -> FakeStream {
        FakeStream {
            vtbl: &raw const FAKE_VTBL,
            replies: replies.to_vec(),
            calls: 0,
        }
    }

    #[test]
    fn state_read_chunk_preserves_short_reads_and_rejects_bad_counts() {
        let mut s = fake(&[(0, 317), (1, 17), (0, 0), (0, -1), (0, 8193)]);
        let mut buf = [0_u8; 8192];
        for expected in [317, 17, 0, -1, -1] {
            let count = unsafe {
                moose_vst3_read_state_chunk(
                    (&raw mut s).cast(),
                    buf.as_mut_ptr(),
                    u32::try_from(buf.len()).expect("buffer length fits u32"),
                )
            };
            assert_eq!(count, expected);
        }
    }

    #[test]
    fn state_write_loops_over_partial_writes() {
        let blob = [7u8; 100];
        let write = |replies: &[(i32, i32)]| {
            let mut s = fake(replies);
            let ok =
                unsafe { moose_vst3_write_state_stream((&raw mut s).cast(), blob.as_ptr(), 100) };
            (ok, s.calls)
        };
        assert_eq!(write(&[(0, 60), (0, 40)]), (1, 2));
        // No progress, or a count past the request, fails the save.
        assert_eq!(write(&[(0, 60), (0, 0)]).0, 0);
        assert_eq!(write(&[(0, 101)]).0, 0);
    }
    use moose_params::{MidiSource, ParamFlags, ParamUnit, ParamValueKind};

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

    /// Bridge a param change the way `process_block` does: only a param
    /// with a `midi_map` produces an event.
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
        // The `process_block` fast path: build the sorted cache the way
        // `cb_create` does, then look up by id. Unmapped params are
        // absent, so their ids (and unknown ids) miss.
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
        // The mapped CC bridges to a ControlChange on its number.
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
        // Volume CC (7) -> VST3 type 0; noteId = (channel<<7)|note.
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

        // Pitch bend -> tuning (type 2), center value ~0.5.
        let (type_id, _, value) = note_expression_of(&EventBody::PerNotePitchBend {
            group: 0,
            channel: 0,
            note: 64,
            value: 0x8000_0000,
        })
        .expect("bend maps");
        assert_eq!(type_id, 2);
        assert!((value - 0.5).abs() < 1e-3);

        // Full-scale wire bend is ±48 st; VST3's tuning norm spans
        // ±120 st, so it must land at 0.5 + 48/240 = 0.7, not 1.0.
        let (_, _, value) = note_expression_of(&EventBody::PerNotePitchBend {
            group: 0,
            channel: 0,
            note: 64,
            value: u32::MAX,
        })
        .expect("bend maps");
        assert!((value - 0.7).abs() < 1e-6);

        // A CC with no predefined VST3 note-expression type is skipped.
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
        // Only registered per-note indices carry the predefined
        // expression semantics; an assignable index 7 is not volume.
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
        assert_eq!(vst3_note_id(2, 60), 0x013C); // (2 << 7) | 60
        assert_eq!(vst3_note_id(15, 127), 0x07FF); // (15 << 7) | 127
    }

    #[test]
    fn midi_event_layout_matches_shim() {
        // The C++ shim static_asserts the same shape; a drift on either
        // side fails its build or this test.
        assert_eq!(std::mem::size_of::<Vst3MidiEvent>(), 24);
        assert_eq!(std::mem::align_of::<Vst3MidiEvent>(), 8);
    }

    #[test]
    fn note_id_map_scopes_ids_per_port() {
        let mut map = NoteIdMap::new();
        // Host counters are arbitrary - nothing like the pitch - and
        // scoped per event bus: the same id on two buses is two
        // distinct voices.
        map.insert(0, 90210, 3, 64);
        map.insert(1, 90210, 5, 72);
        assert_eq!(map.lookup(0, 90210), Some((3, 64)));
        assert_eq!(map.lookup(1, 90210), Some((5, 72)));
        assert_eq!(map.lookup(2, 90210), None);
        assert_eq!(map.lookup(0, 64), None); // pitch is not a key
        // Unassigned ids never enter the map.
        map.insert(0, -1, 0, 60);
        assert_eq!(map.lookup(0, -1), None);
        // A reset drops every correlation.
        map.clear();
        assert_eq!(map.lookup(0, 90210), None);
        assert_eq!(map.lookup(1, 90210), None);
    }

    #[test]
    fn note_id_map_overflow_overwrites_oldest() {
        let mut map = NoteIdMap::new();
        for i in 0..NoteIdMap::CAPACITY {
            // Bounded by CAPACITY = 128, fits in both domains.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            map.insert(0, 1000 + i as i32, 0, i as u8);
        }
        // Full map: the next insert takes the round-robin slot rather
        // than being dropped, and the newest entry resolves.
        map.insert(0, 5000, 1, 72);
        assert_eq!(map.lookup(0, 5000), Some((1, 72)));
        // Entries outlive their note-off by design, so a full map of
        // released voices still can't wedge it: stale ids fall to the
        // round-robin overwrite.
        for i in 0..NoteIdMap::CAPACITY {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            map.insert(0, 6000 + i as i32, 2, i as u8);
        }
        assert_eq!(map.lookup(0, 1000), None);
    }

    #[test]
    fn tuning_norm_round_trips_and_saturates() {
        // Center and mid-range survive the domain re-scale both ways.
        assert_eq!(vst3_tuning_to_wire(0.5), 0x8000_0000);
        assert!((wire_to_vst3_tuning(0x8000_0000) - 0.5).abs() < 1e-9);
        let wire = vst3_tuning_to_wire(0.55); // +12 st
        assert!((wire_to_vst3_tuning(wire) - 0.55).abs() < 1e-6);
        // A host bend past the wire's ±48 st saturates.
        assert_eq!(vst3_tuning_to_wire(1.0), u32::MAX);
        assert_eq!(vst3_tuning_to_wire(0.0), 0);
    }

    #[test]
    fn unmapped_per_note_cc_degrades_to_channel_cc() {
        // No predefined VST3 expression type for cc 20 - it must fall
        // through to the 1.0 downconvert as a channel CC (matching
        // CLAP), not vanish.
        let event = Event::new(
            0,
            EventBody::PerNoteCC {
                group: 0,
                channel: 3,
                note: 60,
                cc: 20,
                value: u32::MAX,
                registered: true,
            },
        );
        let packet = try_encode_vst3_midi(&event).expect("degrades to channel CC");
        assert_eq!(packet.status, 0xB3);
        assert_eq!(packet.data1, 20);
        assert_eq!(packet.data2, 127);

        // Mapped per-note events ride note expression instead - the
        // MIDI encoder must skip them or they'd double-emit.
        let mapped = Event::new(
            0,
            EventBody::PerNoteCC {
                group: 0,
                channel: 0,
                note: 60,
                cc: 7,
                value: 0,
                registered: true,
            },
        );
        assert!(try_encode_vst3_midi(&mapped).is_none());
        let bend = Event::new(
            0,
            EventBody::PerNotePitchBend {
                group: 0,
                channel: 0,
                note: 60,
                value: 0,
            },
        );
        assert!(try_encode_vst3_midi(&bend).is_none());
    }

    #[test]
    fn unit_conversion_round_trips_full_precision() {
        // A centered tuning value must survive the crossing exactly -
        // the old 7-bit path decoded 0.5 as ~0.496 (about a semitone
        // flat over the +/-120 st tuning domain).
        let center = unit_to_u32(0.5);
        assert!((u32_to_unit(center) - 0.5).abs() < 1e-9);
        assert_eq!(unit_to_u32(0.0), 0);
        assert_eq!(unit_to_u32(1.0), u32::MAX);
        // FFI hygiene: out-of-domain hosts get clamped, not wrapped.
        assert_eq!(unit_to_u32(-0.25), 0);
        assert_eq!(unit_to_u32(1.5), u32::MAX);
        assert_eq!(unit_to_u32(f64::NAN), 0);
    }

    #[test]
    fn output_encode_carries_port() {
        // The plug-in stamps an outbound event's MIDI port; the shim
        // reads it back off `Vst3MidiEvent::port` to pick the event bus.
        let event = Event::on_port(
            5,
            2,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 100,
            },
        );
        let packet = try_encode_vst3_midi(&event).expect("note-on encodes");
        assert_eq!(packet.port, 2);
    }

    #[test]
    fn pitch_bend_maps_wheel_position_to_14bit() {
        // The synth's binding range: -1..1, where the host's
        // normalized 0/0.5/1 wheel positions land on plain -1/0/1.
        let i = info(
            ParamRange::Linear {
                min: -1.0,
                max: 1.0,
            },
            Some(MidiSource::PitchBend),
        );

        // Center wheel -> 8192.
        assert!(matches!(
            bridge(&i, 0.0),
            Some(EventBody::PitchBend { value: 8192, .. })
        ));
        // Full down -> 0, full up -> 16383.
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

// The derive names `::moose::{core, params}`; test-only `#[derive(Params)]`s resolve here.
#[cfg(test)]
extern crate self as moose;
#[cfg(test)]
use moose_test_fixtures::host_value_fixture;

#[cfg(test)]
mod state_tests {
    use super::host_value_fixture::TestPlugin;
    use super::*;

    /// Load-then-`getState` in each state where the host thread owns the
    /// plugin: inactive, active before the first `setProcessing(true)`,
    /// and after `setProcessing(false)` (audit finding 1). No `process`
    /// runs in between.
    #[test]
    fn load_then_save_while_not_processing_returns_loaded_custom_state() {
        for phase in ["inactive", "activated", "stopped"] {
            // SAFETY: serial callback calls on an instance this test owns;
            // the save buffer is freed with the matching libc allocator.
            unsafe {
                let ctx = cb_create::<TestPlugin>();
                let hash = (&*ctx.cast::<Vst3Instance<TestPlugin>>()).plugin_id_hash;
                if phase != "inactive" {
                    assert_eq!(cb_prepare::<TestPlugin>(ctx, 48_000., 512, 0), 1);
                    cb_set_active::<TestPlugin>(ctx, 1);
                }
                if phase == "stopped" {
                    cb_set_processing::<TestPlugin>(ctx, 1, 48_000., 512, 0);
                    cb_set_processing::<TestPlugin>(ctx, 0, 48_000., 512, 0);
                }
                let blob = state::serialize_state(hash, &[], &[], &[4, 5, 6], &[]);
                let len = u32::try_from(blob.len()).unwrap();
                assert_eq!(
                    cb_state_load::<TestPlugin>(ctx, blob.as_ptr(), len),
                    1,
                    "{phase}"
                );
                let mut data = std::ptr::null_mut();
                let mut saved_len = 0;
                cb_state_save::<TestPlugin>(ctx, &raw mut data, &raw mut saved_len);
                assert!(!data.is_null(), "{phase}");
                let saved = slice::from_raw_parts(data, saved_len as usize).to_vec();
                cb_state_free(data, saved_len);
                let restored = state::deserialize_state(&saved, hash).expect("own envelope");
                assert_eq!(restored.extra.as_deref(), Some(&[4, 5, 6][..]), "{phase}");
                cb_destroy::<TestPlugin>(ctx);
            }
        }
    }

    /// A state whose custom part `load_state` rejects fails the load
    /// (kResultFalse) and leaves params and custom state as they were
    /// (audit finding 4).
    #[test]
    fn rejected_load_fails_and_keeps_the_previous_state() {
        use moose_params::Params;
        for phase in ["inactive", "stopped"] {
            // SAFETY: serial callback calls on an instance this test owns;
            // the save buffer is freed with the matching libc allocator.
            unsafe {
                let ctx = cb_create::<TestPlugin>();
                let inst = &*ctx.cast::<Vst3Instance<TestPlugin>>();
                let hash = inst.plugin_id_hash;
                if phase == "stopped" {
                    assert_eq!(cb_prepare::<TestPlugin>(ctx, 48_000., 512, 0), 1);
                    cb_set_active::<TestPlugin>(ctx, 1);
                    cb_set_processing::<TestPlugin>(ctx, 1, 48_000., 512, 0);
                    cb_set_processing::<TestPlugin>(ctx, 0, 48_000., 512, 0);
                }
                let load = |gain: f64, extra: &[u8]| {
                    let blob = state::serialize_state(hash, &[0], &[gain], extra, &[]);
                    let len = u32::try_from(blob.len()).unwrap();
                    cb_state_load::<TestPlugin>(ctx, blob.as_ptr(), len)
                };
                assert_eq!(load(-12.0, &[1]), 1, "{phase}");
                assert_eq!(
                    load(-30.0, b"bad"),
                    0,
                    "{phase}: rejected load reports failure"
                );
                assert_eq!(
                    inst.params_arc.get_plain(0),
                    Some(-12.0),
                    "{phase}: params kept"
                );
                let mut data = std::ptr::null_mut();
                let mut saved_len = 0;
                cb_state_save::<TestPlugin>(ctx, &raw mut data, &raw mut saved_len);
                let saved = slice::from_raw_parts(data, saved_len as usize).to_vec();
                cb_state_free(data, saved_len);
                let restored = state::deserialize_state(&saved, hash).expect("own envelope");
                assert_eq!(
                    restored.extra.as_deref(),
                    Some(&[1][..]),
                    "{phase}: custom kept"
                );
                cb_destroy::<TestPlugin>(ctx);
            }
        }
    }
}

#[cfg(test)]
mod main_thread_tests {
    use super::host_value_fixture::TestPlugin;
    use super::*;
    use std::sync::Mutex;

    /// Hooks that keep what the host told them.
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
        // SAFETY: the test thread stands in for the host UI thread and owns
        // the instance for every callback.
        unsafe {
            let ctx = cb_create::<TestPlugin>();
            let inst = &*ctx.cast::<Vst3Instance<TestPlugin>>();
            let seen = Seen::default();
            *inst.main_thread.borrow_mut(&MainThread::assume()) = Box::new(seen.clone());
            let info = ffi::Vst3TrackInfo {
                name: c"Bass".as_ptr(),
                color_argb: 0x80FF_2010,
                has_color: true,
            };
            cb_track_info_changed::<TestPlugin>(ctx, &raw const info);
            let midi1 = ffi::Vst3LearnSource::Midi1;
            assert_eq!(cb_midi_learn::<TestPlugin>(ctx, 1, 2, midi1(74)), 1);
            assert_eq!(cb_midi_learn::<TestPlugin>(ctx, 0, 0, midi1(129)), 0);
            assert_eq!(
                cb_midi_learn::<TestPlugin>(ctx, 0, 0, midi1(131)),
                0,
                "not a controller"
            );
            let nrpn = ffi::Vst3LearnSource::Midi2 {
                registered: false,
                bank: 5,
                index: 9,
            };
            assert_eq!(cb_midi_learn::<TestPlugin>(ctx, 0, 3, nrpn), 1);
            assert_eq!(
                *seen.0.lock().unwrap(),
                [
                    r#"Some("Bass") Some(HostColor { r: 255, g: 32, b: 16, a: 128 })"#,
                    "1 2 Midi1(Cc(74))",
                    "0 0 Midi1(PitchBend)",
                    "0 3 Midi2(Midi2Controller { kind: Assignable, bank: 5, index: 9 })",
                ]
            );
            cb_destroy::<TestPlugin>(ctx);
        }
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
            // SAFETY: the SDK constants are static NUL-terminated strings.
            assert_eq!(unsafe { CStr::from_ptr(function_name(function)) }, name);
        }
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
        // SAFETY: own the instance and bounded buffers for all callback calls.
        unsafe {
            let ctx = cb_create::<TestPlugin>();
            assert!(!ctx.is_null());
            let params = (&*ctx.cast::<Vst3Instance<TestPlugin>>())
                .params_arc
                .clone();
            let normalized = 49. / 96.;
            let plain = cb_param_denormalize::<TestPlugin>(ctx, 1, normalized);
            assert_eq!(plain, normalized);
            let mut buffer = [0_i8; 128];
            assert!(cb_param_format::<TestPlugin>(ctx, 1, plain, buffer.as_mut_ptr(), 128) > 0);
            assert_eq!(CStr::from_ptr(buffer.as_ptr()).to_str().unwrap(), "+1 st");
            let mut parsed = -999.;
            assert_eq!(
                cb_param_parse::<TestPlugin>(ctx, 1, c"+1 st".as_ptr(), &raw mut parsed),
                1
            );
            assert_eq!(cb_param_normalize::<TestPlugin>(ctx, 1, parsed), normalized);
            cb_param_set_value::<TestPlugin>(ctx, 1, parsed);
            assert_eq!(params.get_plain(1), Some(normalized));
            for invalid in [c"0.5", c"nan"] {
                assert_eq!(
                    cb_param_parse::<TestPlugin>(ctx, 1, invalid.as_ptr(), &raw mut parsed),
                    0
                );
            }
            assert_eq!(
                cb_param_parse::<TestPlugin>(ctx, 0, c"-6 dB".as_ptr(), &raw mut parsed),
                1
            );
            assert_eq!(parsed, -6.);
            params.rebound.store(true, Ordering::Relaxed);
            assert!(
                cb_param_format::<TestPlugin>(ctx, 1, normalized, buffer.as_mut_ptr(), 128) > 0
            );
            assert_eq!(CStr::from_ptr(buffer.as_ptr()).to_str().unwrap(), "51%");
            assert_eq!(
                cb_param_parse::<TestPlugin>(ctx, 1, c"25%".as_ptr(), &raw mut parsed),
                1
            );
            assert_eq!(parsed, 0.25);
            assert_eq!(params.get_plain(1), Some(normalized));
            cb_destroy::<TestPlugin>(ctx);
        }
    }
}

#[cfg(test)]
use moose_test_fixtures::large_state_fixture;
#[cfg(test)]
mod large_state_tests {
    use super::large_state_fixture::{LargePlugin, payload};
    use super::*;
    type Large = LargePlugin<{ 64 * 1024 * 1024 }>;
    struct Input {
        bytes: Vec<u8>,
        offset: usize,
    }
    unsafe extern "C" fn read(ctx: *mut std::ffi::c_void, dst: *mut u8, size: u32) -> i64 {
        let input = unsafe { &mut *ctx.cast::<Input>() };
        let n = (size as usize)
            .min(317)
            .min(input.bytes.len() - input.offset);
        unsafe { std::ptr::copy_nonoverlapping(input.bytes.as_ptr().add(input.offset), dst, n) };
        input.offset += n;
        i64::try_from(n).expect("test read size fits i64")
    }
    #[test]
    fn large_host_state_roundtrips_through_stream_callback() {
        unsafe {
            let ctx = cb_create::<Large>();
            assert!(!ctx.is_null());
            let inst = &*ctx.cast::<Vst3Instance<Large>>();
            *inst.params_arc.payload.lock().unwrap() = payload();
            let mut ptr = std::ptr::null_mut();
            let mut len = 0;
            cb_state_save::<Large>(ctx, &raw mut ptr, &raw mut len);
            assert!(len > 32 * 1024 * 1024);
            let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            cb_state_free(ptr, len);
            inst.params_arc.payload.lock().unwrap().clear();
            let mut input = Input { bytes, offset: 0 };
            assert_eq!(
                cb_state_load_stream::<Large>(ctx, (&raw mut input).cast(), read),
                1
            );
            assert_eq!(*inst.params_arc.payload.lock().unwrap(), payload());
            input.bytes.pop();
            input.offset = 0;
            assert_eq!(
                cb_state_load_stream::<Large>(ctx, (&raw mut input).cast(), read),
                0
            );
            cb_destroy::<Large>(ctx);
        }
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
        assert_eq!(
            event.sample_offset, 100,
            "the ramp starts at the previous point"
        );
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
