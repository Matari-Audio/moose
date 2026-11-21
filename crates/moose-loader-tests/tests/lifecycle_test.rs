//! `moose::Plugin` through the `Lifecycle` adapter and the static shell:
//! the processor exists only after a successful activation.
// The asserted values (0.0, 0.5, 1.0, 100.0) are exact in f32 and f64.
#![allow(clippy::float_cmp)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use moose::prelude::{Editor, PluginContext, PreparationError};
use moose::{Lifecycle, Plugin};
use moose_core::AudioConfig;
use moose_core::buffer::AudioBuffer;
use moose_core::editor::RawWindowHandle;
use moose_core::events::{EventList, TransportInfo};
use moose_core::main_thread::{MainThreadHooks, TrackInfo};
use moose_core::plugin::{PluginRuntime, PreparationConfig, PreparationFrameBound};
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::tasks::InitContext;
use moose_core::thread::MainThread;
use moose_derive::Params;
use moose_gui::PluginLogicCore;
use moose_loader::static_shell::StaticShell;
use moose_params::{FloatParam, FloatParamReadF32 as _, Params as _};

#[derive(Params)]
struct LcParams {
    #[param(id = 0, name = "Level", range = "linear(0, 1)", default = 0.5)]
    level: FloatParam,
    /// Custom state, saved and restored with the params.
    #[persist = "value"]
    value: Mutex<u32>,
    /// Main-thread hooks write here so the test can see they ran.
    #[skip]
    track_seen: AtomicBool,
    /// `activate` fails while this is set.
    #[skip]
    refuse: AtomicBool,
}

/// Counts what reached it; `value` is read from `#[persist]` at activation.
struct Counter {
    blocks: u32,
    resets: u32,
    value: u32,
    /// `state_changed` calls.
    changed: u32,
}

struct Hooks(Arc<LcParams>);
impl MainThreadHooks for Hooks {
    fn track_info_changed(&mut self, _cx: &MainThread, _info: &TrackInfo) {
        self.0.track_seen.store(true, Ordering::Relaxed);
    }
}

static ACTIVATIONS: AtomicU32 = AtomicU32::new(0);

struct Lc;

impl Plugin for Lc {
    type Shared = LcParams;
    type MainThread = Hooks;
    type Processor = Counter;

    fn main_thread(shared: &Arc<LcParams>) -> Hooks {
        Hooks(Arc::clone(shared))
    }

    fn activate(
        shared: &LcParams,
        _config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<Counter, PreparationError> {
        if shared.refuse.load(Ordering::Relaxed) {
            return Err(PreparationError::ResourceBudget);
        }
        ACTIVATIONS.fetch_add(1, Ordering::Relaxed);
        Ok(Counter {
            blocks: 0,
            resets: 0,
            value: *shared.value.lock().expect("value lock"),
            changed: 0,
        })
    }

    fn reset(processor: &mut Counter, _shared: &LcParams) {
        processor.resets += 1;
    }

    fn process(
        processor: &mut Counter,
        shared: &LcParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        processor.blocks += 1;
        let level = shared.level.value();
        buffer.output(0).fill(level);
        ProcessStatus::KeepAlive
    }

    fn state_changed(processor: &mut Counter, shared: &LcParams) {
        processor.changed += 1;
        processor.value = *shared.value.lock().expect("value lock");
    }

    fn editor(_shared: Arc<LcParams>) -> Box<dyn Editor> {
        Box::new(NoEditor)
    }
}

struct NoEditor;
impl Editor for NoEditor {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn open(&mut self, _: RawWindowHandle, _: PluginContext) {}
    fn close(&mut self) {}
}

type Shell = StaticShell<LcParams, Lifecycle<Lc>>;

fn shell() -> (Shell, Arc<LcParams>) {
    let params = Arc::new(LcParams::new());
    (Shell::from_parts(Arc::clone(&params), None), params)
}

fn config() -> PreparationConfig {
    config_at(48_000.0, 64)
}

fn config_at(sample_rate: f64, max_block: usize) -> PreparationConfig {
    PreparationConfig::new(
        AudioConfig::new(sample_rate, max_block),
        PreparationFrameBound::NegotiatedHost,
    )
}

/// One 64-frame mono block; returns the status and the first output sample.
fn block(shell: &mut Shell) -> (ProcessStatus, f32) {
    let input = [0.0f32; 64];
    let mut output = [1.0f32; 64];
    let inputs: [&[f32]; 1] = [&input];
    let transport = TransportInfo::default();
    let mut output_events = EventList::default();
    let status = {
        let mut outputs: [&mut [f32]; 1] = [&mut output];
        // SAFETY: one 64-frame channel each way, both live for the call.
        let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 64) };
        let mut ctx = ProcessContext::new(&transport, 48_000.0, 64, &mut output_events);
        shell.process(&mut buffer, &EventList::default(), &mut ctx)
    };
    (status, output[0])
}

#[test]
fn processor_is_absent_and_output_silent_before_activate() {
    let (mut plugin, _params) = shell();
    assert!(plugin.state_ref().processor().is_none());
    // `reset` with no processor is a no-op, not a panic.
    plugin.reset(&AudioConfig::new(48_000.0, 64));
    let (status, first) = block(&mut plugin);
    assert_eq!(status, ProcessStatus::Normal);
    assert_eq!(first, 0.0, "silence before activation");
    assert!(plugin.state_ref().processor().is_none());
}

