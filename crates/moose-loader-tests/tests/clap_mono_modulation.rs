//! D3: sample-accurate, non-destructive CLAP mono modulation.
//!
//! A mono `ParamMod` (`note_id == -1`) is an additive offset beside the
//! stored base: `FloatParam::modulated_value` moves at the event's
//! sub-block boundary while `value()`, the edit counter and saved state
//! stay put. Plugins that read `EventBody::ParamMod` themselves still see
//! every event, so nothing is applied twice.
//!
//! Run with `--features rt-paranoid` to also prove the chunked path makes
//! no audio-thread allocation.
#![allow(clippy::float_cmp, reason = "exactly representable test values")]

use std::sync::Arc;

use moose_core::buffer::AudioBuffer;
use moose_core::chunked_process::{ChunkedProcess, process_chunked};
use moose_core::config::{AudioConfig, ProcessMode};
use moose_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use moose_core::info::PluginInfo;
use moose_core::plugin::PluginRuntime;
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::state::serialize_state;
use moose_derive::Params;
use moose_params::{
    DynamicParams, FloatParam, FloatParamReadF64, ParamInfo, ParamSmoother, Params, SlotDef,
    SlotRange,
};

moose::enable_rt_paranoid!();

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct Filter {
    #[param(id = 0, name = "Drive", range = "linear(-1, 1)", default = 0.0)]
    drive: FloatParam,
}

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct ModParams {
    #[param(
        id = 0,
        name = "Cutoff",
        range = "linear(0, 10)",
        default = 4.0,
        smooth = "linear(10)"
    )]
    cutoff: FloatParam,
    #[nested(base = 1)]
    filter: Filter,
    #[nested(base = 2)]
    slots: DynamicParams<2>,
}

const BLOCK: usize = 512;
const CUTOFF: u32 = 0;

/// What one `process()` call saw.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SubBlock {
    len: usize,
    base: f64,
    modulated: f64,
    /// Mono `ParamMod` events in the rebased sub-block list.
    mono_mod_events: usize,
    /// Polyphonic `ParamMod` events in the rebased sub-block list.
    poly_mod_events: usize,
}

struct Recorder {
    params: Arc<ModParams>,
    seen: Vec<SubBlock>,
}

impl Recorder {
    fn new(params: &Arc<ModParams>) -> Self {
        Self {
            params: Arc::clone(params),
            // Preallocated: recording must not allocate on the audio path.
            seen: Vec::with_capacity(64),
        }
    }
}

impl PluginRuntime for Recorder {
    type Sample = f32;

    fn info() -> PluginInfo {
        unimplemented!("process_chunked never queries info()")
    }

    fn reset(&mut self, _config: &AudioConfig) {
        self.params.clear_modulations();
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let (mut mono, mut poly) = (0, 0);
        for ev in events.iter_from(0) {
            match ev.body {
                EventBody::ParamMod { note_id: -1, .. } => mono += 1,
                EventBody::ParamMod { .. } => poly += 1,
                _ => {}
            }
        }
        let cutoff = &self.params.cutoff;
        self.seen.push(SubBlock {
            len: buffer.num_samples(),
            base: FloatParamReadF64::value(cutoff),
            modulated: cutoff.modulated_value(),
            mono_mod_events: mono,
            poly_mod_events: poly,
        });
        ProcessStatus::Normal
    }
}

/// Preallocated lists and buffers for one host block.
struct Host {
    input: Vec<f32>,
    output: Vec<f32>,
    scratch: EventList,
    out_events: EventList,
    transport: TransportInfo,
    infos: Vec<ParamInfo>,
}

impl Host {
    fn new(params: &ModParams) -> Self {
        Self {
            input: vec![0.0; BLOCK],
            output: vec![0.0; BLOCK],
            scratch: EventList::with_capacity(EVENT_LIST_PREALLOC),
            out_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
            transport: TransportInfo::default(),
            infos: params.param_infos(),
        }
    }

