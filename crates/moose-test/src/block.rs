//! Direct single-block `process` harness.
//!
//! Calls [`PluginLogicCore::process`] on the leaf logic type with no
//! host and no sample-accurate chunker in between, owning the DSP state
//! so a test can thread it across blocks and inspect it afterward. Use
//! [`crate::driver!`] instead when the test needs the real host path
//! (sub-block splitting, transport advance, meter capture).
//!
//! For a plugin that implements `Plugin<S>`, pass its `Lifecycle` adapter
//! as the logic type.

use std::marker::PhantomData;

use moose_core::Sample;
use moose_core::buffer::AudioBuffer;
use moose_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::tasks::InitContext;
use moose_plugin::PluginLogicCore;

/// Output of one [`BlockRunner::run`] call.
pub struct BlockOutput<S> {
    /// Rendered output audio, one `Vec` per output channel.
    pub audio: Vec<Vec<S>>,
    /// Events the plugin emitted this block, in order.
    pub events: Vec<Event>,
    /// Inner `SysEx` payloads emitted this block, resolved from the
    /// block's byte pool (the [`Event`]s only carry pool indices).
    pub sysex: Vec<Vec<u8>>,
    /// The plugin's returned status.
    pub status: ProcessStatus,
}

/// Drives a plugin's `process` one block at a time.
///
/// Build with [`Self::new`] (which inits DSP state from params), tune
/// the block shape with the builder methods, then call [`Self::run`]
/// once per block. The first [`Self::run`] prepares the plugin at the
/// configured sample rate, as a host activates it before processing, so a
/// `moose::Plugin` processor exists from the first block. The runner keeps its DSP state between calls, so
/// consecutive blocks see the filter memory / voice state the previous
/// block left behind.
pub struct BlockRunner<L: PluginLogicCore<S>, S: Sample = f32> {
    state: L::DspState,
    sample_rate: f64,
    transport: TransportInfo,
    out_channels: Option<usize>,
    frames: Option<usize>,
    out_event_capacity: usize,
    /// `prepare` has run (on the first block).
    prepared: bool,
    _sample: PhantomData<fn() -> S>,
}

impl<L: PluginLogicCore<S>, S: Sample> BlockRunner<L, S> {
    /// Construct a runner with DSP state initialized from `params`.
    /// Defaults: 44.1 kHz, still transport, output shape inferred from
    /// the input passed to [`Self::run`].
    #[must_use]
    pub fn new(params: &L::Params) -> Self {
        Self {
            state: L::init(params, &InitContext::new(None)),
            sample_rate: 44_100.0,
            transport: TransportInfo::default(),
            out_channels: None,
            frames: None,
            out_event_capacity: EVENT_LIST_PREALLOC,
            prepared: false,
            _sample: PhantomData,
        }
    }

    /// Set the sample rate reported to `process`.
    #[must_use]
    pub fn sample_rate(mut self, sample_rate: f64) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    /// Set the transport reported to `process`.
    #[must_use]
    pub fn transport(mut self, transport: TransportInfo) -> Self {
        self.transport = transport;
        self
    }

    /// Pin the output shape. Without this, [`Self::run`] infers the
    /// channel count and frame length from its input slices, which is
    /// wrong for a generator (no input) or a MIDI effect (no audio) -
    /// set it explicitly there.
    #[must_use]
    pub fn outputs(mut self, channels: usize, frames: usize) -> Self {
        self.out_channels = Some(channels);
        self.frames = Some(frames);
        self
    }

    /// Set the capacity of the output event pool. Default
    /// [`EVENT_LIST_PREALLOC`]; raise it only when a block emits more.
    #[must_use]
    pub fn output_event_capacity(mut self, capacity: usize) -> Self {
        self.out_event_capacity = capacity;
        self
    }

    /// The DSP state left behind by the last [`Self::run`].
    pub fn state(&self) -> &L::DspState {
        &self.state
    }

    /// Consume the runner, returning its DSP state for inspection.
    pub fn into_state(self) -> L::DspState {
        self.state
    }

