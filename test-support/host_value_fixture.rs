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
use moose_params::FloatParam;
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
    presentation_revision = "revision"
)]
pub struct TestParams {
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)", unit = "dB")]
    gain: FloatParam,
    #[nested(base = 1)]
    bank: Bank,
    #[skip]
    pub rebound: AtomicBool,
    #[skip]
    pub generation: AtomicU64,
}
impl TestParams {
    fn revision(&self) -> u64 {
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

pub struct TestPlugin {
    pub params: Arc<TestParams>,
    meters: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
}
impl PluginExport for TestPlugin {
    type Params = TestParams;
    fn create() -> Self {
        Self {
            params: Arc::new(TestParams::new()),
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
