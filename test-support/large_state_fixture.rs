//! Independent large-state fixture, no prefs/journals/native editor.
use moose_core::{
    PluginRuntime,
    buffer::AudioBuffer,
    config::AudioConfig,
    events::EventList,
    export::PluginExport,
    info::PluginInfo,
    meters::MeterStore,
    process::{ProcessContext, ProcessStatus},
    snapshot::SnapshotSlot,
};
use std::sync::{Arc, Mutex};
#[derive(moose_derive::Params)]
#[params(prepare_persist_stream = "prepare_stream")]
pub struct LargeParams {
    #[param(id = 0, name = "Dummy", range = "linear(0, 1)")]
    dummy: moose_params::FloatParam,
    #[persist = "payload"]
    pub payload: Mutex<Vec<u8>>,
}
impl LargeParams {
    fn prepare_stream(
        input: &mut dyn std::io::Read,
        len: usize,
    ) -> Option<std::io::Result<(Vec<u8>, moose_params::PreparedPersist<Self>)>> {
        Some(Self::read_payload(input, len))
    }
    fn read_payload(
        input: &mut dyn std::io::Read,
        len: usize,
    ) -> std::io::Result<(Vec<u8>, moose_params::PreparedPersist<Self>)> {
        let invalid = || std::io::Error::other("invalid fixture persistence");
        let mut word = || {
            let mut bytes = [0; 4];
            input.read_exact(&mut bytes)?;
            Ok::<_, std::io::Error>(u32::from_le_bytes(bytes) as usize)
        };
        if word()? != 1 || word()? != 7 {
            return Err(invalid());
        }
        let mut key = [0; 7];
        input.read_exact(&mut key)?;
        if &key != b"payload" {
            return Err(invalid());
        }
        let mut bytes = [0; 4];
        input.read_exact(&mut bytes)?;
        let field = u32::from_le_bytes(bytes) as usize;
        input.read_exact(&mut bytes)?;
        let count = u32::from_le_bytes(bytes) as usize;
        if count > len || count.checked_add(4) != Some(field) {
            return Err(invalid());
        }
        let mut payload = Vec::new();
        payload.try_reserve_exact(count).map_err(|_| invalid())?;
        payload.resize(count, 0);
        input.read_exact(&mut payload)?;
        Ok((
            Vec::new(),
            Box::new(move |params, values, _| {
                use moose_params::Params;
                params.restore_values(values);
                *params.payload.lock().unwrap() = payload;
            }),
        ))
    }
}
pub struct LargePlugin<const LIMIT: usize> {
    pub params: Arc<LargeParams>,
    meters: Arc<MeterStore>,
    snapshot: Arc<SnapshotSlot>,
}
impl<const LIMIT: usize> PluginRuntime for LargePlugin<LIMIT> {
    type Sample = f32;
    fn max_host_state_bytes() -> usize {
        LIMIT
    }
    fn info() -> PluginInfo {
        super::host_value_fixture::TestPlugin::info()
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
impl<const LIMIT: usize> PluginExport for LargePlugin<LIMIT> {
    type Params = LargeParams;
    fn create() -> Self {
        Self {
            params: Arc::new(LargeParams::new()),
            meters: MeterStore::new(),
            snapshot: SnapshotSlot::new(),
        }
    }
    fn params(&self) -> &LargeParams {
        &self.params
    }
    fn params_arc(&self) -> Arc<LargeParams> {
        Arc::clone(&self.params)
    }
    fn meter_store(&self) -> Arc<MeterStore> {
        Arc::clone(&self.meters)
    }
    fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
        Arc::clone(&self.snapshot)
    }
}
pub fn payload() -> Vec<u8> {
    (0..33 * 1024 * 1024).map(|i| (i % 251) as u8).collect()
}