    fn run(&mut self, plugin: &mut Recorder, params: &ModParams, events: &EventList) {
        self.run_with_automation(
            plugin,
            params,
            events,
            moose_core::Automation::BlockSplit { min: 1 },
        );
    }

    fn run_with_automation(
        &mut self,
        plugin: &mut Recorder,
        params: &ModParams,
        events: &EventList,
        automation: moose_core::Automation,
    ) {
        let inputs: [&[f32]; 1] = [&self.input];
        let mut outputs: [&mut [f32]; 1] = [&mut self.output];
        let mut buffer = AudioBuffer::<f32>::from_slices_checked(&inputs, &mut outputs, BLOCK);
        self.out_events.clear();
        let args = ChunkedProcess {
            events,
            sub_event_scratch: &mut self.scratch,
            transport: &mut self.transport,
            sample_rate: 48_000.0,
            process_mode: ProcessMode::Realtime,
            output_events: &mut self.out_events,
            params_fn: None,
            meters_fn: None,
            latency_change_request: None,
            param_infos: &self.infos,
            automation,
        };
        process_chunked(plugin, params as &dyn Params, &mut buffer, args);
    }
}

fn mono_mod(offset: u32, id: u32, value: f64) -> Event {
    Event::new(
        offset,
        EventBody::ParamMod {
            id,
            note_id: -1,
            value,
        },
    )
}

/// The bytes a host save would write for this store.
fn saved_state(params: &ModParams) -> Vec<u8> {
    let (ids, values) = params.collect_values();
    serialize_state(0xD3, &ids, &values, &[], &params.serialize_persist())
}

fn sub(len: usize, base: f64, modulated: f64, mono: usize, poly: usize) -> SubBlock {
    SubBlock {
        len,
        base,
        modulated,
        mono_mod_events: mono,
        poly_mod_events: poly,
    }
}

/// 1. A mid-block mono mod moves `modulated_value()` at the exact split
///    offset; the base, the edit counter and the saved state never move.
#[test]
fn mid_block_mono_mod_is_sample_accurate_and_non_destructive() {
    let params = Arc::new(ModParams::new());
    let mut plugin = Recorder::new(&params);
    let mut host = Host::new(&params);
    let state_before = saved_state(&params);
    let epoch_before = params.parameter_edit_epoch();

    let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    events.push(mono_mod(128, CUTOFF, 2.5));
    host.run(&mut plugin, &params, &events);

    assert_eq!(
        plugin.seen,
        [sub(128, 4.0, 4.0, 0, 0), sub(384, 4.0, 6.5, 1, 0)],
        "the offset lands at sample 128, and the plugin still sees the event"
    );
    assert_eq!(params.cutoff.modulation(), 2.5);
    assert_eq!(params.get_plain(CUTOFF), Some(4.0), "host-visible base");
    assert_eq!(params.parameter_edit_epoch(), epoch_before, "not an edit");
    assert_eq!(saved_state(&params), state_before, "saved state unchanged");

    // A later mod replaces the offset (CLAP amounts are absolute, not
    // cumulative), and automation of the base keeps the offset on top.
    plugin.seen.clear();
    let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    events.push(Event::new(
        64,
        EventBody::ParamChange {
            id: CUTOFF,
            value: 5.0,
        },
    ));
    events.push(mono_mod(300, CUTOFF, -1.0));
    host.run(&mut plugin, &params, &events);
    assert_eq!(
        plugin.seen,
        [
            sub(64, 4.0, 6.5, 0, 0),
            sub(236, 5.0, 7.5, 0, 0),
            sub(212, 5.0, 4.0, 1, 0),
        ]
    );

    // Reset (the wrapper's `clear_modulations`) drops the offset.
    plugin.reset(&AudioConfig::new(48_000.0, BLOCK));
    assert_eq!(params.cutoff.modulation(), 0.0);
    assert_eq!(params.cutoff.modulated_value(), 5.0);
}