#[test]
fn activate_error_is_a_preparation_failure() {
    let (mut plugin, params) = shell();
    params.refuse.store(true, Ordering::Relaxed);
    assert_eq!(
        plugin.prepare(&config()),
        Err(PreparationError::ResourceBudget)
    );
    assert!(plugin.state_ref().processor().is_none());
    assert_eq!(block(&mut plugin).1, 0.0, "still silent");

    params.refuse.store(false, Ordering::Relaxed);
    assert_eq!(plugin.prepare(&config()), Ok(()));
    assert!(plugin.state_ref().processor().is_some());
}

#[test]
fn reset_and_process_reach_the_processor() {
    let (mut plugin, _params) = shell();
    plugin.prepare(&config()).expect("activates");
    let (status, first) = block(&mut plugin);
    assert_eq!(status, ProcessStatus::KeepAlive);
    assert_eq!(first, 0.5, "processor wrote the level param");
    block(&mut plugin);
    plugin.reset(&AudioConfig::new(48_000.0, 64));
    let processor = plugin.state_ref().processor().expect("active");
    assert_eq!((processor.blocks, processor.resets), (2, 1));
}

#[test]
fn persist_state_set_before_activate_reaches_the_processor() {
    let (mut plugin, params) = shell();
    *params.value.lock().unwrap() = 7;
    plugin.prepare(&config()).expect("activates");
    assert_eq!(plugin.state_ref().processor().map(|p| p.value), Some(7));

    // Re-activation (new sample rate) builds a new processor from `shared`.
    let before = ACTIVATIONS.load(Ordering::Relaxed);
    plugin
        .prepare(&config_at(96_000.0, 64))
        .expect("re-activates");
    assert!(ACTIVATIONS.load(Ordering::Relaxed) > before);
    assert_eq!(plugin.state_ref().processor().map(|p| p.value), Some(7));
}

#[test]
fn main_thread_hooks_object_is_returned() {
    let params = Arc::new(LcParams::new());
    let mut hooks = <Lifecycle<Lc> as PluginLogicCore<f32>>::main_thread(&params);
    // SAFETY: the test thread is the only thread that uses main-thread
    // tokens in this test.
    let cx = unsafe { MainThread::assume() };
    hooks.track_info_changed(&cx, &TrackInfo::new());
    assert!(params.track_seen.load(Ordering::Relaxed));
}

#[test]
fn state_changed_follows_a_live_load() {
    let (mut plugin, params) = shell();
    plugin.prepare(&config()).expect("activates");
    // A host load restores `#[persist]` on the params, then the shell runs
    // the custom-state step, which ends in `state_changed`.
    params.load_persist(&params_with_value(9).serialize_persist());
    plugin.load_state(&[]).expect("loads");
    let processor = plugin.state_ref().processor().expect("active");
    assert_eq!((processor.value, processor.changed), (9, 1));
}

fn params_with_value(value: u32) -> LcParams {
    let params = LcParams::new();
    *params.value.lock().unwrap() = value;
    params
}

/// `moose::Plugin` keeps its custom state in `#[persist]`, so the processor
/// never publishes a snapshot or a save blob of its own.
#[test]
fn processor_side_custom_state_is_gone() {
    type Logic = Lifecycle<Lc>;
    let (mut plugin, _params) = shell();
    plugin.prepare(&config()).expect("activates");
    let mut buf = Vec::new();
    assert!(!<Logic as PluginLogicCore<f32>>::snapshot_into(
        plugin.state_ref(),
        &mut buf
    ));
    assert_eq!(buf, [] as [u8; 0]);
    assert_eq!(
        <Logic as PluginLogicCore<f32>>::snapshot_version(plugin.state_ref()),
        None
    );
    assert_eq!(plugin.save_state(), [] as [u8; 0]);
}

#[test]
fn reactivation_the_processor_fits_keeps_it() {
    let (mut plugin, _params) = shell();
    plugin
        .prepare(&config_at(48_000.0, 256))
        .expect("activates");
    block(&mut plugin);
    // Same rate, smaller max block: the processor is kept and reset.
    plugin
        .prepare(&config_at(48_000.0, 64))
        .expect("re-prepares");
    let processor = plugin.state_ref().processor().expect("active");
    assert_eq!(
        (processor.blocks, processor.resets),
        (1, 1),
        "kept and reset, not rebuilt"
    );
    // A larger block than it was built for needs a new processor.
    plugin
        .prepare(&config_at(48_000.0, 512))
        .expect("re-activates");
    let processor = plugin.state_ref().processor().expect("active");
    assert_eq!((processor.blocks, processor.resets), (0, 0), "rebuilt");
}

/// Declares fallible preparation; nothing else.
struct Fallible;

impl Plugin for Fallible {
    type Shared = LcParams;
    type MainThread = ();
    type Processor = ();

    fn requires_fallible_preparation() -> bool {
        true
    }

    fn main_thread(_shared: &Arc<LcParams>) {}

    fn activate(
        _shared: &LcParams,
        _config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<(), PreparationError> {
        Ok(())
    }

    fn process(
        _processor: &mut (),
        _shared: &LcParams,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }

    fn editor(_shared: Arc<LcParams>) -> Box<dyn Editor> {
        Box::new(NoEditor)
    }
}

#[test]
fn requires_fallible_preparation_is_forwarded() {
    assert!(!<Lifecycle<Lc> as PluginLogicCore<f32>>::requires_fallible_preparation());
    assert!(<Lifecycle<Fallible> as PluginLogicCore<f32>>::requires_fallible_preparation());
}
