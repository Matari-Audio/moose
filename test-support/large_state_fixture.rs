//! Independent large-state fixture, no prefs/journals/native editor.
use moose_core::{PluginRuntime, buffer::AudioBuffer, config::AudioConfig, events::EventList,
    export::PluginExport, meters::MeterStore, snapshot::SnapshotSlot,
    process::{ProcessContext, ProcessStatus}, info::PluginInfo};
use std::sync::{Arc, Mutex};
#[derive(moose_derive::Params)]
pub struct LargeParams {
    #[persist = "payload"]
    pub payload: Mutex<Vec<u8>>,
}
pub struct LargePlugin<const LIMIT: usize> {
    pub params: Arc<LargeParams>, meters: Arc<MeterStore>, snapshot: Arc<SnapshotSlot>,
}
impl<const LIMIT: usize> PluginRuntime for LargePlugin<LIMIT> {
    type Sample=f32;
    fn max_host_state_bytes() -> usize { LIMIT }
    fn info() -> PluginInfo { super::host_value_fixture::TestPlugin::info() }
    fn reset(&mut self,_:&AudioConfig) {}
    fn process(&mut self,_:&mut AudioBuffer,_:&EventList,_:&mut ProcessContext)->ProcessStatus { ProcessStatus::Normal }
}
impl<const LIMIT: usize> PluginExport for LargePlugin<LIMIT> {
    type Params=LargeParams;
    fn create()->Self { Self {params:Arc::new(LargeParams::new()),meters:MeterStore::new(),snapshot:SnapshotSlot::new()} }
    fn params(&self)->&LargeParams { &self.params }
    fn params_arc(&self)->Arc<LargeParams> { Arc::clone(&self.params) }
    fn meter_store(&self)->Arc<MeterStore> { Arc::clone(&self.meters) }
    fn snapshot_slot(&self)->Arc<SnapshotSlot> { Arc::clone(&self.snapshot) }
}
pub fn payload() -> Vec<u8> { (0..33*1024*1024).map(|i| (i%251) as u8).collect() }
