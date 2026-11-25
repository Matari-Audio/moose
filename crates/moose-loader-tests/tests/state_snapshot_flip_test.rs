//! Custom-state save contract (audit 2026-11-18, step 1).
//!
//! Exercises the low-level `PluginLogicCore::save_state` / `load_state` /
//! snapshot hooks on `StaticShell` directly: a core impl that overrides only
//! `save_state` must round-trip without serializing on the audio thread every
//! block (publishes a baseline at creation, serializes on demand when a host
//! save asks for a fresh snapshot, and serializes live on the host thread via
//! `republish_snapshot`).
//!
//! Run with `--features rt-paranoid` to also prove the steady-state blocks
//! make no audio-thread allocation.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moose_core::AudioConfig;
use moose_core::buffer::AudioBuffer;
use moose_core::events::{EventList, TransportInfo};
use moose_core::plugin::PluginRuntime;
use moose_core::process::{ProcessContext, ProcessStatus};
use moose_core::state::StateLoadError;
use moose_core::tasks::InitContext;
use moose_derive::Params;
use moose_loader::PluginLogicCore;
use moose_loader::static_shell::StaticShell;
use moose_params::FloatParam;

#[derive(Params)]
struct StateParams {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)")]
    gain: FloatParam,
}

moose::enable_rt_paranoid!();

/// `save_state` calls on `LegacyStatePlugin`. Tests that read it hold
/// `SERIAL` so they don't count each other's calls.
static SAVES: AtomicUsize = AtomicUsize::new(0);
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Default)]
struct Counter {
    value: u32,
}

/// Overrides only `save_state` / `load_state` - never `snapshot_into`.
struct LegacyStatePlugin;

impl PluginLogicCore<f32> for LegacyStatePlugin {
    type Params = StateParams;
    type DspState = Counter;

    fn init(_params: &StateParams, _cx: &InitContext) -> Counter {
        Counter::default()
    }

    fn reset(_state: &mut Counter, _params: &StateParams, _config: &AudioConfig) {}

    fn process(
        _state: &mut Counter,
        _params: &StateParams,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }

    fn save_state(state: &Counter) -> Vec<u8> {
        SAVES.fetch_add(1, Ordering::Relaxed);
        state.value.to_le_bytes().to_vec()
    }

    fn load_state(state: &mut Counter, data: &[u8]) -> Result<(), StateLoadError> {
        let bytes: [u8; 4] = data
            .try_into()
            .map_err(|_| StateLoadError::Malformed("Counter expects 4 bytes"))?;
        state.value = u32::from_le_bytes(bytes);
        Ok(())
    }
}

/// A plugin with no custom state at all - both `save_state` and
/// `snapshot_into` keep their defaults, so it must never publish.
struct PlainPlugin;

impl PluginLogicCore<f32> for PlainPlugin {
    type Params = StateParams;
    type DspState = ();

    fn init(_params: &StateParams, _cx: &InitContext) {}

    fn reset(_state: &mut (), _params: &StateParams, _config: &AudioConfig) {}

    fn process(
        _state: &mut (),
        _params: &StateParams,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }
}

fn shell<L>() -> StaticShell<StateParams, L>
where
    L: PluginLogicCore<f32, Params = StateParams>,
{
    let mut shell =
        StaticShell::<StateParams, L>::from_parts(Arc::new(StateParams::new()), None, None);
    shell.reset(&AudioConfig::new(44100.0, 64));
    shell
}

fn run_block<L: PluginLogicCore<f32, Params = StateParams>>(
    shell: &mut StaticShell<StateParams, L>,
) {
    run_block_audited(shell, false);
}

/// One block; with `audited`, only the shell's `process` call runs inside
/// a real-time section (the harness's own buffers and lists allocate).
fn run_block_audited<L: PluginLogicCore<f32, Params = StateParams>>(
    shell: &mut StaticShell<StateParams, L>,
    audited: bool,
) {
    let input = [0.0f32; 64];
    let mut output = [0.0f32; 64];
    let inputs: [&[f32]; 1] = [&input];
    let mut outputs: [&mut [f32]; 1] = [&mut output];
    // SAFETY: one 64-frame channel each way, both live for the call.
    let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 64) };
    let transport = TransportInfo::default();
    let mut output_events = EventList::default();
    let mut ctx = ProcessContext::new(&transport, 44100.0, 64, &mut output_events);
    let events = EventList::default();
    let _rt = audited.then(moose::rt::RtSection::enter);
    shell.process(&mut buffer, &events, &mut ctx);
}

#[test]
fn save_state_only_plugin_does_not_serialize_per_block() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut plugin = shell::<LegacyStatePlugin>();
    let slot = plugin.snapshot_slot();
    assert_eq!(slot.read(), Some(0u32.to_le_bytes().to_vec()), "baseline");
    let before = SAVES.load(Ordering::Relaxed);
    for _ in 0..8 {
        run_block(&mut plugin);
    }
    assert_eq!(
        SAVES.load(Ordering::Relaxed),
        before,
        "no per-block save_state"
    );
    let mut buf = Vec::new();
    assert!(!plugin.snapshot_into(&mut buf), "not opted in");
}

#[test]
fn save_state_only_plugin_blocks_are_allocation_free() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut plugin = shell::<LegacyStatePlugin>();
    run_block(&mut plugin);
    let ((), allocs) = moose::rt::audit(|| {
        for _ in 0..4 {
            run_block_audited(&mut plugin, true);
        }
    });
    assert_eq!(allocs, 0, "per-block path allocated {allocs} times");
    let ((), probe) = moose::rt::audit(|| {
        let _rt = moose::rt::RtSection::enter();
        std::hint::black_box(Box::new(std::hint::black_box(1_u8)));
    });
    assert_eq!(probe > 0, moose::rt::is_active());
}

#[test]
fn requested_snapshot_is_served_by_the_next_block() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut plugin = shell::<LegacyStatePlugin>();
    let slot = plugin.snapshot_slot();
    plugin.state_ref_mut().value = 42;
    let ticket = slot.request_fresh();
    assert!(!slot.wait_served(ticket, std::time::Duration::ZERO));
    let before = SAVES.load(Ordering::Relaxed);
    run_block(&mut plugin);
    assert!(slot.wait_served(ticket, std::time::Duration::ZERO));
    assert_eq!(slot.read(), Some(42u32.to_le_bytes().to_vec()));
    run_block(&mut plugin);
    assert_eq!(
        SAVES.load(Ordering::Relaxed),
        before + 1,
        "served exactly once"
    );
}

#[test]
fn republish_stores_live_state_after_load() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut plugin = shell::<LegacyStatePlugin>();
    plugin.load_state(&7u32.to_le_bytes()).expect("load_state");
    plugin.republish_snapshot();
    assert_eq!(
        plugin.snapshot_slot().read(),
        Some(7u32.to_le_bytes().to_vec())
    );
}

#[test]
fn plugin_without_custom_state_does_not_publish() {
    let plugin = shell::<PlainPlugin>();
    let mut buf = Vec::new();
    assert!(!plugin.snapshot_into(&mut buf));
    assert_eq!(buf, Vec::<u8>::new());
    assert_eq!(plugin.snapshot_slot().read(), Some(Vec::new()));
}
