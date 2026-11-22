use moose::core::state::{
    DeserializedState, StateLoadError, StateMigrationChain, apply_params, deserialize_state,
    hash_plugin_id, serialize_state_v2,
};
use moose::params::{CatalogChange, CatalogSignal, ParamRange, ParamValueKind};
use moose::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Params)]
struct OscParams {
    #[param(name = "Cutoff", range = "linear(20, 20000)", default = 1000.0)]
    cutoff: FloatParam,
}

#[derive(Params)]
struct SynthParams {
    #[param(
        name = "Master Gain",
        key = "master.gain",
        range = "linear(0, 1)",
        default = 0.5
    )]
    gain: FloatParam,
    #[param(name = "Sample Bank")]
    bank: ValueListParam,
    #[nested]
    osc1: OscParams,
}

#[test]
fn derive_value_list_param_and_nested_string_ids_work_end_to_end() {
    let params = SynthParams::new();
    let woken = Arc::new(AtomicUsize::new(0));
    let woken_cb = Arc::clone(&woken);
    let signal = Arc::new(CatalogSignal::new(move || {
        woken_cb.fetch_add(1, Ordering::Relaxed);
    }));
    params.set_catalog_signal(Arc::downgrade(&signal));

    // Nested and explicit `key = "..."` string IDs compose cleanly in ParamCatalog.
    let catalog = params.param_catalog();
    assert_eq!(catalog.string_id(params.gain.id()), Some("master.gain"));
    assert_eq!(catalog.string_id(params.bank.id()), Some("bank"));
    assert_eq!(
        catalog.string_id(params.osc1.cutoff.id()),
        Some("osc1.cutoff")
    );
    assert_eq!(
        catalog.id_by_string("osc1.cutoff"),
        Some(params.osc1.cutoff.id())
    );
    assert_eq!(
        catalog.by_id(params.bank.id()).unwrap().kind,
        ParamValueKind::ValueList
    );

    // Runtime ValueListParam updates wake the wrapper signal and flow into param_infos + labels.
    assert!(params.bank.set_labels(["Acoustic", "808", "909"]));
    assert_eq!(woken.load(Ordering::Relaxed), 1);
    let changes = params.take_catalog_changes();
    assert!(changes.flags.contains(CatalogChange::STRUCTURE));
    assert!(changes.flags.contains(CatalogChange::INFO));
    assert_eq!(
        params.value_list_labels(params.bank.id()),
        Some(vec!["Acoustic".into(), "808".into(), "909".into()])
    );
    let refreshed = params.param_catalog();
    assert!(matches!(
        refreshed.by_id(params.bank.id()).unwrap().range,
        ParamRange::Enum { count: 3 }
    ));
    params.set_plain(params.bank.id(), 2.0);
    assert_eq!(
        params.format_value(params.bank.id(), 2.0).as_deref(),
        Some("909")
    );
    assert_eq!(params.parse_value(params.bank.id(), "808"), Some(1.0));
}