    /// Process one block: feed `inputs` (per-channel slices) and
    /// `events`, return the rendered audio plus any emitted events.
    ///
    /// # Panics
    ///
    /// Panics if the first block's `prepare` fails.
    pub fn run(
        &mut self,
        params: &L::Params,
        inputs: &[&[S]],
        events: &EventList,
    ) -> BlockOutput<S> {
        let frames = self
            .frames
            .unwrap_or_else(|| inputs.iter().map(|c| c.len()).max().unwrap_or(0));
        let channels = self.out_channels.unwrap_or(inputs.len());
        if !self.prepared {
            self.prepare(params, frames);
        }

        let mut output: Vec<Vec<S>> = (0..channels).map(|_| vec![S::default(); frames]).collect();
        let mut out_refs: Vec<&mut [S]> = output.iter_mut().map(Vec::as_mut_slice).collect();
        // SAFETY: `inputs` and `out_refs` are disjoint owned buffers and
        // both outlive the `AudioBuffer`, which never escapes this call.
        let mut buffer = unsafe { AudioBuffer::from_slices(inputs, &mut out_refs, frames) };

        let mut out_events = EventList::with_capacity(self.out_event_capacity);
        let mut context =
            ProcessContext::new(&self.transport, self.sample_rate, frames, &mut out_events);
        let status = L::process(&mut self.state, params, &mut buffer, events, &mut context);

        let sysex = out_events
            .iter()
            .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
            .map(|e| out_events.sysex_bytes(&e.body).to_vec())
            .collect();
        let emitted = out_events.iter().copied().collect();

        BlockOutput {
            audio: output,
            events: emitted,
            sysex,
            status,
        }
    }

    /// Prepare at the runner's sample rate with room for `frames` (at
    /// least [`MIN_PREPARED_BLOCK`]) per block.
    fn prepare(&mut self, params: &L::Params, frames: usize) {
        use moose_core::config::AudioConfig;
        use moose_core::plugin::{PreparationConfig, PreparationFrameBound};

        let config = PreparationConfig::new(
            AudioConfig::new(self.sample_rate, frames.max(MIN_PREPARED_BLOCK)),
            PreparationFrameBound::NegotiatedHost,
        );
        if let Err(e) = L::prepare(&mut self.state, params, &config) {
            panic!("prepare failed at {:?}: {e:?}", config.audio);
        }
        self.prepared = true;
    }
}

/// Smallest maximum block size [`BlockRunner`] prepares for, so later
/// blocks up to this size fit without re-preparing.
const MIN_PREPARED_BLOCK: usize = 4096;

/// Assert that `L::reset` is real-time safe once `L::prepare` has run.
///
/// Builds the DSP state from `params`, runs `prepare` at `config` (which may
/// allocate), then calls `reset` with the same config inside a real-time
/// section, the way CLAP `reset` and VST3 `setProcessing(false)` call it on
/// the audio thread. Any allocation or free in `reset` fails the assertion.
///
/// Meaningful only with the checker installed (`moose::enable_rt_paranoid!()`
/// plus the `rt-paranoid` feature); without it this passes vacuously.
///
/// # Panics
///
/// Panics if `prepare` fails, or if `reset` allocated or freed memory.
pub fn assert_reset_realtime<L: PluginLogicCore<S>, S: Sample>(
    params: &L::Params,
    config: &moose_core::config::AudioConfig,
) {
    use moose_core::plugin::{PreparationConfig, PreparationFrameBound};
    use moose_core::rt;

    let mut state = L::init(params, &InitContext::new(None));
    let prepared = PreparationConfig::new(*config, PreparationFrameBound::NegotiatedHost);
    if let Err(e) = L::prepare(&mut state, params, &prepared) {
        panic!("prepare failed at {config:?}: {e:?}");
    }
    let prev = rt::check_dealloc();
    rt::set_check_dealloc(true);
    let ((), violations) = rt::audit(|| {
        let _rt = rt::RtSection::enter();
        L::reset(&mut state, params, config);
    });
    rt::set_check_dealloc(prev);
    assert_eq!(
        violations, 0,
        "reset allocated or freed {violations} time(s) after prepare at {config:?}; size buffers in prepare"
    );
}