/// 2. A per-voice mod neither splits the block nor touches the mono slot;
///    it still reaches the plugin.
#[test]
fn poly_mod_never_clobbers_the_mono_offset() {
    let params = Arc::new(ModParams::new());
    let mut plugin = Recorder::new(&params);
    let mut host = Host::new(&params);

    let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    events.push(mono_mod(128, CUTOFF, 2.5));
    events.push(Event::new(
        256,
        EventBody::ParamMod {
            id: CUTOFF,
            note_id: 42,
            value: -9.0,
        },
    ));
    host.run(&mut plugin, &params, &events);

    assert_eq!(
        plugin.seen,
        [sub(128, 4.0, 4.0, 0, 0), sub(384, 4.0, 6.5, 1, 1)],
        "no split at the poly event; the mono offset survives it"
    );
    assert_eq!(params.cutoff.modulation(), 2.5);
}

/// 3. The effective value clamps into the range, and non-finite amounts are
///    dropped on every path (field, trait, chunker).
#[test]
fn modulation_clamps_to_range_and_drops_non_finite() {
    let params = ModParams::new();
    let cutoff = &params.cutoff;

    params.set_modulation(CUTOFF, 100.0);
    assert_eq!(cutoff.modulated_value(), 10.0, "clamped at max");
    params.set_modulation(CUTOFF, -100.0);
    assert_eq!(cutoff.modulated_value(), 0.0, "clamped at min");
    assert_eq!(cutoff.modulation(), -10.0, "offset saturates at the span");

    params.set_modulation(CUTOFF, 1.5);
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        params.set_modulation(CUTOFF, bad);
        cutoff.set_modulation(bad);
        assert_eq!(cutoff.modulation(), 1.5, "{bad} is dropped");
    }
    let mut plugin = Recorder::new(&Arc::new(ModParams::new()));
    let mut host = Host::new(&params);
    let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    events.push(mono_mod(10, CUTOFF, f64::NAN));
    host.run(&mut plugin, &params, &events);
    assert_eq!(cutoff.modulation(), 1.5, "a NaN event is dropped too");
    assert_eq!(cutoff.modulated_value(), 5.5);
    assert_eq!(FloatParamReadF64::value(cutoff), 4.0);

    // Nested fields and runtime slots take mods by their rebased ids.
    let drive = params.filter.drive.id();
    params.set_modulation(drive, 0.25);
    assert_eq!(params.filter.drive.modulated_value(), 0.25);
    assert_eq!(params.get_plain(drive), Some(0.0));
    let slot = params.slots.slot(1).expect("slot 1");
    params.slots.edit(|tx| {
        tx.bind(slot, SlotDef::new("Mix", SlotRange::Int { min: 0, max: 8 }));
    });
    params.set_modulation(params.slots.id(slot), 2.6);
    assert_eq!(params.slots.modulated_plain(slot), 3.0, "stepped slot");
    assert_eq!(params.slots.plain(slot), 0.0);

    // An unknown id is ignored; clearing reaches every level.
    params.set_modulation(9_999, 1.0);
    params.clear_modulations();
    assert_eq!(cutoff.modulation(), 0.0);
    assert_eq!(params.filter.drive.modulation(), 0.0);
    assert_eq!(params.slots.modulation(slot), 0.0);
    assert_eq!(cutoff.modulated_value(), 4.0);
}

