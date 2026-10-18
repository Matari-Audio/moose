//! Real generated-symbol tests; root runner must provide exact built SDK rlibs.
//! No skipping, no guessed target directory, no dependency/manifest mutation.
#![cfg(feature="shell")]
use moose_core::{AudioConfig,plugin::{PluginRuntime,PreparationConfig,PreparationFrameBound,PreparationError}};
use moose_loader::{NativeLoader,HotShell};
use reload_fixture_common::FxParams;
use std::{path::{Path,PathBuf},process::Command,sync::Arc};
fn required_env(name:&str)->PathBuf{
    let path=PathBuf::from(std::env::var_os(name).unwrap_or_else(||panic!("root qualification requires {name}")));
    assert!(path.is_absolute(),"{name} must be an explicit absolute receipt path");path
}
fn suffix()->&'static str{std::env::consts::DLL_SUFFIX}
fn build(out:&Path,name:&str,required:bool)->PathBuf{
    let source=out.join(format!("{name}.rs"));let library=out.join(format!("{}{name}{}",std::env::consts::DLL_PREFIX,suffix()));
    let text=r#"
use moose::prelude::*;
use reload_fixture_common::{CounterLogic,CounterState,FxParams};
const REQUIRED:bool=__CLASS__;
struct Logic;
impl PluginLogic for Logic{
 type Params=FxParams;type DspState=CounterState;
 fn requires_fallible_preparation()->bool{REQUIRED}
 fn reset(state:&mut CounterState,_:&FxParams,_:&AudioConfig){state.counter+=1000;}
 fn prepare(state:&mut CounterState,params:&FxParams,config:&moose::core::plugin::PreparationConfig)->Result<(),moose::core::plugin::PreparationError>{
  config.validate()?;if REQUIRED{return Err(moose::core::plugin::PreparationError::MissingPolicy);}
  <Self as PluginLogic>::reset(state,params,&config.audio);Ok(())
 }
 fn process(state:&mut CounterState,params:&FxParams,buffer:&mut AudioBuffer,events:&EventList,cx:&mut ProcessContext)->ProcessStatus{
  <CounterLogic as moose_loader::PluginLogicCore<f32>>::process(state,params,buffer,events,cx)
 }
 fn latency(state:&CounterState)->u32{<CounterLogic as moose_loader::PluginLogicCore<f32>>::latency(state)}
 fn save_state(state:&CounterState)->Vec<u8>{<CounterLogic as moose_loader::PluginLogicCore<f32>>::save_state(state)}
 fn load_state(state:&mut CounterState,data:&[u8])->Result<(),StateLoadError>{<CounterLogic as moose_loader::PluginLogicCore<f32>>::load_state(state,data)}
 fn editor(params:Arc<FxParams>)->Box<dyn Editor>{<CounterLogic as moose_loader::PluginLogicCore<f32>>::editor(params)}
}
moose_loader::export_plugin!(Logic,FxParams);
"#.replace("__CLASS__",if required{"true"}else{"false"});
    std::fs::write(&source,text).unwrap();
    let mut command=Command::new(std::env::var_os("RUSTC").unwrap_or_else(||"rustc".into()));
    command.args(["--edition=2024","--crate-type=cdylib","--crate-name",name]);
    for (crate_name,variable) in [("moose","MOOSE_PREPARATION_MOOSE_RLIB"),("moose_loader","MOOSE_PREPARATION_LOADER_RLIB"),("reload_fixture_common","MOOSE_PREPARATION_COMMON_RLIB")]{
        let path=required_env(variable);assert!(path.is_file(),"{variable} actual build artifact missing");
        command.arg("--extern").arg(format!("{crate_name}={}",path.display()));
    }
    let deps=required_env("MOOSE_PREPARATION_DEPS");assert!(deps.is_dir());
    let output=command.arg("-L").arg(format!("dependency={}",deps.display())).arg(&source).arg("-o").arg(&library).output().expect("root-only fixture rustc");
    std::fs::write(out.join(format!("{name}.stdout")),&output.stdout).unwrap();
    std::fs::write(out.join(format!("{name}.stderr")),&output.stderr).unwrap();
    assert!(output.status.success(),"actual complete export fixture {name} failed: {}",String::from_utf8_lossy(&output.stderr));
    assert!(library.is_file());library
}
#[test]
fn real_complete_exports_gate_initial_and_reload_and_direct_hot_prepare(){
    // Root executes this whole test under ONE common outer build lock and
    // timeout, with TMPDIR and output on the dev drive. No runtime Cargo call,
    // repo copy, target override, download or missing-fixture skip.
    let out=required_env("MOOSE_PREPARATION172_OUT");assert!(!out.exists(),"fresh immutable receipt directory required");
    assert!(std::env::var_os("TMPDIR").is_some(),"root must set a dev-drive TMPDIR for actual loader copies");
    std::fs::create_dir_all(&out).unwrap();
    let legacy=build(&out,"preparation_legacy_172",false);let required=build(&out,"preparation_required_172",true);
    let params=Arc::new(FxParams::default());let pointer=Arc::as_ptr(&params).cast::<()>();
    let rejected=NativeLoader::<f32>::new(required.clone(),pointer);
    assert!(!rejected.is_loaded());assert!(rejected.init_state().is_none());
    let mut required_shell=HotShell::<FxParams>::new(FxParams::default(),required.clone());
    let request=PreparationConfig::new(AudioConfig::new(48000.,71),PreparationFrameBound::ExplicitDriver);
    assert_eq!(required_shell.prepare(&request),Err(PreparationError::NotReady));assert_eq!(required_shell.latency(),0);
    let mut legacy_shell=HotShell::<FxParams>::new(FxParams::default(),legacy.clone());
    assert_eq!(legacy_shell.prepare(&request),Ok(()));assert_eq!(legacy_shell.latency(),1000,"real admitted legacy reset ran exactly once");
    let live=out.join(format!("live{}",suffix()));std::fs::copy(&legacy,&live).unwrap();
    let mut loader=NativeLoader::<f32>::new(live.clone(),pointer);assert!(loader.is_loaded());
    let(state,origin)=loader.init_state().expect("valid full-symbol legacy fixture");
    loader.reset(state,&request.audio);assert_eq!(loader.latency(state),1000);let generation=loader.swap_generation();
    std::fs::copy(&required,&live).unwrap();assert!(!loader.reload(),"class-only required change cannot replace legacy instance");
    assert_eq!(loader.swap_generation(),generation);assert!(loader.is_loaded());assert_eq!(loader.latency(state),1000);
    loader.reset(state,&request.audio);assert_eq!(loader.latency(state),2000,"the exact old valid instance remains usable");
    (origin.drop)(state);
}