#[test]
fn state_v2_restores_by_string_id_after_numeric_id_change_and_migrations() {
    #[allow(clippy::unnecessary_wraps)]
    fn v1_to_v2(s: &mut DeserializedState) -> Result<(), StateLoadError> {
        for entry in &mut s.named_params {
            if entry.key == "master.gain" {
                entry.value = (entry.value * 2.0).min(1.0);
            }
        }
        Ok(())
    }

    let params = SynthParams::new();
    // Move gain away from the expected migrated value (0.5) so the assertion
    // below catches a skipped restore as well as an unmigrated value.
    params.set_plain(params.gain.id(), 0.1);
    params.bank.set_labels(["Piano", "Organ", "Strings"]);
    let plugin_hash = hash_plugin_id("com.test.synth");

    // Simulate a saved v2 blob from schema v1 whose numeric IDs don't match
    // the current instance's numeric IDs at all.
    let stale_entries = [
        ("master.gain", 999_001u32, 0.25),
        ("bank", 999_002u32, 2.0),
        ("osc1.cutoff", 999_003u32, 2500.0),
    ];
    let blob = serialize_state_v2(plugin_hash, 1, &stale_entries, b"v1-extra", &[]);
    let mut state = deserialize_state(&blob, plugin_hash).expect("v2 envelope decodes");
    assert_eq!(state.schema_version, 1);

    // Run schema migration 1 -> 2 doubling master.gain.
    let chain = StateMigrationChain::new(2).step(1, v1_to_v2);
    chain.run(&mut state).expect("migration succeeds");
    assert_eq!(state.schema_version, 2);

    apply_params(&params, &state);
    assert!((params.gain.raw_target() - 0.5).abs() < 1e-9);
    assert_eq!(params.bank.index(), 2);
    assert!((params.osc1.cutoff.raw_target() - 2500.0).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Presets go through the same schema-aware writer and migration-aware reader
// as host state: v1 for `state_schema_version() == 0`, v2 otherwise, and every
// load runs the plugin's `StateMigrationChain` + string-ID resolution.
// ---------------------------------------------------------------------------

mod presets {
    use super::SynthParams;
    use moose_core::buffer::AudioBuffer;
    use moose_core::config::AudioConfig;
    use moose_core::events::EventList;
    use moose_core::export::PluginExport;
    use moose_core::info::{Automation, MidiDialect, PluginCategory, PluginInfo};
    use moose_core::meters::MeterStore;
    use moose_core::plugin::PluginRuntime;
    use moose_core::presets::{
        PresetError, PresetMeta, PresetStore, parse_preset_file, write_preset_file,
    };
    use moose_core::process::{ProcessContext, ProcessStatus};
    use moose_core::snapshot::SnapshotSlot;
    use moose_core::state::{
        DeserializedState, STATE_VERSION_V1, STATE_VERSION_V2, StateLoadError, StateMigrationChain,
        apply_params, hash_plugin_id, load_preset, load_preset_file, save_user_preset,
        serialize_preset, serialize_state, serialize_state_v2,
    };
    use moose_params::Params;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn info(clap_id: &'static str) -> PluginInfo {
        PluginInfo {
            name: "Preset fixture",
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
            bundle_id: "preset-fixture",
            vst3_id: clap_id,
            clap_id,
            fourcc: *b"PrFx",
            au_type: *b"aufx",
            au_manufacturer: *b"Moos",
            vst3_subcategory: None,
            preset_user_dir: None,
            preset_extension: "trucepreset",
            vst3_name: None,
            clap_name: None,
            au_name: None,
            au3_name: None,
            mute_preview_output: false,
            automation: Automation::DEFAULT,
            legacy_au_keys: &[],
            legacy_aax_chunks: &[],
        }
    }

    /// One fixture plugin per schema: `$schema == 0` is a legacy plugin.
    macro_rules! fixture {
        ($name:ident, $clap_id:literal, $schema:expr, $chain:expr) => {
            struct $name {
                params: Arc<SynthParams>,
                meters: Arc<MeterStore>,
                snapshot: Arc<SnapshotSlot>,
            }
            impl PluginExport for $name {
                type Params = SynthParams;
                fn create() -> Self {
                    Self {
                        params: Arc::new(SynthParams::new()),
                        meters: MeterStore::new(),
                        snapshot: SnapshotSlot::new(),
                    }
                }
                fn params(&self) -> &SynthParams {
                    &self.params
                }
                fn params_arc(&self) -> Arc<SynthParams> {
                    Arc::clone(&self.params)
                }
                fn meter_store(&self) -> Arc<MeterStore> {
                    Arc::clone(&self.meters)
                }
                fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
                    Arc::clone(&self.snapshot)
                }
            }
            impl PluginRuntime for $name {
                type Sample = f32;
                fn info() -> PluginInfo {
                    info($clap_id)
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
                fn state_schema_version() -> u32 {
                    $schema
                }
                fn state_migrations() -> StateMigrationChain {
                    $chain
                }
            }
        };
    }

    fn gain_id() -> u32 {
        SynthParams::new().gain.id()
    }

    /// Schema 0 -> 1: v1 envelopes stored gain at half scale.
    #[allow(clippy::unnecessary_wraps)]
    fn double_gain(s: &mut DeserializedState) -> Result<(), StateLoadError> {
        let id = gain_id();
        for (pid, value) in &mut s.params {
            if *pid == id {
                *value = (*value * 2.0).min(1.0);
            }
        }
        Ok(())
    }

    /// Schema 1 -> 2: the gain's string ID was renamed.
    #[allow(clippy::unnecessary_wraps)]
    fn rename_gain(s: &mut DeserializedState) -> Result<(), StateLoadError> {
        for entry in &mut s.named_params {
            if entry.key == "gain_old" {
                "master.gain".clone_into(&mut entry.key);
            }
        }
        Ok(())
    }

    fixture!(
        Versioned,
        "org.moose.preset-versioned",
        2,
        StateMigrationChain::new(2)
            .step(0, double_gain)
            .step(1, rename_gain)
    );
    fixture!(
        Legacy,
        "org.moose.preset-legacy",
        0,
        StateMigrationChain::new(0)
    );

    fn temp_store<P: PluginExport>(name: &str) -> (PresetStore, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("moose-preset-schema-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let info = P::info();
        let store = PresetStore::new(info.vendor, info.name, hash_plugin_id(info.clap_id), None)
            .with_user_root(&dir);
        (store, dir)
    }

    fn envelope(path: &Path) -> Vec<u8> {
        parse_preset_file(&std::fs::read(path).unwrap()).unwrap().1
    }

    fn envelope_version(blob: &[u8]) -> u32 {
        u32::from_le_bytes(blob[4..8].try_into().unwrap())
    }

    fn write_raw(dir: &Path, name: &str, blob: &[u8]) -> PathBuf {
        let meta = PresetMeta {
            uuid: format!("00000000-0000-4000-8000-{:012}", name.len()),
            name: name.into(),
            ..PresetMeta::default()
        };
        let path = dir.join(format!("{name}.trucepreset"));
        std::fs::write(&path, write_preset_file(&meta, blob)).unwrap();
        path
    }

    #[test]
    #[cfg_attr(miri, ignore = "real file I/O")]
    fn schema_one_plus_plugin_saves_v2_and_round_trips() {
        let (store, dir) = temp_store::<Versioned>("v2-roundtrip");
        let plugin = Versioned::create();
        plugin.params().gain.set_value(0.3);
        plugin.params().osc1.cutoff.set_value(4000.0);

        let saved = save_user_preset::<Versioned>(
            &store,
            PresetMeta {
                name: "Bright".into(),
                ..PresetMeta::default()
            },
            plugin.params(),
            b"custom",
        )
        .expect("saved");
        let blob = envelope(&saved.path);
        assert_eq!(envelope_version(&blob), STATE_VERSION_V2);
        assert_eq!(
            blob,
            serialize_preset::<Versioned>(plugin.params(), b"custom"),
            "store save and the raw writer agree"
        );

        let fresh = Versioned::create();
        let state = load_preset::<Versioned>(&store, fresh.params(), &saved.uri).expect("loads");
        assert_eq!(state.schema_version, 2);
        assert_eq!(state.named_params, Vec::new(), "resolved to numeric IDs");
        assert_eq!(state.extra.as_deref(), Some(&b"custom"[..]));
        apply_params(fresh.params(), &state);
        assert!((fresh.params().gain.raw_target() - 0.3).abs() < 1e-9);
        assert!((fresh.params().osc1.cutoff.raw_target() - 4000.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg_attr(miri, ignore = "real file I/O")]
    fn older_v1_and_v2_presets_load_through_the_migration_chain() {
        let (store, dir) = temp_store::<Versioned>("migrate");
        let hash = hash_plugin_id(Versioned::info().clap_id);
        let params = SynthParams::new();
        let cutoff = params.osc1.cutoff.id();

        // A schema-0 v1 preset from before the plugin opted into schemas:
        // runs 0 -> 1 (doubles gain) and 1 -> 2.
        let v1 = write_raw(
            &dir,
            "v1",
            &serialize_state(hash, &[gain_id(), cutoff], &[0.2, 900.0], b"", &[]),
        );
        let state = load_preset::<Versioned>(&store, &params, "v1").expect("v1 loads");
        assert_eq!(state.schema_version, 2);
        apply_params(&params, &state);
        assert!((params.gain.raw_target() - 0.4).abs() < 1e-9);
        assert!((params.osc1.cutoff.raw_target() - 900.0).abs() < 1e-9);
        // The AU factory-preset path reads files through the same gate.
        let via_file = load_preset_file::<Versioned>(&v1, hash, &params).expect("file loads");
        assert_eq!(via_file.params, state.params);

        // A schema-1 v2 preset whose numeric IDs went stale and whose gain
        // string ID was later renamed: 1 -> 2 renames, then the string IDs
        // resolve to the current numeric IDs.
        write_raw(
            &dir,
            "v2-schema1",
            &serialize_state_v2(
                hash,
                1,
                &[("gain_old", 999_001, 0.7), ("osc1.cutoff", 999_002, 1500.0)],
                b"",
                &[],
            ),
        );
        let fresh = SynthParams::new();
        let state = load_preset::<Versioned>(&store, &fresh, "v2-schema1").expect("v2 loads");
        assert_eq!(state.schema_version, 2);
        apply_params(&fresh, &state);
        assert!((fresh.gain.raw_target() - 0.7).abs() < 1e-9);
        assert!((fresh.osc1.cutoff.raw_target() - 1500.0).abs() < 1e-9);

        // A preset from a newer build than this plugin is refused, not
        // half-applied.
        write_raw(
            &dir,
            "future",
            &serialize_state_v2(hash, 9, &[("master.gain", 0, 0.9)], b"", &[]),
        );
        assert!(matches!(
            load_preset::<Versioned>(&store, &fresh, "future"),
            Err(PresetError::InvalidState)
        ));
        assert!((fresh.gain.raw_target() - 0.7).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg_attr(miri, ignore = "real file I/O")]
    fn schema_zero_plugin_keeps_writing_v1() {
        let (store, dir) = temp_store::<Legacy>("legacy");
        let plugin = Legacy::create();
        plugin.params().gain.set_value(0.6);
        let saved = save_user_preset::<Legacy>(
            &store,
            PresetMeta {
                name: "Old".into(),
                ..PresetMeta::default()
            },
            plugin.params(),
            b"x",
        )
        .expect("saved");
        let blob = envelope(&saved.path);
        assert_eq!(envelope_version(&blob), STATE_VERSION_V1);
        let (ids, values) = plugin.params().collect_values();
        assert_eq!(
            blob,
            serialize_state(
                hash_plugin_id(Legacy::info().clap_id),
                &ids,
                &values,
                b"x",
                &[]
            ),
            "byte-identical to the pre-v2 writer"
        );
        let fresh = SynthParams::new();
        let state = load_preset::<Legacy>(&store, &fresh, &saved.uri).expect("loads");
        apply_params(&fresh, &state);
        assert!((fresh.gain.raw_target() - 0.6).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
