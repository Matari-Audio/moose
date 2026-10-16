//! Shared derive fixture exercised through each format's real FFI callbacks.
use moose_core::{
    buffer::AudioBuffer,
    config::AudioConfig,
    events::EventList,
    export::PluginExport,
    info::{AutomationConfig, MidiDialect, PluginCategory, PluginInfo},
    meters::MeterStore,
    plugin::PluginRuntime,
    process::{ProcessContext, ProcessStatus},
    snapshot::SnapshotSlot,
};
use moose_derive::Params;
use moose_params::{FloatParam, FloatParamReadF64};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Params)]
pub struct Bank {
    #[param(id = 0, name = "Slot", range = "linear(0, 1)", default = 0.5)]
    slot: FloatParam,
}

#[derive(Params)]
#[params(
    value_format = "display",
    value_parse = "parse",
    presentation_revision = "revision",
    presentation_change = "affects_presentation",
    persist_changed = "drain_completion"
)]
pub struct TestParams {
    #[persist]
    pub completion: Completion,
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)", unit = "dB")]
    gain: FloatParam,
    #[nested(base = 1)]
    bank: Bank,
    #[skip]
    pub rebound: AtomicBool,
    #[skip]
    pub generation: AtomicU64,
    #[skip]
    pub structural_gain: AtomicBool,
}
impl TestParams {
    fn drain_completion(&self) -> Option<(u32, f64)> {
        if !self.completion.pending.swap(false, Ordering::AcqRel) {
            return None;
        }
        let content = self.completion.content.load(Ordering::Acquire);
        if self.completion.reported.swap(content, Ordering::AcqRel) == content {
            return None;
        }
        Some((
            1,
            if self.bank.slot.value() < 0.5 {
                0.75
            } else {
                0.25
            },
        ))
    }
    fn affects_presentation(&self, id: u32) -> bool {
        id == 0 && self.structural_gain.load(Ordering::Relaxed)
    }
    fn revision(&self) -> u64 {
        if self.structural_gain.load(Ordering::Relaxed) {
            let rebound = self.gain.value() > -30.;
            if self.rebound.swap(rebound, Ordering::Relaxed) != rebound {
                self.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.generation.load(Ordering::Relaxed)
    }
    fn display(&self, id: u32, value: f64) -> Option<String> {
        (id == 1).then(|| {
            if self.rebound.load(Ordering::Relaxed) {
                format!("{:.0}%", value * 100.)
            } else {
                format!("{:+.0} st", value * 96. - 48.)
            }
        })
    }
    fn parse(&self, id: u32, text: &str) -> Result<Option<f64>, ()> {
        if id != 1 {
            return Ok(None);
        }
        if text == "nan" {
            return Ok(Some(f64::NAN));
        }
        if self.rebound.load(Ordering::Relaxed) {
            let percent: f64 = text.strip_suffix('%').ok_or(())?.parse().map_err(|_| ())?;
            return Ok(Some(percent / 100.));
        }
        let semitones: f64 = text
            .strip_suffix(" st")
            .ok_or(())?
            .parse()
            .map_err(|_| ())?;
        Ok(Some((semitones + 48.) / 96.))
    }
}

/// Source fixture only: an independently owned payload completion with exact
/// no-op detection. Native tests get the instance created on their own thread.
#[derive(Default)]
pub struct Completion {
    signal: std::sync::Mutex<std::sync::Weak<moose_params::PersistChangeSignal>>,
    pending: AtomicBool,
    content: AtomicU64,
    reported: AtomicU64,
}
impl Completion {
    pub fn complete(&self, value: u64) {
        self.content.store(value, Ordering::Release);
        self.pending.store(true, Ordering::Release);
        let signal = self.signal.lock().unwrap().upgrade();
        if let Some(signal) = signal {
            signal.notify();
        }
    }
    pub fn signal(&self) -> Option<Arc<moose_params::PersistChangeSignal>> {
        self.signal.lock().unwrap().upgrade()
    }
}
impl moose_core::custom_state::PersistField for Completion {
    fn persist_write(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.content.load(Ordering::Acquire).to_le_bytes());
    }
    fn persist_read(&self, _: &mut moose_core::custom_state::StateCursor) {}
    fn set_persist_change_signal(
        &self,
        signal: std::sync::Weak<moose_params::PersistChangeSignal>,
    ) {
        *self.signal.lock().unwrap() = signal;
    }
}
thread_local! {
    pub static CREATED: std::cell::RefCell<std::sync::Weak<TestParams>> = const {
        std::cell::RefCell::new(std::sync::Weak::new())
    };
}

pub struct TestPlugin {
    pub params: Arc<TestParams>,
    meters: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
}
impl PluginExport for TestPlugin {
    type Params = TestParams;
    fn create() -> Self {
        let params = Arc::new(TestParams::new());
        CREATED.with(|created| *created.borrow_mut() = Arc::downgrade(&params));
        Self {
            params,
            meters: MeterStore::new(),
            snapshot: SnapshotSlot::new(),
        }
    }
    fn params(&self) -> &TestParams {
        &self.params
    }
    fn params_arc(&self) -> Arc<TestParams> {
        self.params.clone()
    }
    fn meter_store(&self) -> Arc<MeterStore> {
        self.meters.clone()
    }
    fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
        self.snapshot.clone()
    }
}
impl PluginRuntime for TestPlugin {
    type Sample = f32;
    fn info() -> PluginInfo {
        PluginInfo {
            name: "Host values fixture",
            vendor: "moose",
            url: "https://example.com",
            version: "0.0.0",
            category: PluginCategory::Effect,
            description: None,
            clap_manual_url: None,
            clap_support_url: None,
            clap_features: &[],
            accepts_midi_in: false,
            emits_midi: false,
            midi_input_dialect: MidiDialect::Midi1,
            midi_output_dialect: MidiDialect::Midi1,
            midi_input_ports: 0,
            midi_output_ports: 0,
            bundle_id: "host-values-fixture",
            vst3_id: "org.moose.host-values-fixture",
            clap_id: "org.moose.host-values-fixture",
            fourcc: *b"HvFx",
            au_type: *b"aufx",
            au_manufacturer: *b"Moos",
            vst3_subcategory: None,
            preset_user_dir: None,
            preset_extension: "moose",
            vst3_name: None,
            clap_name: None,
            au_name: None,
            au3_name: None,
            mute_preview_output: false,
            automation: AutomationConfig::DEFAULT,
            legacy_au_keys: &[],
        }
    }
    fn reset(&mut self, _: &AudioConfig) {}
    fn process(
        &mut self,
        _: &mut AudioBuffer,
        _: &EventList,
        _: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }
}