/// 4. A processor-owned smoother glides to the modulated value at the
///    param's declared smoothing instead of stepping.
#[test]
fn param_smoother_tracks_the_modulated_value() {
    let params = ModParams::new();
    let cutoff = &params.cutoff;
    let mut smoother = ParamSmoother::default();
    smoother.reset_modulated(cutoff, 48_000.0);
    assert_eq!(smoother.current_f64(), 4.0);

    params.set_modulation(CUTOFF, 4.0);
    let mut previous = smoother.current_f64();
    // linear(10) at 48 kHz: 480 samples.
    for n in 0..480 {
        let v = smoother.next_modulated_f64(cutoff);
        assert!(
            v >= previous && v <= 8.0,
            "sample {n}: {v} after {previous}"
        );
        if n == 239 {
            assert!((v - 6.0).abs() < 0.05, "halfway through the glide: {v}");
        }
        previous = v;
    }
    assert!((previous - 8.0).abs() < 1e-9, "landed: {previous}");
    assert!(!smoother.is_smoothing_modulated(cutoff));
    assert!(
        smoother.is_smoothing(cutoff),
        "the base ramp target is unmodulated"
    );
    assert_eq!(FloatParamReadF64::value(cutoff), 4.0, "base unchanged");

    // A host snap (state load) jumps the ramp to the modulated value.
    cutoff.set_value(1.0);
    cutoff.snap_smoother();
    assert_eq!(smoother.next_modulated_f64(cutoff), 5.0);
}

/// 5. The chunked mod path makes no audio-thread allocation. Vacuous
///    without `--features rt-paranoid`, so the probe proves the checker
///    is live when it is enabled.
#[test]
fn modulation_path_is_allocation_free() {
    let params = Arc::new(ModParams::new());
    let mut plugin = Recorder::new(&params);
    let mut host = Host::new(&params);
    let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    for (i, offset) in (0..BLOCK).step_by(32).enumerate() {
        let offset = u32::try_from(offset).expect("small offset");
        let amount = f64::from(u32::try_from(i).expect("small index")) * 0.25 - 2.0;
        events.push(mono_mod(offset, CUTOFF, amount));
        events.push(Event::new(
            offset + 1,
            EventBody::ParamMod {
                id: CUTOFF,
                note_id: 7,
                value: amount,
            },
        ));
    }
    // Warm-up block outside the audit.
    host.run(&mut plugin, &params, &events);
    plugin.seen.clear();
    let ((), allocs) = moose::rt::audit(|| {
        host.run(&mut plugin, &params, &events);
        params.clear_modulations();
    });
    assert_eq!(allocs, 0, "mono modulation path allocated {allocs} times");
    assert_eq!(plugin.seen.len(), BLOCK / 32, "one sub-block per mono mod");

    let ((), probe) = moose::rt::audit(|| {
        let _rt = moose::rt::RtSection::enter();
        std::hint::black_box(Box::new(std::hint::black_box(1_u8)));
    });
    assert_eq!(probe > 0, moose::rt::is_active());
}

/// 6. Under `Automation::PerSample`, only offset-0 modulation is applied
///    before `plugin.process`; later events stay in the event stream for the
///    callback and reconcile into the store after `plugin.process` returns.
#[test]
fn per_sample_defers_future_modulation_until_after_process() {
    let params = Arc::new(ModParams::new());
    let mut plugin = Recorder::new(&params);
    let mut host = Host::new(&params);

    let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    events.push(mono_mod(128, CUTOFF, 2.5));

    host.run_with_automation(
        &mut plugin,
        &params,
        &events,
        moose_core::Automation::PerSample,
    );

    assert_eq!(
        plugin.seen,
        vec![sub(BLOCK, 4.0, 4.0, 1, 0)],
        "sample-128 modulation must not leak before PerSample process runs"
    );
    assert_eq!(
        params.cutoff.modulated_value(),
        6.5,
        "end-of-block modulation reconciles after process for the next block"
    );

    // Offset-0 modulation DOES apply before PerSample process.
    plugin.seen.clear();
    let mut zero_events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    zero_events.push(mono_mod(0, CUTOFF, -1.0));
    zero_events.push(mono_mod(64, CUTOFF, 3.0));
    host.run_with_automation(
        &mut plugin,
        &params,
        &zero_events,
        moose_core::Automation::PerSample,
    );
    assert_eq!(plugin.seen, vec![sub(BLOCK, 4.0, 3.0, 2, 0)]);
    assert_eq!(params.cutoff.modulated_value(), 7.0);
}