struct RequiredStatic;
impl moose::prelude::PluginLogic for RequiredStatic{
    type Params=FxParams;type DspState=reload_fixture_common::CounterState;
    fn requires_fallible_preparation()->bool{true}
    fn prepare(_:&mut Self::DspState,_:&Self::Params,_:&PreparationConfig)->Result<(),PreparationError>{Err(PreparationError::MissingPolicy)}
    fn reset(state:&mut Self::DspState,_:&Self::Params,_:&AudioConfig){state.counter=0;}
    fn process(state:&mut Self::DspState,params:&Self::Params,buffer:&mut moose_core::buffer::AudioBuffer,
        events:&moose_core::events::EventList,cx:&mut moose_core::process::ProcessContext)->moose_core::process::ProcessStatus{
        <reload_fixture_common::CounterLogic as moose_loader::PluginLogicCore<f32>>::process(state,params,buffer,events,cx)
    }
    fn editor(params:Arc<FxParams>)->Box<dyn moose::prelude::Editor>{
        <reload_fixture_common::CounterLogic as moose_loader::PluginLogicCore<f32>>::editor(params)
    }
}
#[test]
fn real_static_runtime_requirement_is_forwarded_and_refusal_does_not_reset_dsp_state(){
    use moose_loader::static_shell::StaticShell;
    let params=Arc::new(FxParams::default());
    let request=PreparationConfig::new(AudioConfig::new(48000.,8192),PreparationFrameBound::AdvertisedDeviceMaximum);
    let mut legacy=StaticShell::<FxParams,reload_fixture_common::CounterLogic>::from_parts(Arc::clone(&params),None);
    assert!(!legacy.requires_fallible_preparation());legacy.prepare(&request).unwrap();
    legacy.load_state(&71u64.to_le_bytes()).unwrap();assert_eq!(legacy.state_ref().counter,71);
    // The standalone's existing legacy no-growth/no-force branch skips a new
    // prepare; the actual runtime classification remains false after restore.
    assert!(!legacy.requires_fallible_preparation());assert_eq!(legacy.state_ref().counter,71);
    let mut required=StaticShell::<FxParams,RequiredStatic>::from_parts(params,None);required.state_ref_mut().counter=71;
    assert!(required.requires_fallible_preparation());
    let fallback=PreparationConfig::new(AudioConfig::new(48000.,8192),PreparationFrameBound::UnprovenDeviceFallback);
    assert_eq!(required.prepare(&fallback),Err(PreparationError::MissingPolicy));
    assert_eq!(required.state_ref().counter,71,"required refusal cannot run legacy reset over restored DSP state");
}
